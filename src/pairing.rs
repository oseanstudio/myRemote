use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use rand::{rngs::OsRng, Rng, RngCore};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Error, Result};

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);

struct PairingWindow {
    generation: u64,
    pin: u32,
    expires: Instant,
    attempts: u8,
}

pub struct PairingState {
    path: PathBuf,
    _lock: File,
    pub(crate) private_key: [u8; 32],
    pub(crate) identifier: String,
    pub(crate) mac: [u8; 6],
    window: Mutex<Option<PairingWindow>>,
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
            window: Mutex::new(None),
        })
    }

    pub fn enable_pairing(&self) -> u32 {
        let pin = OsRng.gen_range(1000..=9999);
        *self.window.lock().unwrap() = Some(PairingWindow {
            generation: OsRng.next_u64(),
            pin,
            expires: Instant::now() + PAIRING_WINDOW,
            attempts: 8,
        });
        pin
    }

    pub fn pairing_code(&self) -> Option<u32> {
        self.window
            .lock()
            .unwrap()
            .as_ref()
            .filter(|window| Instant::now() < window.expires)
            .map(|window| window.pin)
    }

    pub(crate) fn begin_pairing(&self) -> Option<(u32, u64)> {
        let mut window = self.window.lock().unwrap();
        let window = window.as_mut()?;
        if Instant::now() >= window.expires || window.attempts == 0 {
            return None;
        }
        window.attempts -= 1;
        Some((window.pin, window.generation))
    }

    pub(crate) fn trust(&self, generation: u64, id: &[u8], key: [u8; 32]) -> Result<()> {
        let mut window = self.window.lock().unwrap();
        if !window.as_ref().is_some_and(|window| {
            window.generation == generation && Instant::now() < window.expires
        }) {
            return Err(Error::Protocol("pairing window closed".into()));
        }
        write_private(&self.controller_file(id), &key)?;
        *window = None;
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
