use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use myremote::{PairingState, Receiver, RemoteInput, TrackpadMode};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};

use crate::{default_state_dir, tailnet, ActiveReceiver, Bridge, Options};

const REFRESH: Duration = Duration::from_secs(30);
const CONCURRENT_PROBES: usize = 16;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    share: bool,
    bridge: bool,
}

pub struct MenuNetwork {
    path: Option<PathBuf>,
    settings: Preferences,
    share: Option<ShareTask>,
    bridge: Option<Task>,
    error: Option<&'static str>,
}

struct Task {
    worker: JoinHandle<()>,
    status: watch::Receiver<String>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

struct ShareTask {
    task: Task,
    stop: oneshot::Sender<()>,
}

struct Target {
    name: String,
    pairing: Arc<PairingState>,
    mode: TrackpadMode,
    input: Arc<dyn RemoteInput>,
}

impl MenuNetwork {
    pub fn new(options: &Options) -> Self {
        let path = options
            .state_dir
            .clone()
            .or_else(default_state_dir)
            .map(|directory| directory.join("tailscale.json"));
        let settings = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            settings,
            share: None,
            bridge: None,
            error: None,
        }
    }

    pub fn sharing(&self) -> bool {
        self.settings.share
    }

    pub fn bridging(&self) -> bool {
        self.settings.bridge
    }

    pub fn status(&self) -> String {
        if let Some(error) = self.error {
            return error.into();
        }
        let share = self.share.as_ref().map(|share| share.task.status.borrow());
        let bridge = self.bridge.as_ref().map(|bridge| bridge.status.borrow());
        match (share.as_deref(), bridge.as_deref()) {
            (Some(a), Some(b)) if a != b => format!("{a} · {b}"),
            (Some(status), _) | (_, Some(status)) => status.to_owned(),
            _ if self.sharing() => "Start Receiver to share over Tailscale".into(),
            _ => "Tailscale is off".into(),
        }
    }

    pub fn toggle_share(
        &mut self,
        runtime: &Runtime,
        options: &Options,
        active: Option<&ActiveReceiver>,
    ) {
        if self.save(Preferences {
            share: !self.sharing(),
            ..self.settings
        }) {
            self.sync_receiver(runtime, options, active);
        }
    }

    pub fn toggle_bridge(&mut self, runtime: &Runtime, options: &Options) {
        if self.save(Preferences {
            bridge: !self.bridging(),
            ..self.settings
        }) {
            self.start_bridge(runtime, options);
        }
    }

    fn save(&mut self, settings: Preferences) -> bool {
        if self
            .path
            .as_ref()
            .is_none_or(|path| save_preferences(path, settings).is_err())
        {
            self.error = Some("Could not save Tailscale settings");
            return false;
        }
        self.error = None;
        self.settings = settings;
        true
    }

    pub fn sync_receiver(
        &mut self,
        runtime: &Runtime,
        options: &Options,
        active: Option<&ActiveReceiver>,
    ) {
        if let Some(mut share) = self.share.take() {
            let _ = share.stop.send(());
            let _ = runtime.block_on(&mut share.task.worker);
        }
        if !self.sharing() || options.tailnet_host.is_some() {
            return;
        }
        let Some(active) = active else { return };
        let target = Target {
            name: options.name.clone(),
            pairing: active.state.clone(),
            mode: options.mode,
            input: active.input.clone(),
        };
        let (updates, status) = watch::channel("Connecting to Tailscale…".to_owned());
        let (stop, stopped) = oneshot::channel();
        let worker = runtime.spawn(share_loop(target, updates, stopped));
        self.share = Some(ShareTask {
            task: Task { worker, status },
            stop,
        });
    }

    pub fn start_bridge(&mut self, runtime: &Runtime, options: &Options) {
        self.bridge = self.bridging().then(|| {
            let (updates, status) = watch::channel("Finding myRemote computers…".to_owned());
            let worker = runtime.spawn(bridge_loop(options.ip, updates));
            Task { worker, status }
        });
    }
}

fn save_preferences(path: &Path, settings: Preferences) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing settings directory"))?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, &settings)?;
    temporary.persist(path)?;
    Ok(())
}

async fn share_loop(
    target: Target,
    updates: watch::Sender<String>,
    mut stopped: oneshot::Receiver<()>,
) {
    let mut host = None;
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            _ = update_share(&target, &updates, &mut host) => {}
        }
        tokio::select! {
            _ = &mut stopped => break,
            _ = sleep(REFRESH) => {}
        }
    }
    if let Some((_, service, receiver)) = host {
        drop(service);
        receiver.stop().await;
    }
}

async fn update_share(
    target: &Target,
    updates: &watch::Sender<String>,
    host: &mut Option<(Ipv4Addr, tailnet::Host, Receiver)>,
) {
    let network = tailscale_status().await;
    if host.as_ref().map(|host| host.0) == network.as_ref().ok().map(|status| status.ip) {
        if let Err(message) = network {
            updates.send_replace(message.into());
        }
        return;
    }
    if let Some((_, service, receiver)) = host.take() {
        drop(service);
        receiver.stop().await;
    }
    let status = match network {
        Ok(status) => status,
        Err(message) => {
            updates.send_replace(message.into());
            return;
        }
    };
    let message = match start_share(target, status.ip).await {
        Ok((service, receiver)) => {
            *host = Some((status.ip, service, receiver));
            "Sharing this computer over Tailscale"
        }
        Err(error) => {
            tracing::debug!("Tailscale sharing could not start: {error}");
            "Tailscale sharing could not start"
        }
    };
    updates.send_replace(message.into());
}

async fn start_share(target: &Target, ip: Ipv4Addr) -> tailnet::Result<(tailnet::Host, Receiver)> {
    let (receiver, records) = Receiver::listen_tailnet(
        target.name.clone(),
        ip,
        target.pairing.clone(),
        target.mode,
        target.input.clone(),
    )
    .await?;
    let catalog = tailnet::Catalog {
        name: target.name.clone(),
        records,
    };
    let service = tailnet::Host::start(ip, catalog).await?;
    Ok((service, receiver))
}

struct BridgedPeer {
    catalog: tailnet::Catalog,
    bridge: Bridge,
    missed: bool,
}

async fn bridge_loop(lan_ip: Option<Ipv4Addr>, updates: watch::Sender<String>) {
    let mut published = HashMap::new();
    loop {
        let network = tailscale_status().await;
        let message = match network {
            Ok(status) => refresh_bridges(lan_ip, &status.peers, &mut published).await,
            Err(message) => {
                published.clear();
                message.into()
            }
        };
        updates.send_replace(message);
        sleep(REFRESH).await;
    }
}

async fn refresh_bridges(
    lan_ip: Option<Ipv4Addr>,
    peers: &[Ipv4Addr],
    published: &mut HashMap<Ipv4Addr, BridgedPeer>,
) -> String {
    #[cfg(not(target_os = "macos"))]
    let lan_ip = lan_ip.or_else(|| myremote::detect_local_ip().ok());
    #[cfg(not(target_os = "macos"))]
    if let Some(ip) = lan_ip {
        published.retain(|_, peer| peer.bridge.lan_ip == ip);
    } else {
        published.clear();
        return "Connect this computer to the iPhone’s local network".into();
    }
    let mut found = false;
    let mut failed = false;
    for batch in peers.chunks(CONCURRENT_PROBES) {
        let mut probes = JoinSet::new();
        for &ip in batch {
            probes.spawn(async move { (ip, tailnet::fetch(ip).await.ok()) });
        }
        while let Some(Ok((ip, catalog))) = probes.join_next().await {
            let Some(catalog) = catalog else {
                if let Some(peer) = published.get_mut(&ip) {
                    if std::mem::replace(&mut peer.missed, true) {
                        published.remove(&ip);
                    }
                }
                continue;
            };
            found = true;
            if let Some(peer) = published.get_mut(&ip) {
                if peer.catalog == catalog {
                    peer.missed = false;
                    continue;
                }
                published.remove(&ip);
            }
            match Bridge::start(lan_ip, ip, catalog.clone()).await {
                Ok(bridge) => {
                    published.insert(
                        ip,
                        BridgedPeer {
                            catalog,
                            bridge,
                            missed: false,
                        },
                    );
                }
                Err(error) => {
                    failed = true;
                    tracing::debug!("Could not start a local Tailscale bridge: {error}");
                }
            }
        }
    }
    published.retain(|ip, _| peers.binary_search(ip).is_ok());
    #[cfg(target_os = "macos")]
    if published.values().any(|peer| {
        peer.bridge.bonjour.status() == crate::mac_bonjour::Status::LocalNetworkPermission
    }) {
        return "Tailscale bridge needs Local Network access".into();
    }
    #[cfg(target_os = "macos")]
    published.retain(|_, peer| peer.bridge.bonjour.status() != crate::mac_bonjour::Status::Failed);
    match published.len() {
        1 => "Bridging 1 computer to this network".into(),
        count if count > 1 => format!("Bridging {count} computers to this network"),
        _ if found || failed => "Could not advertise a Tailscale receiver".into(),
        _ => "No myRemote computers sharing on Tailscale".into(),
    }
}

struct Network {
    ip: Ipv4Addr,
    peers: Vec<Ipv4Addr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<IpAddr>>,
    peer: Option<HashMap<String, Peer>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Peer {
    online: bool,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<IpAddr>>,
}

fn tailnet_ipv4(addresses: Option<Vec<IpAddr>>) -> Option<Ipv4Addr> {
    addresses?.into_iter().find_map(|address| match address {
        IpAddr::V4(ip) if tailnet::is_tailnet(ip) => Some(ip),
        _ => None,
    })
}

async fn tailscale_status() -> Result<Network, &'static str> {
    for path in tailscale_paths() {
        let mut command = Command::new(path);
        command
            .args(["status", "--json"])
            .env("TAILSCALE_BE_CLI", "1")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        match timeout(Duration::from_secs(4), command.output()).await {
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => continue,
            Ok(Ok(output)) if output.status.success() => return parse_status(&output.stdout),
            _ => return Err("Tailscale is unavailable · open Tailscale and connect"),
        }
    }
    Err("Install and connect Tailscale on this computer")
}

fn parse_status(data: &[u8]) -> Result<Network, &'static str> {
    let status: Status =
        serde_json::from_slice(data).map_err(|_| "Could not read Tailscale status")?;
    if status.backend_state != "Running" {
        return Err("Connect Tailscale to continue");
    }
    let ip = tailnet_ipv4(status.tailscale_ips).ok_or("Tailscale has no IPv4 address")?;
    let peers: BTreeSet<_> = status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter(|peer| peer.online)
        .filter_map(|peer| tailnet_ipv4(peer.tailscale_ips))
        .filter(|peer| *peer != ip)
        .collect();
    Ok(Network {
        ip,
        peers: peers.into_iter().collect(),
    })
}

fn tailscale_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let bundle = "Applications/Tailscale.app/Contents/MacOS/Tailscale";
        paths.push(Path::new("/").join(bundle));
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(home).join(bundle));
        }
        paths
            .extend(["/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"].map(PathBuf::from));
    }
    #[cfg(windows)]
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(programs).join("Tailscale/tailscale.exe"));
    }
    #[cfg(target_os = "linux")]
    paths.extend(["/usr/bin/tailscale", "/usr/sbin/tailscale"].map(PathBuf::from));
    paths.push(PathBuf::from("tailscale"));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_online_tailnet_ipv4_peers_are_probed() {
        let status = parse_status(
            br#"{
            "BackendState": "Running",
            "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.64.0.1"],
            "Peer": {
                "remote": {"Online": true, "TailscaleIPs": ["100.64.0.2"]},
                "offline": {"Online": false, "TailscaleIPs": ["100.64.0.3"]},
                "lan": {"Online": true, "TailscaleIPs": ["192.168.1.3"]},
                "self": {"Online": true, "TailscaleIPs": ["100.64.0.1"]}
            }
        }"#,
        )
        .unwrap();
        assert_eq!(status.ip, Ipv4Addr::new(100, 64, 0, 1));
        assert_eq!(status.peers, [Ipv4Addr::new(100, 64, 0, 2)]);
    }
}
