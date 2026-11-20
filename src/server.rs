use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at, Instant};
use tracing::{debug, warn};

use crate::session::{CompanionSession, DEVICE_MODEL};
use crate::{Error, FrameType, PairingState, RemoteInput, Result, TrackpadMode};

const MAX_FRAME_SIZE: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 32;
const SOURCE_VERSION: &str = "715.2";

pub struct Receiver {
    mdns: ServiceDaemon,
    services: Vec<String>,
    listeners: Vec<JoinHandle<()>>,
    pub ports: [u16; 3],
}

impl Receiver {
    pub async fn start(
        name: String,
        ip: Ipv4Addr,
        pairing: Arc<PairingState>,
        mode: TrackpadMode,
        input: Arc<dyn RemoteInput>,
    ) -> Result<Self> {
        let mrp = bind(ip, 49152).await?;
        let companion = bind(ip, 49153).await?;
        let airplay = bind(ip, 0).await?;
        let ports = [
            mrp.local_addr()?.port(),
            companion.local_addr()?.port(),
            airplay.local_addr()?.port(),
        ];
        let records = discovery_records(&name, ip, &pairing, ports)?;
        let mdns = ServiceDaemon::new().map_err(mdns_error)?;
        let events = mdns.monitor().map_err(mdns_error)?;
        mdns.disable_interface(IfKind::All).map_err(mdns_error)?;
        mdns.enable_interface(IfKind::Addr(ip.into()))
            .map_err(mdns_error)?;
        let mut services = Vec::new();
        for record in records {
            services.push(record.get_fullname().to_owned());
            if let Err(error) = mdns.register(record) {
                let _ = mdns.shutdown();
                return Err(mdns_error(error));
            }
        }

        let listeners = vec![
            tokio::spawn(async move {
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
            }),
            tokio::spawn(discovery_endpoint(mrp, false)),
            tokio::spawn(discovery_endpoint(airplay, true)),
            tokio::spawn(companion_endpoint(
                companion,
                name.into(),
                pairing,
                mode,
                input,
            )),
        ];
        Ok(Self {
            mdns,
            services,
            listeners,
            ports,
        })
    }

    pub async fn stop(self) {
        for listener in &self.listeners {
            listener.abort();
        }
        for listener in self.listeners {
            let _ = listener.await;
        }
        for service in self.services {
            let _ = self.mdns.unregister(&service);
        }
        if let Ok(done) = self.mdns.shutdown() {
            let _ = timeout(Duration::from_secs(2), done.recv_async()).await;
        }
    }
}

pub fn detect_local_ip() -> Result<Ipv4Addr> {
    // UDP connect selects an interface without sending a packet.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    match socket.local_addr()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Ok(ip),
        _ => Err(Error::Protocol(
            "no LAN IPv4 address found. Pass --ip to select the interface.".into(),
        )),
    }
}

async fn bind(ip: Ipv4Addr, preferred: u16) -> io::Result<TcpListener> {
    match TcpListener::bind((ip, preferred)).await {
        Err(err) if err.kind() == io::ErrorKind::AddrInUse && preferred != 0 => {
            TcpListener::bind((ip, 0)).await
        }
        result => result,
    }
}

fn discovery_records(
    name: &str,
    ip: Ipv4Addr,
    identity: &PairingState,
    ports: [u16; 3],
) -> Result<Vec<ServiceInfo>> {
    let mac = identity.mac.map(|byte| format!("{byte:02X}")).join(":");
    let unique = identity.identifier.replace('-', "");
    let mac_id = mac.replace(':', "");
    let host = format!("myremote-{}.local.", mac_id.to_ascii_lowercase());
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
        ("_mediaremotetv", &mrp[..]),
        ("_companion-link", &companion[..]),
        ("_airplay", &airplay[..]),
    ]
    .into_iter()
    .zip(ports)
    .map(|((service, txt), port)| {
        ServiceInfo::new(
            &format!("{service}._tcp.local."),
            name,
            &host,
            std::net::IpAddr::V4(ip),
            port,
            txt,
        )
        .map_err(mdns_error)
    })
    .collect()
}

fn mdns_error(error: mdns_sd::Error) -> Error {
    Error::Mdns(error.to_string())
}

async fn discovery_endpoint(listener: TcpListener, airplay: bool) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            Ok((mut stream, _)) = listener.accept(), if clients.len() < MAX_CONNECTIONS => {
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
                    if let Err(error) = run_session(stream, input, pairing, name, mode).await {
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
) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, writer) = stream.into_split();
    let mut session = CompanionSession::new(writer, input, pairing, name, mode);
    let auth_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let header_deadline = if session.is_verified() {
            Instant::now() + Duration::from_secs(30 * 60)
        } else {
            auth_deadline
        };
        let mut header = [0; 4];
        timeout_at(header_deadline, reader.read_exact(&mut header))
            .await
            .map_err(io::Error::other)??;
        let frame = FrameType::from_u8(header[0])
            .ok_or_else(|| Error::Protocol("unknown frame type".into()))?;
        let size = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if size > MAX_FRAME_SIZE {
            return Err(Error::Protocol("Companion frame too large".into()));
        }
        let mut data = vec![0; size];
        let frame_deadline = header_deadline.min(Instant::now() + Duration::from_secs(5));
        timeout_at(frame_deadline, async {
            reader.read_exact(&mut data).await?;
            session.handle_frame(frame, &data, &header).await
        })
        .await
        .map_err(io::Error::other)??;
    }
}
