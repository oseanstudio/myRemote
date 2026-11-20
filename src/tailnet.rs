use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use myremote::DiscoveryRecord;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

const PORT: u16 = 49151;
const LIMIT: usize = 8192;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize)]
pub struct Catalog {
    pub name: String,
    pub records: Vec<DiscoveryRecord>,
}

pub struct Host(JoinHandle<()>);

impl Host {
    pub async fn start(ip: Ipv4Addr, catalog: Catalog) -> Result<Self> {
        let data = Arc::new(serde_json::to_vec(&catalog)?);
        let listener = TcpListener::bind((ip, PORT)).await?;
        Ok(Self(tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = clients.join_next(), if !clients.is_empty() => {}
                    Ok((mut stream, peer)) = listener.accept(), if clients.len() < 16 => {
                        if !matches!(peer.ip(), std::net::IpAddr::V4(ip) if is_tailnet(ip)) {
                            continue;
                        }
                        let data = Arc::clone(&data);
                        clients.spawn(async move {
                            let _ = timeout(Duration::from_secs(3), stream.write_all(&data)).await;
                        });
                    }
                }
            }
        })))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn is_tailnet(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    first == 100 && (64..128).contains(&second)
}

pub async fn fetch(ip: Ipv4Addr) -> Result<Catalog> {
    let data = timeout(Duration::from_secs(5), async {
        let stream = TcpStream::connect((ip, PORT)).await?;
        let mut data = Vec::new();
        stream
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut data)
            .await?;
        Ok::<_, std::io::Error>(data)
    })
    .await??;
    if data.len() > LIMIT {
        return Err("The Tailscale discovery response is too large.".into());
    }
    let catalog: Catalog = serde_json::from_slice(&data)
        .map_err(|_| "The remote host did not return a myRemote discovery response.")?;
    validate(&catalog)?;
    Ok(catalog)
}

fn validate(catalog: &Catalog) -> Result<()> {
    if catalog.name.trim().is_empty()
        || catalog.name.len() > 63
        || catalog.name.chars().any(char::is_control)
    {
        return Err("The remote host sent an invalid receiver name.".into());
    }
    let expected = [
        "_airplay._tcp",
        "_companion-link._tcp",
        "_mediaremotetv._tcp",
    ];
    let mut types: Vec<_> = catalog
        .records
        .iter()
        .map(|record| record.service_type.as_str())
        .collect();
    types.sort_unstable();
    if types != expected
        || catalog.records.iter().any(|record| {
            record.port == 0
                || record.port == PORT
                || record.txt.len() > 16
                || record.txt.iter().any(|(key, value)| {
                    key.is_empty()
                        || !key.bytes().all(|byte| byte.is_ascii_alphanumeric())
                        || key.len() + value.len() + 1 > 255
                        || value.chars().any(char::is_control)
                })
        })
    {
        return Err("The remote host sent invalid discovery records.".into());
    }
    Ok(())
}
