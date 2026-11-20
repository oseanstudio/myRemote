use std::ffi::{c_char, c_void, CString};
use std::io;
use std::net::Ipv4Addr;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use myremote::DiscoveryRecord;

const PENDING: i32 = i32::MAX;
const POLICY_DENIED: i32 = -65570;
const SERVICE_UNAVAILABLE: i32 = -65563;
type ServiceRef = *mut c_void;
type RegisterCallback = unsafe extern "C" fn(
    ServiceRef,
    u32,
    i32,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);

#[link(name = "dns_sd")]
unsafe extern "C" {
    fn DNSServiceRegister(
        reference: *mut ServiceRef,
        flags: u32,
        interface: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_length: u16,
        txt: *const c_void,
        callback: Option<RegisterCallback>,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(reference: ServiceRef) -> libc::c_int;
    fn DNSServiceProcessResult(reference: ServiceRef) -> i32;
    fn DNSServiceRefDeallocate(reference: ServiceRef);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Starting,
    Ready,
    LocalNetworkPermission,
    Failed,
}

pub struct Bonjour {
    state: Arc<[AtomicI32]>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Bonjour {
    pub fn start(
        name: &str,
        ip: Option<Ipv4Addr>,
        records: Vec<DiscoveryRecord>,
    ) -> io::Result<Self> {
        let interface = interface_index(ip)?;
        let name = CString::new(name)?;
        let state: Arc<[AtomicI32]> = (0..records.len())
            .map(|_| AtomicI32::new(PENDING))
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("myremote-bonjour".into())
            .spawn(move || {
                process(interface, &name, records, &worker_state, &worker_stop);
            })?;
        Ok(Self {
            state,
            stop,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> Status {
        let codes: Vec<_> = self
            .state
            .iter()
            .map(|code| code.load(Ordering::Relaxed))
            .collect();
        if codes.contains(&POLICY_DENIED) {
            Status::LocalNetworkPermission
        } else if codes.iter().any(|&code| code != PENDING && code != 0) {
            Status::Failed
        } else if codes.contains(&PENDING) {
            Status::Starting
        } else {
            Status::Ready
        }
    }
}

impl Drop for Bonjour {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn interface_index(ip: Option<Ipv4Addr>) -> io::Result<u32> {
    let Some(ip) = ip else { return Ok(0) };
    if ip.is_loopback() {
        return Ok(u32::MAX); // kDNSServiceInterfaceIndexLocalOnly
    }
    let interface = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|address| address.ip() == ip)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "The selected address is no longer available.",
            )
        })?;
    let name = CString::new(interface.name)?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

struct Registration(ServiceRef);

impl Drop for Registration {
    fn drop(&mut self) {
        unsafe { DNSServiceRefDeallocate(self.0) };
    }
}

fn process(
    interface: u32,
    name: &CString,
    records: Vec<DiscoveryRecord>,
    state: &[AtomicI32],
    stop: &AtomicBool,
) {
    let mut services = Vec::new();
    let mut sockets = Vec::new();
    for (record, status) in records.iter().zip(state) {
        match register(interface, name, record, status) {
            Ok(reference) => {
                let fd = unsafe { DNSServiceRefSockFD(reference.0) };
                if fd < 0 {
                    status.store(SERVICE_UNAVAILABLE, Ordering::Relaxed);
                    continue;
                }
                sockets.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                services.push((reference, status));
            }
            Err(code) => {
                status.store(code, Ordering::Relaxed);
                tracing::debug!("Bonjour rejected {} with error {code}", record.service_type);
            }
        }
    }
    while !stop.load(Ordering::Relaxed) && sockets.iter().any(|socket| socket.fd >= 0) {
        // The registration callbacks run synchronously on this thread while we own the refs.
        let ready = unsafe { libc::poll(sockets.as_mut_ptr(), sockets.len() as libc::nfds_t, 125) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            for (_, status) in &services {
                status.store(SERVICE_UNAVAILABLE, Ordering::Relaxed);
            }
            break;
        }
        for (socket, (reference, status)) in sockets.iter_mut().zip(&services) {
            let error = if socket.revents & libc::POLLIN != 0 {
                unsafe { DNSServiceProcessResult(reference.0) }
            } else if socket.revents != 0 {
                SERVICE_UNAVAILABLE
            } else {
                continue;
            };
            if error != 0 {
                status.store(error, Ordering::Relaxed);
                socket.fd = -1;
            }
        }
    }
}

fn register(
    interface: u32,
    name: &CString,
    record: &DiscoveryRecord,
    status: &AtomicI32,
) -> Result<Registration, i32> {
    let service_type = CString::new(record.service_type.as_str()).map_err(|_| -65540)?;
    let mut txt = Vec::new();
    for (key, value) in &record.txt {
        let len = key.len() + 1 + value.len();
        txt.push(u8::try_from(len).map_err(|_| -65540)?);
        txt.extend_from_slice(key.as_bytes());
        txt.push(b'=');
        txt.extend_from_slice(value.as_bytes());
    }
    let mut reference = null_mut();
    // Local domain, system hostname, network byte order for the service port.
    // status points into the owned Arc and outlives every DNSServiceRef.
    let code = unsafe {
        DNSServiceRegister(
            &mut reference,
            0,
            interface,
            name.as_ptr(),
            service_type.as_ptr(),
            c"local.".as_ptr(),
            null(),
            record.port.to_be(),
            u16::try_from(txt.len()).map_err(|_| -65540)?,
            txt.as_ptr().cast(),
            Some(registered),
            (status as *const AtomicI32).cast_mut().cast(),
        )
    };
    if code == 0 {
        Ok(Registration(reference))
    } else {
        Err(code)
    }
}

unsafe extern "C" fn registered(
    _reference: ServiceRef,
    _flags: u32,
    code: i32,
    _name: *const c_char,
    _service_type: *const c_char,
    _domain: *const c_char,
    context: *mut c_void,
) {
    // DNSServiceProcessResult invokes this with the AtomicI32 passed during registration.
    unsafe { &*context.cast::<AtomicI32>() }.store(code, Ordering::Relaxed);
    if code != 0 {
        tracing::debug!("Bonjour callback returned error {code}");
    }
}
