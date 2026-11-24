use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{tcp::OwnedReadHalf, TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Instant};
use tracing::{debug, warn};

use crate::pairing::PAIRING_WINDOW;
use crate::session::{CompanionSession, DEVICE_MODEL, SOURCE_VERSION};
use crate::{Error, PairingState, RemoteInput, Result, TrackpadMode};

const MAX_FRAME_SIZE: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 32;
pub const TAILNET_PORTS: [u16; 3] = [49152, 49153, 49154];

pub struct Receiver {
    mdns: Option<(ServiceDaemon, Vec<String>)>,
    listeners: Vec<JoinHandle<()>>,
    pub ports: [u16; 3],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveryRecord {
    pub service_type: String,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

impl Receiver {
    pub async fn start(
        name: String,
        ip: Ipv4Addr,
        pairing: Arc<PairingState>,
        mode: TrackpadMode,
        input: Arc<dyn RemoteInput>,
    ) -> Result<Self> {
        let (mut receiver, records) =
            Self::listen(name.clone(), ip.into(), pairing.clone(), mode, input).await?;
        let device_id = hex::encode(pairing.mac);
        let host = format!("myremote-{device_id}.local.");
        let result = receiver.advertise(&name, &host, ip, &records);
        if let Err(error) = result {
            receiver.stop().await;
            return Err(error);
        }
        Ok(receiver)
    }

    pub fn advertise(
        &mut self,
        name: &str,
        host: &str,
        ip: Ipv4Addr,
        records: &[DiscoveryRecord],
    ) -> Result<()> {
        let mdns = ServiceDaemon::new().map_err(mdns_error)?;
        // Keep the handle available for cleanup even if registration is interrupted.
        self.mdns = Some((mdns.clone(), Vec::new()));
        let events = mdns.monitor().map_err(mdns_error)?;
        mdns.disable_interface(IfKind::All).map_err(mdns_error)?;
        mdns.enable_interface(IfKind::Addr(ip.into()))
            .map_err(mdns_error)?;
        let (_, services) = self.mdns.as_mut().unwrap();
        for record in records {
            let txt: Vec<_> = record
                .txt
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let info = ServiceInfo::new(
                &format!("{}.local.", record.service_type),
                name,
                host,
                IpAddr::V4(ip),
                record.port,
                &txt[..],
            )
            .map_err(mdns_error)?;
            services.push(info.get_fullname().to_owned());
            mdns.register(info).map_err(mdns_error)?;
        }

        self.listeners.push(tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                match event {
                    DaemonEvent::Error(error) => warn!("Bonjour: {error}"),
                    DaemonEvent::Announce(service, interface) => {
                        debug!("Bonjour announced {service} on {interface}");
                    }
                    DaemonEvent::NameChange(change) => {
                        debug!("Bonjour renamed a record: {change:?}")
                    }
                    _ => {}
                }
            }
        }));
        Ok(())
    }

    /// Forward the advertised endpoints without decoding or modifying Companion traffic.
    pub async fn relay(
        ip: IpAddr,
        remote: Ipv4Addr,
        records: &mut [DiscoveryRecord],
    ) -> Result<Self> {
        let mut sockets = Vec::with_capacity(records.len());
        for record in records {
            let destination = SocketAddr::new(remote.into(), record.port);
            let listener = bind(ip, 0).await?;
            record.port = listener.local_addr()?.port();
            sockets.push((listener, destination));
        }
        Ok(Self {
            mdns: None,
            listeners: sockets
                .into_iter()
                .map(|(listener, target)| tokio::spawn(relay_endpoint(listener, target)))
                .collect(),
            ports: [0; 3],
        })
    }

    /// Start the TCP endpoints so the caller can publish them through platform Bonjour.
    pub async fn listen(
        name: String,
        ip: IpAddr,
        pairing: Arc<PairingState>,
        mode: TrackpadMode,
        input: Arc<dyn RemoteInput>,
    ) -> Result<(Self, Vec<DiscoveryRecord>)> {
        Self::listen_endpoints(name, ip, pairing, mode, input, false).await
    }

    pub async fn listen_tailnet(
        name: String,
        ip: Ipv4Addr,
        pairing: Arc<PairingState>,
        mode: TrackpadMode,
        input: Arc<dyn RemoteInput>,
    ) -> Result<(Self, Vec<DiscoveryRecord>)> {
        Self::listen_endpoints(name, ip.into(), pairing, mode, input, true).await
    }

    async fn listen_endpoints(
        name: String,
        ip: IpAddr,
        pairing: Arc<PairingState>,
        mode: TrackpadMode,
        input: Arc<dyn RemoteInput>,
        fixed_ports: bool,
    ) -> Result<(Self, Vec<DiscoveryRecord>)> {
        let (mrp, companion, airplay) = if fixed_ports {
            (
                TcpListener::bind((ip, TAILNET_PORTS[0])).await?,
                TcpListener::bind((ip, TAILNET_PORTS[1])).await?,
                TcpListener::bind((ip, TAILNET_PORTS[2])).await?,
            )
        } else {
            // Keep the dedicated Tailscale ports available when listening on all interfaces.
            (
                bind(ip, 49052).await?,
                bind(ip, 49053).await?,
                bind(ip, 49054).await?,
            )
        };
        let ports = [
            mrp.local_addr()?.port(),
            companion.local_addr()?.port(),
            airplay.local_addr()?.port(),
        ];
        let records = discovery_records(&name, &pairing, ports);
        let listeners = vec![
            tokio::spawn(discovery_endpoint(mrp, false, pairing.clone())),
            tokio::spawn(discovery_endpoint(airplay, true, pairing.clone())),
            tokio::spawn(companion_endpoint(
                companion,
                (!fixed_ports).then_some(ports[1]),
                name.into(),
                pairing,
                mode,
                input,
            )),
        ];
        Ok((
            Self {
                mdns: None,
                listeners,
                ports,
            },
            records,
        ))
    }

    pub async fn stop(mut self) {
        for listener in &self.listeners {
            listener.abort();
        }
        for listener in self.listeners.drain(..) {
            let _ = listener.await;
        }
        if let Some((mdns, services)) = self.mdns.take() {
            unregister(&mdns, services);
            if let Ok(done) = mdns.shutdown() {
                let _ = timeout(Duration::from_secs(2), done.recv_async()).await;
            }
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.abort();
        }
        if let Some((mdns, services)) = self.mdns.take() {
            unregister(&mdns, services);
            let _ = mdns.shutdown();
        }
    }
}

fn unregister(mdns: &ServiceDaemon, services: Vec<String>) {
    for service in services {
        let _ = mdns.unregister(&service);
    }
}

pub fn detect_local_ip() -> Result<Ipv4Addr> {
    // UDP connect selects an interface without sending a packet.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    if socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok() {
        if let IpAddr::V4(ip) = socket.local_addr()?.ip() {
            if is_lan_candidate(ip) {
                return Ok(ip);
            }
        }
    }
    if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(ip) if is_lan_candidate(ip) => Some(ip),
            _ => None,
        })
        .min_by_key(|ip| (!ip.is_private(), ip.is_link_local()))
        .ok_or_else(|| Error::Protocol("No active local-network IPv4 address found.".into()))
}

fn is_lan_candidate(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    !ip.is_loopback() && !ip.is_unspecified() && (first != 100 || !(64..128).contains(&second))
}

async fn bind(ip: IpAddr, preferred: u16) -> io::Result<TcpListener> {
    let bind_port = |port| async move {
        if ip.is_ipv6() && ip.is_unspecified() {
            let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_only_v6(false)?;
            socket.set_nonblocking(true)?;
            socket.bind(&SocketAddr::new(ip, port).into())?;
            socket.listen(128)?;
            TcpListener::from_std(socket.into())
        } else {
            TcpListener::bind((ip, port)).await
        }
    };
    match bind_port(preferred).await {
        Err(err) if err.kind() == io::ErrorKind::AddrInUse && preferred != 0 => bind_port(0).await,
        result => result,
    }
}

fn discovery_records(name: &str, identity: &PairingState, ports: [u16; 3]) -> Vec<DiscoveryRecord> {
    let mac = identity.mac.map(|byte| format!("{byte:02X}")).join(":");
    let unique = identity.identifier.replace('-', "");
    let mac_id = mac.replace(':', "");
    let alias = |label: &str, bytes| {
        let hash = Sha256::new()
            .chain_update(label)
            .chain_update(identity.identifier.as_bytes())
            .finalize();
        hex::encode_upper(&hash[..bytes])
    };
    let airplay_id = alias("airplay-identity:", 8);
    let rp_hn = alias("rpHN:", 6);
    let rp_ad = alias("rpAD:", 6);
    let rp_hi = alias("rpHI:", 6);
    let airplay_key = alias("pk:", 16).to_ascii_lowercase();
    let mrp = [
        ("Name", name),
        ("UniqueIdentifier", unique.as_str()),
        ("SystemBuildVersion", "22K160"),
        ("LocalAirPlayReceiverPairingIdentity", airplay_id.as_str()),
        ("ModelName", "Apple TV"),
        ("AllowPairing", "YES"),
    ];
    let companion = [
        ("rpMac", "1"),
        ("rpHA", mac_id.as_str()),
        ("rpHN", rp_hn.as_str()),
        ("rpVr", SOURCE_VERSION),
        ("rpMd", DEVICE_MODEL),
        ("rpFl", "0x36782"),
        ("rpAD", rp_ad.as_str()),
        ("rpHI", rp_hi.as_str()),
        ("rpBA", mac.as_str()),
    ];
    let airplay = [
        ("deviceid", mac.as_str()),
        ("features", "0x5A7FFFF7,0x1E"),
        ("flags", "0x44"),
        ("model", DEVICE_MODEL),
        ("srcvers", SOURCE_VERSION),
        ("vv", "2"),
        ("pi", identity.identifier.as_str()),
        ("pk", airplay_key.as_str()),
        ("name", name),
    ];
    [
        ("_mediaremotetv._tcp", &mrp[..]),
        ("_companion-link._tcp", &companion[..]),
        ("_airplay._tcp", &airplay[..]),
    ]
    .into_iter()
    .zip(ports)
    .map(|((service_type, txt), port)| DiscoveryRecord {
        service_type: service_type.to_owned(),
        port,
        txt: txt
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    })
    .collect()
}

fn mdns_error(error: mdns_sd::Error) -> Error {
    Error::Mdns(error.to_string())
}

async fn relay_endpoint(listener: TcpListener, destination: SocketAddr) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            Ok((mut local, _)) = listener.accept(), if clients.len() < MAX_CONNECTIONS => {
                clients.spawn(async move {
                    if let Ok(Ok(mut remote)) = timeout(Duration::from_secs(5), TcpStream::connect(destination)).await {
                        let _ = local.set_nodelay(true);
                        let _ = remote.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                    }
                });
            }
        }
    }
}

async fn discovery_endpoint(listener: TcpListener, airplay: bool, pairing: Arc<PairingState>) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            Ok((mut stream, _)) = listener.accept(), if clients.len() < MAX_CONNECTIONS => {
                let counter = if airplay {
                    &pairing.diagnostics.airplay_probes
                } else {
                    &pairing.diagnostics.media_probes
                };
                counter.fetch_add(1, Ordering::Relaxed);
                clients.spawn(async move {
                    if !airplay {
                        // Keep the discovery connection open while it is active.
                        // Desktop input only comes through the Companion listener.
                        let mut buffer = [0; 1024];
                        while let Ok(Ok(1..)) = timeout(Duration::from_secs(60), stream.read(&mut buffer)).await {}
                        return;
                    }
                    let _ = timeout(Duration::from_secs(3), async {
                        let mut buf = [0; 1024];
                        let count = stream.read(&mut buf).await?;
                        if count > 0 {
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK").await?;
                        }
                        Ok::<_, io::Error>(())
                    }).await;
                });
            }
        }
    }
}

async fn companion_endpoint(
    listener: TcpListener,
    advertised_port: Option<u16>,
    name: Arc<str>,
    pairing: Arc<PairingState>,
    mode: TrackpadMode,
    input: Arc<dyn RemoteInput>,
) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            Ok((stream, peer)) = listener.accept(), if clients.len() < MAX_CONNECTIONS => {
                let (name, pairing, input) = (name.clone(), pairing.clone(), input.clone());
                clients.spawn(async move {
                    if let Err(error) = run_session(stream, input, pairing, name, mode, advertised_port).await {
                        debug!("Companion connection from {peer} ended: {error}");
                    }
                });
            }
        }
    }
}

async fn run_session(
    stream: TcpStream,
    input: Arc<dyn RemoteInput>,
    pairing: Arc<PairingState>,
    name: Arc<str>,
    mode: TrackpadMode,
    advertised_port: Option<u16>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    // Keep verified sockets usable across idle periods while letting the OS
    // reclaim dead connections when an iPhone changes networks.
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(3));
    let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::with_capacity(16 * 1024, reader);
    let (mut session, replaced) =
        CompanionSession::new(writer, input, pairing, name, mode, advertised_port);
    let result = tokio::select! {
        biased;
        _ = replaced => Ok(()),
        result = read_session(&mut reader, &mut session) => result,
    };
    if let Err(error) = &result {
        session.report_disconnect(error);
    }
    result
}

async fn read_session(
    reader: &mut BufReader<OwnedReadHalf>,
    session: &mut CompanionSession,
) -> Result<()> {
    let auth_deadline = Instant::now() + PAIRING_WINDOW + Duration::from_secs(15);
    let mut focus_poll = tokio::time::interval(Duration::from_millis(250));
    focus_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keyboard_probes = JoinSet::new();
    let mut media_poll = tokio::time::interval(Duration::from_secs(1));
    media_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut media_probes = JoinSet::new();
    let mut data = Vec::new();
    loop {
        let header_deadline = session
            .idle_limit()
            .map_or(auth_deadline, |limit| Instant::now() + limit);
        let mut header = [0; 4];
        {
            // Keep the same read alive across focus notifications, including
            // when only part of the header has arrived.
            let read = timeout_at(header_deadline, reader.read_exact(&mut header));
            tokio::pin!(read);
            loop {
                let touch_deadline = session.touch_deadline();
                let seek_deadline = session.seek_deadline();
                tokio::select! {
                    biased;
                    // A buffered move or lift takes precedence over a local hold timer.
                    result = &mut read => {
                        result.map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
                        break;
                    }
                    _ = tokio::time::sleep_until(touch_deadline.unwrap_or(header_deadline)), if touch_deadline.is_some() => {
                        session.touch_timer();
                    }
                    _ = tokio::time::sleep_until(seek_deadline.unwrap_or(header_deadline)), if seek_deadline.is_some() => {
                        session.repeat_seek();
                    }
                    _ = focus_poll.tick(), if session.watches_keyboard_focus() && keyboard_probes.is_empty() => {
                        session.queue_keyboard_probe(&mut keyboard_probes);
                    }
                    Some(result) = keyboard_probes.join_next(), if !keyboard_probes.is_empty() => {
                        timeout(Duration::from_secs(5), session.publish_keyboard_focus(result.ok().flatten()))
                            .await
                            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
                    }
                    _ = media_poll.tick(), if session.watches_playback() && media_probes.is_empty() => {
                        // The browser's accessibility tree may be slow to answer.
                        session.queue_playback_probe(&mut media_probes);
                    }
                    Some(result) = media_probes.join_next(), if !media_probes.is_empty() => {
                        timeout(Duration::from_secs(5), session.update_playback(result.ok().flatten()))
                            .await
                            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
                    }
                }
            }
        }
        let size = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if size > MAX_FRAME_SIZE {
            return Err(Error::Protocol("Companion frame too large".into()));
        }
        data.resize(size, 0);
        let frame_deadline = header_deadline.min(Instant::now() + Duration::from_secs(5));
        timeout_at(frame_deadline, async {
            reader.read_exact(&mut data).await?;
            session.handle_frame(header[0], &data, &header).await
        })
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opack::{self, Value};
    use crate::tlv::{self, tags};
    use crate::FrameType;

    struct NoInput;

    impl RemoteInput for NoInput {
        fn button(&self, _: &str) {
            panic!("input must require verification");
        }

        fn move_pointer(&self, _: f64, _: f64) {
            panic!("input must require verification");
        }

        fn click(&self) {
            panic!("input must require verification");
        }
    }

    #[tokio::test]
    async fn first_pairing_connection_shows_a_local_code_only_with_the_menu_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(PairingState::open(directory.path()).unwrap());
        let (receiver, _) = Receiver::listen(
            "myRemote test".to_owned(),
            Ipv4Addr::LOCALHOST.into(),
            state.clone(),
            TrackpadMode::Mouse,
            Arc::new(NoInput),
        )
        .await
        .unwrap();

        let request = opack::pack(&Value::dict(vec![Value::kv(
            "_pd",
            Value::Bytes(tlv::write(&[(tags::SEQ_NO, &[1])])),
        )]));
        let mut packet = vec![FrameType::PsStart as u8, 0, 0, request.len() as u8];
        packet.extend(request);

        for menu_enabled in [false, true] {
            if menu_enabled {
                state.enable_pairing_on_request();
            }
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, receiver.ports[1]))
                .await
                .unwrap();
            stream.write_all(&packet).await.unwrap();
            let mut header = [0; 4];
            timeout(Duration::from_secs(3), stream.read_exact(&mut header))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(header[0], FrameType::PsNext as u8);
            let mut bytes =
                vec![0; u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let (reply, _) = opack::unpack(&bytes).unwrap();
            let reply = tlv::read(reply.get("_pd").and_then(Value::as_bytes).unwrap());
            assert_eq!(reply[&tags::SEQ_NO], [2]);
            if menu_enabled {
                assert!(reply.contains_key(&tags::SALT));
                assert!(reply.contains_key(&tags::PUBLIC_KEY));
                assert!(!reply.contains_key(&tags::ERROR));
                let pin = state.take_requested_code().unwrap();
                assert!((1000..=9999).contains(&pin));
                assert_eq!(state.take_requested_code(), None);
                assert_eq!(state.enable_pairing(), pin);
            } else {
                assert_eq!(reply[&tags::ERROR], [6]);
                assert_eq!(state.take_requested_code(), None);
            }
        }

        receiver.stop().await;
    }
}
