//! Opt-in peer-to-peer transport using macOS Network.framework.
use std::ffi::{c_char, c_void, CString};
use std::io;
use std::ptr::{null, NonNull};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use block2::{Block, RcBlock};
use dispatch2::{DispatchData, DispatchQueue, DispatchRetained};
use myremote::{DiscoveryRecord, PeerTarget};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

type Raw = *mut c_void;
type Configure = Block<dyn Fn(Raw)>;
type ListenerState = Block<dyn Fn(u32, Raw)>;
type NewConnection = Block<dyn Fn(Raw)>;
type Receive = Block<dyn Fn(Raw, Raw, u8, Raw)>;
type SendCompletion = Block<dyn Fn(Raw)>;

const LISTENER_READY: u32 = 2;
const LISTENER_FAILED: u32 = 3;
const LISTENER_CANCELLED: u32 = 4;
const MAX_CONNECTIONS: usize = 32;
const BUFFER: usize = 16 * 1024;

pub fn nearby_name(name: &str) -> String {
    const SUFFIX: &str = " · Nearby";
    let mut end = name.len().min(63 - SUFFIX.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{SUFFIX}", &name[..end])
}

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    static _nw_parameters_configure_protocol_disable: *mut Configure;
    static _nw_content_context_default_stream: Raw;
    fn nw_retain(object: Raw) -> Raw;
    fn nw_release(object: Raw);
    fn nw_parameters_create_secure_tcp(tls: *mut Configure, tcp: *mut Configure) -> Raw;
    fn nw_parameters_set_include_peer_to_peer(parameters: Raw, enabled: bool);
    fn nw_parameters_set_local_only(parameters: Raw, enabled: bool);
    fn nw_tcp_options_set_no_delay(options: Raw, enabled: bool);
    fn nw_advertise_descriptor_create_bonjour_service(
        name: *const c_char,
        service: *const c_char,
        domain: *const c_char,
    ) -> Raw;
    fn nw_advertise_descriptor_set_txt_record(descriptor: Raw, txt: *const c_void, len: usize);
    fn nw_listener_create(parameters: Raw) -> Raw;
    fn nw_listener_get_port(listener: Raw) -> u16;
    fn nw_listener_set_queue(listener: Raw, queue: &DispatchQueue);
    fn nw_listener_set_advertise_descriptor(listener: Raw, descriptor: Raw);
    fn nw_listener_set_state_changed_handler(listener: Raw, handler: *mut ListenerState);
    fn nw_listener_set_new_connection_handler(listener: Raw, handler: *mut NewConnection);
    fn nw_listener_start(listener: Raw);
    fn nw_listener_cancel(listener: Raw);
    fn nw_connection_set_queue(connection: Raw, queue: &DispatchQueue);
    fn nw_connection_start(connection: Raw);
    fn nw_connection_cancel(connection: Raw);
    fn nw_connection_receive(connection: Raw, min: u32, max: u32, handler: *mut Receive);
    fn nw_connection_send(
        connection: Raw,
        content: *const DispatchData,
        context: Raw,
        complete: bool,
        completion: *mut SendCompletion,
    );
    fn nw_error_get_error_code(error: Raw) -> i32;
}

struct NetworkObject(NonNull<c_void>);

// Network objects are retained independently of the dispatch queue. The framework
// synchronizes their lifetime and serializes callbacks on their specified queue.
unsafe impl Send for NetworkObject {}
unsafe impl Sync for NetworkObject {}

impl NetworkObject {
    fn from_owned(raw: Raw) -> io::Result<Self> {
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| io::Error::other("macOS could not create a nearby network object"))
    }

    fn raw(&self) -> Raw {
        self.0.as_ptr()
    }
}

impl Drop for NetworkObject {
    fn drop(&mut self) {
        unsafe { nw_release(self.raw()) };
    }
}

pub struct Nearby {
    listeners: Vec<Listener>,
}

impl Nearby {
    pub fn start(runtime: &Handle, target: Arc<PeerTarget>) -> io::Result<Self> {
        let queue = DispatchQueue::new("com.oseanstudio.myremote.nearby", None);
        let name = CString::new(target.name())?;
        let listeners = target
            .records()
            .into_iter()
            .map(|record| Listener::start(runtime, queue.clone(), &name, target.clone(), record))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { listeners })
    }

    pub fn ports(&self) -> Option<Vec<u16>> {
        self.listeners
            .iter()
            .map(|listener| {
                (listener.state.load(Ordering::Acquire) == LISTENER_READY)
                    .then(|| listener.port.load(Ordering::Relaxed))
                    .filter(|port| *port != 0)
            })
            .collect()
    }

    pub fn unavailable(&self) -> bool {
        self.listeners.iter().any(|listener| {
            matches!(
                listener.state.load(Ordering::Acquire),
                LISTENER_FAILED | LISTENER_CANCELLED
            )
        })
    }

    pub async fn wait_for_ports(&self) -> io::Result<Vec<u16>> {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(ports) = self.ports() {
                    return Ok(ports);
                }
                if self.unavailable() {
                    return Err(io::Error::other("A nearby listener failed to start"));
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Waiting for nearby listeners"))?
    }
}

struct Listener {
    native: Arc<NetworkObject>,
    state: Arc<AtomicU32>,
    port: Arc<AtomicU16>,
    task: JoinHandle<()>,
}

impl Listener {
    fn start(
        runtime: &Handle,
        queue: DispatchRetained<DispatchQueue>,
        name: &CString,
        target: Arc<PeerTarget>,
        record: DiscoveryRecord,
    ) -> io::Result<Self> {
        let service = CString::new(record.service_type.as_str())?;
        let txt = crate::mac_bonjour::txt_record(&record.txt)?;
        let configure_tcp = RcBlock::new(|options: Raw| unsafe {
            nw_tcp_options_set_no_delay(options, true);
        });

        // Companion applies its existing encrypted session protocol over this TCP stream.
        let parameters = NetworkObject::from_owned(unsafe {
            nw_parameters_create_secure_tcp(
                _nw_parameters_configure_protocol_disable,
                RcBlock::as_ptr(&configure_tcp),
            )
        })?;
        unsafe {
            nw_parameters_set_include_peer_to_peer(parameters.raw(), true);
            nw_parameters_set_local_only(parameters.raw(), true);
        }
        let native = Arc::new(NetworkObject::from_owned(unsafe {
            nw_listener_create(parameters.raw())
        })?);
        let descriptor = NetworkObject::from_owned(unsafe {
            nw_advertise_descriptor_create_bonjour_service(name.as_ptr(), service.as_ptr(), null())
        })?;
        let (incoming, accepted) = mpsc::channel(MAX_CONNECTIONS);
        let state = Arc::new(AtomicU32::new(0));
        let port = Arc::new(AtomicU16::new(0));
        let state_notice = {
            let state = state.clone();
            let port = port.clone();
            let native = Arc::downgrade(&native);
            let service = record.service_type.clone();
            RcBlock::new(move |next: u32, error: Raw| {
                if next == LISTENER_READY {
                    if let Some(listener) = native.upgrade() {
                        port.store(
                            unsafe { nw_listener_get_port(listener.raw()) },
                            Ordering::Relaxed,
                        );
                    }
                }
                if !error.is_null() {
                    tracing::debug!(
                        "Nearby {service} listener state {next}: macOS error {}",
                        unsafe { nw_error_get_error_code(error) }
                    );
                }
                state.store(next, Ordering::Release);
            })
        };
        let new_connection = RcBlock::new(move |raw: Raw| {
            // The callback borrows this connection. Retain it before handing it to Tokio.
            if let Ok(object) = NetworkObject::from_owned(unsafe { nw_retain(raw) }) {
                let _ = incoming.try_send(Connection(object));
            }
        });
        // Network.framework retains the queue, descriptor and copies each handler.
        // The descriptor copies its TXT bytes before returning.
        unsafe {
            nw_advertise_descriptor_set_txt_record(
                descriptor.raw(),
                txt.as_ptr().cast(),
                txt.len(),
            );
            nw_listener_set_queue(native.raw(), &queue);
            nw_listener_set_advertise_descriptor(native.raw(), descriptor.raw());
            nw_listener_set_state_changed_handler(native.raw(), RcBlock::as_ptr(&state_notice));
            nw_listener_set_new_connection_handler(native.raw(), RcBlock::as_ptr(&new_connection));
            nw_listener_start(native.raw());
        }
        let task = runtime.spawn(serve(
            accepted,
            queue,
            target,
            record.service_type,
            port.clone(),
        ));
        Ok(Self {
            native,
            state,
            port,
            task,
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe { nw_listener_cancel(self.native.raw()) };
        self.task.abort();
    }
}

async fn serve(
    mut accepted: mpsc::Receiver<Connection>,
    queue: DispatchRetained<DispatchQueue>,
    target: Arc<PeerTarget>,
    service: String,
    port: Arc<AtomicU16>,
) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            connection = accepted.recv(), if clients.len() < MAX_CONNECTIONS => {
                let Some(connection) = connection else { break };
                let service = service.clone();
                let target = target.clone();
                let port = port.load(Ordering::Relaxed);
                unsafe {
                    nw_connection_set_queue(connection.0.raw(), &queue);
                    nw_connection_start(connection.0.raw());
                }
                clients.spawn(async move {
                    let (transport, receiver) = tokio::io::duplex(BUFFER);
                    tokio::join!(connection.transfer(transport), target.serve(&service, receiver, port));
                });
            }
        }
    }
}

struct Connection(NetworkObject);

impl Connection {
    async fn transfer(self, stream: DuplexStream) {
        let (mut outgoing, mut incoming) = tokio::io::split(stream);
        let send = async {
            let mut bytes = [0; BUFFER];
            loop {
                let count = outgoing.read(&mut bytes).await?;
                if count == 0 {
                    return self.send(None).await;
                }
                self.send(Some(&bytes[..count])).await?;
            }
        };
        let receive = async {
            loop {
                let (bytes, complete) = self.receive().await?;
                incoming.write_all(&bytes).await?;
                if complete {
                    return incoming.shutdown().await;
                }
            }
        };
        tokio::pin!(send);
        let result = tokio::select! {
            result = &mut send => result,
            result = receive => match result {
                Ok(()) => send.await,
                Err(error) => Err(error),
            },
        };
        if let Err(error) = result {
            tracing::debug!("Nearby connection closed: {error}");
        }
    }

    async fn receive(&self) -> io::Result<(Vec<u8>, bool)> {
        let (complete, mut response) = mpsc::channel(1);
        {
            let handler = RcBlock::new(
                move |data: Raw, _context: Raw, final_chunk: u8, error: Raw| {
                    let result = network_result(error).map(|()| {
                        // The dispatch data is borrowed for this callback only.
                        let bytes = unsafe { data.cast::<DispatchData>().as_ref() }
                            .map_or_else(Vec::new, DispatchData::to_vec);
                        (bytes, final_chunk != 0)
                    });
                    let _ = complete.try_send(result);
                },
            );
            unsafe {
                nw_connection_receive(self.0.raw(), 1, BUFFER as u32, RcBlock::as_ptr(&handler));
            }
        }
        response.recv().await.ok_or_else(closed)?
    }

    async fn send(&self, bytes: Option<&[u8]>) -> io::Result<()> {
        let (complete, mut response) = mpsc::channel(1);
        {
            let data = bytes.map(DispatchData::from_bytes);
            let handler = RcBlock::new(move |error: Raw| {
                let _ = complete.try_send(network_result(error));
            });
            unsafe {
                nw_connection_send(
                    self.0.raw(),
                    data.as_deref()
                        .map_or(null(), |data| data as *const DispatchData),
                    _nw_content_context_default_stream,
                    data.is_none(),
                    RcBlock::as_ptr(&handler),
                );
            }
        }
        response.recv().await.ok_or_else(closed)?
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { nw_connection_cancel(self.0.raw()) };
    }
}

fn network_result(error: Raw) -> io::Result<()> {
    if error.is_null() {
        Ok(())
    } else {
        let code = unsafe { nw_error_get_error_code(error) };
        Err(io::Error::other(format!("macOS network error {code}")))
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "nearby connection closed")
}
