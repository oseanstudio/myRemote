mod install;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::{default_state_dir, Options, VERSION};
pub use install::{clean_helper, run_helper};
#[cfg(target_os = "macos")]
pub use install::{
    destination as app_bundle_path, prepare_accessibility_relaunch, run_accessibility_relaunch,
    ACCESSIBILITY_RELAUNCH_ARG,
};

const REPO: &str = "oseanstudio/myRemote";
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PACKAGE: u64 = 100 * 1024 * 1024;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    check: bool,
    install: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            check: true,
            install: false,
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

struct Download {
    version: Version,
    asset: &'static str,
    size: u64,
    digest: [u8; 32],
}

enum Finished {
    Checked(ResultMessage<Option<Download>>, bool),
    Prepared(ResultMessage<install::Ready>, bool),
}

type ResultMessage<T> = std::result::Result<T, String>;

pub struct Updater {
    path: Option<PathBuf>,
    preferences: Preferences,
    next_check: Instant,
    task: Option<Receiver<Finished>>,
    status: String,
}

impl Updater {
    pub fn new(options: &Options) -> Self {
        let path = options
            .state_dir
            .clone()
            .or_else(default_state_dir)
            .map(|directory| directory.join("updates.json"));
        let preferences = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        let previous_error = default_state_dir().and_then(|directory| {
            let file = directory.join("update-error.txt");
            let message = fs::read_to_string(&file).ok()?;
            let _ = fs::remove_file(file);
            Some(message)
        });
        let delay = if previous_error.is_some() {
            INTERVAL
        } else {
            Duration::from_secs(5)
        };
        Self {
            path,
            preferences,
            next_check: Instant::now() + delay,
            task: None,
            status: previous_error.unwrap_or_else(|| "Ready to check for updates".to_owned()),
        }
    }

    pub fn checking(&self) -> bool {
        self.preferences.check
    }
    pub fn automatic(&self) -> bool {
        self.preferences.install
    }
    pub fn busy(&self) -> bool {
        self.task.is_some()
    }
    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn toggle_checks(&mut self) {
        let check = !self.checking();
        self.save(Preferences {
            check,
            install: check && self.automatic(),
        });
    }

    pub fn toggle_installation(&mut self) {
        let install = !self.automatic();
        self.save(Preferences {
            check: install || self.checking(),
            install,
        });
        if self.automatic() {
            self.next_check = Instant::now();
        }
    }

    pub fn update_now(&mut self) {
        if !self.busy() {
            self.check(true);
        }
    }

    /// Poll on the UI thread. Returns true when the app should exit for installation.
    pub fn poll(&mut self) -> bool {
        if let Some(task) = self.task.as_ref() {
            match task.try_recv() {
                Ok(finished) => {
                    self.task = None;
                    self.next_check = Instant::now() + INTERVAL;
                    match finished {
                        Finished::Checked(Ok(Some(download)), manual)
                            if manual || self.automatic() =>
                        {
                            self.prepare(download, manual);
                        }
                        Finished::Checked(Ok(Some(download)), _) => {
                            self.status = format!("Version {} is available", download.version);
                        }
                        Finished::Checked(Ok(None), _) => {
                            self.status = "You have the latest version".into();
                        }
                        Finished::Prepared(Ok(ready), manual) if manual || self.automatic() => {
                            match install::launch(ready) {
                                Ok(()) => return true,
                                Err(err) => {
                                    self.status = format!("Could not install update: {err}")
                                }
                            }
                        }
                        Finished::Prepared(Ok(_), _) => {
                            self.status = "Automatic installation paused".into()
                        }
                        Finished::Checked(Err(error), _) | Finished::Prepared(Err(error), _) => {
                            self.status = error;
                        }
                    }
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.task = None;
                    self.status = "Update interrupted".into();
                }
            }
        }
        if !self.busy() && self.checking() && Instant::now() >= self.next_check {
            self.check(false);
        }
        false
    }

    fn save(&mut self, preferences: Preferences) {
        let result = self
            .path
            .as_deref()
            .ok_or_else(|| io::Error::other("settings directory unavailable"))
            .and_then(|path| save_preferences(path, preferences));
        match result {
            Ok(()) => {
                self.preferences = preferences;
                self.next_check = Instant::now() + Duration::from_secs(5);
            }
            Err(err) => self.status = format!("Could not save update settings: {err}"),
        }
    }

    fn check(&mut self, manual: bool) {
        self.status = "Checking GitHub for updates…".into();
        self.next_check = Instant::now() + INTERVAL;
        let (tx, rx) = mpsc::channel();
        self.task = Some(rx);
        std::thread::spawn(move || {
            let result = latest().map_err(|error| format!("Could not check for updates: {error}"));
            let _ = tx.send(Finished::Checked(result, manual));
        });
    }

    fn prepare(&mut self, download: Download, manual: bool) {
        self.status = format!("Downloading myRemote {}…", download.version);
        let (tx, rx) = mpsc::channel();
        self.task = Some(rx);
        std::thread::spawn(move || {
            let result =
                prepare(download).map_err(|error| format!("Could not prepare update: {error}"));
            let _ = tx.send(Finished::Prepared(result, manual));
        });
    }
}

fn save_preferences(path: &Path, preferences: Preferences) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("settings directory unavailable"))?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temp.as_file_mut(), &preferences)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

fn latest() -> Result<Option<Download>> {
    let output = curl(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        1024 * 1024,
        20,
    )
    .args(["--header", "Accept: application/vnd.github+json"])
    .output()?;
    if !output.status.success() {
        return Err("GitHub release request failed".into());
    }
    let release: Release = serde_json::from_slice(&output.stdout)?;
    choose_release(release, &Version::parse(VERSION)?, install::ASSET)
}

fn choose_release(
    release: Release,
    current: &Version,
    asset: &'static str,
) -> Result<Option<Download>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .ok_or("invalid release tag")?,
    )?;
    if !version.pre.is_empty() || !version.build.is_empty() || &version <= current {
        return Ok(None);
    }
    let package = release
        .assets
        .iter()
        .find(|item| item.name == asset && (1..=MAX_PACKAGE).contains(&item.size))
        .ok_or("release has no valid package for this computer")?;
    Ok(Some(Download {
        version,
        asset,
        size: package.size,
        digest: parse_digest(package.digest.as_deref())?,
    }))
}

fn prepare(download: Download) -> Result<install::Ready> {
    let target = install::destination()?;
    let directory = target
        .parent()
        .ok_or("cannot find the installation directory")?;
    let stage: TempDir = tempfile::Builder::new()
        .prefix(".myremote-update-")
        .tempdir_in(directory)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("the installation directory must be writable ({error})"),
            )
        })?;
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{}/{}",
        download.version, download.asset
    );
    let package = stage.path().join(download.asset);
    let status = curl(&url, download.size, 180)
        .arg("--output")
        .arg(&package)
        .status()?;
    if !status.success() {
        return Err("GitHub package download failed".into());
    }
    if fs::metadata(&package)?.len() != download.size || file_hash(&package)? != download.digest {
        return Err("download does not match its release checksum".into());
    }
    let prepared = install::prepare(stage.path(), &package, &download.version)?;
    Ok(install::Ready {
        stage,
        target,
        prepared,
    })
}

fn parse_digest(value: Option<&str>) -> Result<[u8; 32]> {
    let digest = value
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("GitHub did not provide a SHA-256 digest for this package")?;
    let mut output = [0; 32];
    hex::decode_to_slice(digest, &mut output)?;
    Ok(output)
}

fn file_hash(path: &Path) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut file = File::open(path)?;
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().into())
}

fn curl(url: &str, limit: u64, timeout: u16) -> Command {
    #[cfg(windows)]
    let mut command = install::system_program("curl.exe");
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/usr/bin/curl");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("curl");
    command
        .args([
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "5",
            "--connect-timeout",
            "10",
            "--user-agent",
            concat!("myRemote/", env!("CARGO_PKG_VERSION")),
        ])
        .arg("--max-filesize")
        .arg(limit.to_string())
        .arg("--max-time")
        .arg(timeout.to_string())
        .arg("--url")
        .arg(url)
        .stdin(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_checks_and_remember_both_choices() {
        let defaults = Preferences::default();
        assert!(defaults.check);
        assert!(!defaults.install);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("updates.json");
        save_preferences(&path, defaults).unwrap();
        save_preferences(
            &path,
            Preferences {
                check: true,
                install: true,
            },
        )
        .unwrap();
        let saved: Preferences = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.check && saved.install);
    }

    #[test]
    fn chooses_only_new_stable_release_with_package_and_checksum() {
        let asset = "app.zip";
        let release = Release {
            tag_name: "v2.0.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: asset.into(),
                size: 1000,
                digest: Some(format!("sha256:{}", "01".repeat(32))),
            }],
        };
        assert_eq!(
            choose_release(release, &Version::new(1, 0, 0), asset)
                .unwrap()
                .unwrap()
                .digest,
            [1; 32]
        );
        let prerelease = Release {
            tag_name: "v2.0.0-rc.1".into(),
            draft: false,
            prerelease: true,
            assets: vec![],
        };
        assert!(choose_release(prerelease, &Version::new(1, 0, 0), asset)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_missing_wrong_algorithm_and_truncated_digests() {
        for value in [None, Some("sha512:aa"), Some("sha256:abcd")] {
            assert!(parse_digest(value).is_err());
        }
    }
}
