use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use rand::{rngs::OsRng, Rng, RngCore};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Error, Result};

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const AUTOMATIC_PAIRING_COOLDOWN: Duration = Duration::from_secs(300);

struct PairingWindow {
    generation: u64,
    pin: u32,
    expires: Instant,
    attempts: u8,
    show_code: bool,
}

struct Pairing {
    window: Option<PairingWindow>,
    next_automatic: Option<Instant>,
    status: &'static str,
    connected_remotes: usize,
    remotes_with_input: usize,
}

pub struct PairingState {
    path: PathBuf,
    _lock: File,
    pub(crate) private_key: [u8; 32],
    pub(crate) identifier: String,
    pub(crate) mac: [u8; 6],
    pairing: Mutex<Pairing>,
    pub(crate) diagnostics: Diagnostics,
}

#[derive(Default)]
pub(crate) struct Diagnostics {
    pub verified_connections: AtomicU32,
    pub double_clicks: AtomicU32,
    pub keyboard_messages: AtomicU32,
    pub keyboard_edits: AtomicU32,
    pub keyboard_posted: AtomicU32,
    pub player: AtomicU8,
}

impl PairingState {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join("receiver.lock"))?;
        lock.try_lock().map_err(|err| {
            Error::Protocol(format!("cannot lock state at {}: {err}", path.display()))
        })?;
        let key_path = path.join("identity.key");
        let private_key = match fs::read(&key_path) {
            Ok(key) => key
                .try_into()
                .map_err(|_| Error::Protocol("invalid identity.key".into()))?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut key = [0; 32];
                OsRng.fill_bytes(&mut key);
                write_private(&key_path, &key)?;
                key
            }
            Err(err) => return Err(err.into()),
        };
        let identity_hash = Sha256::new()
            .chain_update(b"myRemote device identifier v1")
            .chain_update(private_key)
            .finalize();
        let identifier = Uuid::from_bytes(identity_hash[..16].try_into().unwrap())
            .to_string()
            .to_uppercase();
        let mut mac: [u8; 6] = identity_hash[16..22].try_into().unwrap();
        mac[0] = (mac[0] | 2) & !1;
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
            private_key,
            identifier,
            mac,
            pairing: Mutex::new(Pairing {
                window: None,
                next_automatic: None,
                status: "Waiting for iPhone",
                connected_remotes: 0,
                remotes_with_input: 0,
            }),
            diagnostics: Diagnostics::default(),
        })
    }

    pub fn remote_diagnostics(&self) -> String {
        let counters = &self.diagnostics;
        let read = |counter: &AtomicU32| counter.load(Ordering::Relaxed);
        let player = match counters.player.load(Ordering::Relaxed) {
            1 => "Jellyfin",
            2 => "YouTube",
            _ => "None",
        };
        format!(
            "Verified connections: {}\nDouble-click events sent: {}\nKeyboard messages received: {}\nKeyboard edits decoded: {}\nKeyboard edits posted: {}\nLast player advertised: {player}",
            read(&counters.verified_connections),
            read(&counters.double_clicks),
            read(&counters.keyboard_messages),
            read(&counters.keyboard_edits),
            read(&counters.keyboard_posted),
        )
    }

    pub fn enable_pairing_on_request(&self) {
        self.pairing.lock().unwrap().next_automatic = Some(Instant::now());
    }

    pub fn enable_pairing(&self) -> u32 {
        let mut pairing = self.pairing.lock().unwrap();
        let now = Instant::now();
        if let Some(window) = pairing
            .window
            .as_mut()
            .filter(|window| now < window.expires && window.attempts > 0)
        {
            window.show_code = false;
            return window.pin;
        }
        pairing.open(now, false)
    }

    pub fn pairing_code(&self) -> Option<u32> {
        self.pairing
            .lock()
            .unwrap()
            .window
            .as_ref()
            .filter(|window| Instant::now() < window.expires)
            .map(|window| window.pin)
    }

    pub fn take_requested_code(&self) -> Option<u32> {
        let mut pairing = self.pairing.lock().unwrap();
        let window = pairing.window.as_mut()?;
        if Instant::now() < window.expires && std::mem::take(&mut window.show_code) {
            return Some(window.pin);
        }
        None
    }

    pub fn connection_status(&self) -> &'static str {
        let pairing = self.pairing.lock().unwrap();
        if pairing.remotes_with_input > 0 {
            "iPhone input received"
        } else if pairing.connected_remotes > 0 {
            "iPhone remote connected"
        } else {
            pairing.status
        }
    }

    pub(crate) fn set_connection_status(&self, status: &'static str) {
        self.pairing.lock().unwrap().status = status;
    }

    pub(crate) fn remote_started(&self) {
        self.pairing.lock().unwrap().connected_remotes += 1;
    }

    pub(crate) fn remote_received_input(&self) {
        self.pairing.lock().unwrap().remotes_with_input += 1;
    }

    pub(crate) fn remote_stopped(&self, received_input: bool) {
        let mut pairing = self.pairing.lock().unwrap();
        pairing.connected_remotes -= 1;
        if received_input {
            pairing.remotes_with_input -= 1;
        }
    }

    pub(crate) fn begin_pairing(&self) -> Option<(u32, u64)> {
        let mut pairing = self.pairing.lock().unwrap();
        let now = Instant::now();
        if pairing
            .window
            .as_ref()
            .is_none_or(|window| now >= window.expires)
        {
            if !pairing.next_automatic.is_some_and(|next| now >= next) {
                pairing.status = "iPhone requested pairing · choose Pair iPhone";
                return None;
            }
            pairing.open(now, true);
        }
        let window = pairing.window.as_mut().unwrap();
        if window.attempts == 0 {
            pairing.status = "Pairing attempts used · choose Pair iPhone";
            return None;
        }
        window.attempts -= 1;
        Some((window.pin, window.generation))
    }

    pub(crate) fn trust(&self, generation: u64, id: &[u8], key: [u8; 32]) -> Result<()> {
        let mut pairing = self.pairing.lock().unwrap();
        if !pairing.window.as_ref().is_some_and(|window| {
            window.generation == generation && Instant::now() < window.expires
        }) {
            return Err(Error::Protocol("pairing window closed".into()));
        }
        write_private(&self.controller_file(id), &key)?;
        pairing.window = None;
        pairing.status = "iPhone paired";
        Ok(())
    }

    pub(crate) fn controller_key(&self, id: &[u8]) -> Result<VerifyingKey> {
        let key: [u8; 32] = fs::read(self.controller_file(id))
            .map_err(|_| Error::Protocol("controller is not paired".into()))?
            .try_into()
            .map_err(|_| Error::Protocol("invalid stored controller key".into()))?;
        VerifyingKey::from_bytes(&key).map_err(|_| Error::Protocol("invalid controller key".into()))
    }

    pub fn forget_phones(&self) -> Result<usize> {
        let mut count = 0;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy().starts_with("phone-")
            {
                fs::remove_file(entry.path())?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn controller_file(&self, id: &[u8]) -> PathBuf {
        self.path
            .join(format!("phone-{}.pub", hex::encode(Sha256::digest(id))))
    }
}

impl Pairing {
    fn open(&mut self, now: Instant, show_code: bool) -> u32 {
        let pin = OsRng.gen_range(1000..=9999);
        self.window = Some(PairingWindow {
            generation: OsRng.next_u64(),
            pin,
            expires: now + PAIRING_WINDOW,
            attempts: if show_code { 3 } else { 8 },
            show_code,
        });
        if self.next_automatic.is_some() {
            self.next_automatic = Some(now + AUTOMATIC_PAIRING_COOLDOWN);
        }
        self.status = "Waiting for the pairing code";
        pin
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("pending");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_connection_status_does_not_hide_live_remote_input() {
        let directory = tempfile::tempdir().unwrap();
        let state = PairingState::open(directory.path()).unwrap();
        state.remote_started();
        state.remote_received_input();
        state.remote_started();
        state.set_connection_status("iPhone verified · waiting for Remote session");
        assert_eq!(state.connection_status(), "iPhone input received");

        state.remote_stopped(true);
        assert_eq!(state.connection_status(), "iPhone remote connected");

        state.set_connection_status("iPhone remote disconnected");
        state.remote_stopped(false);
        assert_eq!(state.connection_status(), "iPhone remote disconnected");
    }
}
