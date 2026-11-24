mod install;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::{default_state_dir, version_label, Options, VERSION};
pub use install::{clean_helper, run_helper};
#[cfg(target_os = "macos")]
pub use install::{
    destination as app_bundle_path, prepare_accessibility_relaunch, run_accessibility_relaunch,
    ACCESSIBILITY_RELAUNCH_ARG,
};

const REPO: &str = "oseanstudio/myRemote";
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(5);
const MANUAL_STATUS_DURATION: Duration = Duration::from_secs(15);
const MAX_PACKAGE: u64 = 100 * 1024 * 1024;
const UPDATE_NOW: &str = "Update Now";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct CheckState {
    last_check: Option<SystemTime>,
    available_version: Option<String>,
}

impl CheckState {
    fn read(path: Option<&Path>) -> Self {
        path.and_then(|path| fs::read(path).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn next_check_at(&self, now: SystemTime) -> SystemTime {
        let scheduled = self
            .last_check
            .filter(|last| *last <= now)
            .map_or(now, |last| last + INTERVAL);
        scheduled.max(now + STARTUP_DELAY)
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
    Checked(ResultMessage<Option<Download>>),
    Prepared(ResultMessage<install::Ready>),
}

type ResultMessage<T> = std::result::Result<T, String>;

struct UpdateTask {
    result: Receiver<Finished>,
    manual: bool,
}

pub struct Updater {
    path: Option<PathBuf>,
    check_state: CheckState,
    next_check: SystemTime,
    task: Option<UpdateTask>,
    action_title: String,
    status: String,
    status_expires: Option<Instant>,
}

impl Updater {
    pub fn new(options: &Options) -> Self {
        let path = options
            .state_dir
            .clone()
            .or_else(default_state_dir)
            .map(|directory| directory.join("updates.json"));
        let mut check_state = CheckState::read(path.as_deref());
        let current = Version::parse(VERSION).expect("Cargo package version is valid");
        check_state.available_version = check_state.available_version.filter(|version| {
            Version::parse(version).is_ok_and(|version| is_newer_stable(&version, &current))
        });
        let action_title = update_title(check_state.available_version.as_deref());
        let previous_error = default_state_dir().and_then(|directory| {
            let file = directory.join("update-error.txt");
            let message = fs::read_to_string(&file).ok()?;
            let _ = fs::remove_file(file);
            Some(message)
        });
        let next_check = check_state.next_check_at(SystemTime::now());
        Self {
            path,
            check_state,
            next_check,
            task: None,
            action_title,
            status: previous_error.unwrap_or_default(),
            status_expires: None,
        }
    }

    fn busy(&self) -> bool {
        self.task.is_some()
    }

    #[cfg(target_os = "macos")]
    pub fn has_update(&self) -> bool {
        self.check_state.available_version.is_some()
    }

    pub fn action_title(&self) -> &str {
        &self.action_title
    }

    pub fn status(&self) -> &str {
        if self
            .status_expires
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            ""
        } else {
            &self.status
        }
    }

    pub fn update_now(&mut self) {
        if let Some(task) = self.task.as_mut() {
            // An explicit update request also applies to a check already in progress.
            task.manual = true;
            if self.status.is_empty() {
                self.status = "Checking GitHub for updates…".into();
            }
        } else {
            self.check(true);
        }
    }

    /// Poll on the UI thread. Returns true when the app should exit for installation.
    pub fn poll(&mut self) -> bool {
        if let Some(task) = self.task.as_ref() {
            match task.result.try_recv() {
                Ok(finished) => {
                    let manual = task.manual;
                    self.task = None;
                    match finished {
                        Finished::Checked(Ok(Some(download))) => {
                            self.set_available(Some(download.version.to_string()));
                            self.status.clear();
                            if manual {
                                self.prepare(download);
                            }
                        }
                        Finished::Checked(Ok(None)) => {
                            self.set_available(None);
                            if manual {
                                self.status = "No update found".into();
                                self.status_expires = Some(Instant::now() + MANUAL_STATUS_DURATION);
                            } else {
                                self.status.clear();
                            }
                        }
                        Finished::Prepared(Ok(ready)) => match install::launch(ready) {
                            Ok(()) => return true,
                            Err(err) => self.status = format!("Could not install update: {err}"),
                        },
                        Finished::Checked(Err(error)) | Finished::Prepared(Err(error)) => {
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
        if !self.busy() && SystemTime::now() >= self.next_check {
            self.check(false);
        }
        false
    }

    fn set_available(&mut self, version: Option<String>) {
        self.action_title = update_title(version.as_deref());
        self.check_state.available_version = version;
        self.save_check_state();
    }

    fn save_check_state(&self) {
        if let Some(path) = self.path.as_deref() {
            if let Err(error) = save_check_state(path, &self.check_state) {
                tracing::warn!("Could not save the last update check: {error}");
            }
        }
    }

    fn check(&mut self, manual: bool) {
        let was_transient = self.status_expires.take().is_some();
        if manual {
            self.status = "Checking GitHub for updates…".into();
        } else if was_transient {
            self.status.clear();
        }
        let now = SystemTime::now();
        self.next_check = now + INTERVAL;
        self.check_state.last_check = Some(now);
        self.save_check_state();
        let (tx, rx) = mpsc::channel();
        self.task = Some(UpdateTask { result: rx, manual });
        std::thread::spawn(move || {
            let result = latest().map_err(|error| format!("Could not check for updates: {error}"));
            let _ = tx.send(Finished::Checked(result));
        });
    }

    fn prepare(&mut self, download: Download) {
        self.status = "Downloading update…".into();
        let (tx, rx) = mpsc::channel();
        self.task = Some(UpdateTask {
            result: rx,
            manual: true,
        });
        std::thread::spawn(move || {
            let result =
                prepare(download).map_err(|error| format!("Could not prepare update: {error}"));
            let _ = tx.send(Finished::Prepared(result));
        });
    }
}

fn update_title(version: Option<&str>) -> String {
    version.map_or_else(
        || UPDATE_NOW.to_owned(),
        |version| format!("Update to {}…", version_label(version)),
    )
}

fn save_check_state(path: &Path, state: &CheckState) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("settings directory unavailable"))?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temp.as_file_mut(), state)?;
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
    .args([
        "--header",
        "Accept: application/vnd.github+json",
        "--header",
        "Cache-Control: no-cache",
    ])
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
    if !is_newer_stable(&version, current) {
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

fn is_newer_stable(version: &Version, current: &Version) -> bool {
    version.pre.is_empty() && version.build.is_empty() && version > current
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
    fn background_check_waits_a_day_after_relaunch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("updates.json");
        let checked_at = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let shortly_after = checked_at + Duration::from_secs(30);
        let overdue = checked_at + INTERVAL + Duration::from_secs(30);
        fs::write(&path, r#"{"check":true,"install":true}"#).unwrap();
        let legacy = CheckState::read(Some(&path));
        assert_eq!(legacy.next_check_at(checked_at), checked_at + STARTUP_DELAY);

        let state = CheckState {
            last_check: Some(checked_at),
            available_version: Some("2.0.0".into()),
        };
        save_check_state(&path, &state).unwrap();
        let saved = CheckState::read(Some(&path));
        assert_eq!(saved.next_check_at(shortly_after), checked_at + INTERVAL);
        assert_eq!(saved.next_check_at(overdue), overdue + STARTUP_DELAY);
        assert_eq!(saved.available_version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn chooses_only_new_stable_release_with_package_and_checksum() {
        let asset = "app.zip";
        let release = Release {
            tag_name: "v2.3.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: asset.into(),
                size: 1000,
                digest: Some(format!("sha256:{}", "01".repeat(32))),
            }],
        };
        assert_eq!(
            choose_release(release, &Version::new(0, 2, 32), asset)
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

    #[test]
    fn update_action_shows_available_version_without_a_status_line() {
        let future = SystemTime::now() + INTERVAL;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("updates.json");
        let mut updater = Updater {
            path: Some(path.clone()),
            check_state: CheckState::default(),
            next_check: future,
            task: None,
            action_title: UPDATE_NOW.to_owned(),
            status: String::new(),
            status_expires: None,
        };
        let (sender, result) = mpsc::channel();
        updater.task = Some(UpdateTask {
            result,
            manual: false,
        });
        sender.send(Finished::Checked(Ok(None))).unwrap();
        assert!(!updater.poll());
        assert_eq!(updater.status(), "");
        assert_eq!(updater.action_title(), "Update Now");

        let (sender, result) = mpsc::channel();
        updater.task = Some(UpdateTask {
            result,
            manual: false,
        });
        updater.update_now();
        sender.send(Finished::Checked(Ok(None))).unwrap();
        assert!(!updater.poll());
        assert_eq!(updater.status(), "No update found");
        updater.status_expires = Some(Instant::now());
        assert_eq!(updater.status(), "");

        let (sender, result) = mpsc::channel();
        updater.task = Some(UpdateTask {
            result,
            manual: false,
        });
        updater.status_expires = None;
        sender
            .send(Finished::Checked(Ok(Some(Download {
                version: Version::new(2, 0, 0),
                asset: "app.zip",
                size: 1000,
                digest: [1; 32],
            }))))
            .unwrap();
        assert!(!updater.poll());
        assert_eq!(updater.action_title(), "Update to 2.0…");
        assert_eq!(update_title(Some("2.3.1")), "Update to 2.3.1…");
        assert_eq!(updater.status(), "");
        assert!(updater.task.is_none());
        let saved = CheckState::read(Some(&path));
        assert_eq!(saved.available_version.as_deref(), Some("2.0.0"));

        let (sender, result) = mpsc::channel();
        updater.task = Some(UpdateTask {
            result,
            manual: false,
        });
        sender.send(Finished::Checked(Ok(None))).unwrap();
        assert!(!updater.poll());
        assert_eq!(updater.action_title(), "Update Now");
        assert_eq!(updater.status(), "");
    }
}
