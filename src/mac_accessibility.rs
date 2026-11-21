use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::{input, VERSION};

const PROBE_ARG: &str = "--probe-input-permission";
pub const RELAUNCHED_ENV: &str = "MYREMOTE_ACCESSIBILITY_RELAUNCHED";
const WATCH_TIME: Duration = Duration::from_secs(5 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
const BUNDLE_ID: &str = "com.oseanstudio.myremote";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permission {
    Missing,
    Accessibility,
    PostEvents,
}

impl Permission {
    fn current() -> Self {
        if input::can_post_input() {
            Self::PostEvents
        } else if input::accessibility_is_trusted() {
            Self::Accessibility
        } else {
            Self::Missing
        }
    }

    fn from_probe(bytes: &[u8]) -> io::Result<Self> {
        match bytes {
            b"denied\n" => Ok(Self::Missing),
            b"accessibility\n" => Ok(Self::Accessibility),
            b"granted\n" => Ok(Self::PostEvents),
            _ => Err(io::Error::other("Unexpected permission check response")),
        }
    }
}

struct GrantProgress {
    awaiting_accessibility: bool,
}

impl GrantProgress {
    fn new() -> Self {
        Self {
            awaiting_accessibility: !input::accessibility_is_trusted(),
        }
    }

    fn granted(&mut self, permission: Permission) -> bool {
        match permission {
            Permission::PostEvents => true,
            Permission::Accessibility => self.awaiting_accessibility,
            Permission::Missing => {
                self.awaiting_accessibility = true;
                false
            }
        }
    }
}

pub fn run_probe() -> Option<ExitCode> {
    if !env::args_os().nth(1).is_some_and(|arg| arg == PROBE_ARG) {
        return None;
    }
    println!(
        "{}",
        match Permission::current() {
            Permission::Missing => "denied",
            Permission::Accessibility => "accessibility",
            Permission::PostEvents => "granted",
        }
    );
    Some(ExitCode::SUCCESS)
}

pub struct ApprovalWatch {
    deadline: Instant,
    granted: oneshot::Receiver<()>,
    worker: JoinHandle<()>,
    progress: GrantProgress,
}

impl ApprovalWatch {
    pub fn start(bundle: PathBuf, runtime: &Runtime) -> Self {
        let deadline = Instant::now() + WATCH_TIME;
        let (notify, granted) = oneshot::channel();
        let mut probe_progress = GrantProgress::new();
        let worker = runtime.spawn(async move {
            let watch = async {
                loop {
                    sleep(PROBE_INTERVAL).await;
                    match probe_fresh_process(&bundle).await {
                        Ok(permission) if probe_progress.granted(permission) => {
                            let _ = notify.send(());
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => tracing::debug!(
                            "Could not check Accessibility in a fresh process: {error}"
                        ),
                    }
                }
            };
            let _ = timeout(WATCH_TIME, watch).await;
        });
        Self {
            deadline,
            granted,
            worker,
            progress: GrantProgress::new(),
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn granted(&mut self) -> bool {
        self.progress.granted(Permission::current()) || self.granted.try_recv().is_ok()
    }
}

impl Drop for ApprovalWatch {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn probe_fresh_process(bundle: &Path) -> io::Result<Permission> {
    // Launch the same signed bundle so the check uses a fresh process with the same app identity.
    // The probe exits before initializing AppKit, starting a receiver or requesting permission.
    let output = tempfile::NamedTempFile::new()?;
    let status = timeout(
        Duration::from_secs(8),
        Command::new("/usr/bin/open")
            .args(["-n", "-g", "-j", "-W", "--stdout"])
            .arg(output.path())
            .arg(bundle)
            .args(["--args", PROBE_ARG])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
    if !status.success() {
        return Err(io::Error::other(
            "Launch Services rejected the permission check",
        ));
    }
    Permission::from_probe(&fs::read(output.path())?)
}

pub async fn refresh_updated_permissions(bundle: &Path, state_dir: &Path) -> io::Result<()> {
    if !record_release(state_dir, VERSION)? || input::can_post_input() {
        return Ok(());
    }
    let signature = timeout(
        Duration::from_secs(3),
        Command::new("/usr/bin/codesign")
            .args(["--display", "--verbose=2"])
            .arg(bundle)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
    let description = String::from_utf8_lossy(&signature.stderr);
    if !signature.status.success()
        || !description.lines().any(|line| line == "Signature=adhoc")
        || !description
            .lines()
            .any(|line| line.strip_prefix("Identifier=") == Some(BUNDLE_ID))
    {
        return Ok(());
    }

    // Scope the reset to this app and these two services. macOS still asks the user for consent.
    for service in ["Accessibility", "PostEvent"] {
        let result = timeout(
            Duration::from_secs(3),
            Command::new("/usr/bin/tccutil")
                .args(["reset", service, BUNDLE_ID])
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if !matches!(result, Ok(Ok(output)) if output.status.success()) {
            tracing::warn!("macOS did not reset myRemote’s {service} permission");
        }
    }
    Ok(())
}

fn record_release(state_dir: &Path, version: &str) -> io::Result<bool> {
    fs::create_dir_all(state_dir)?;
    let mut installed = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("accessibility-release"))?;
    installed.try_lock()?;
    let mut last_release = String::new();
    installed.read_to_string(&mut last_release)?;
    if last_release == version {
        return Ok(false);
    }
    installed.rewind()?;
    installed.write_all(version.as_bytes())?;
    installed.set_len(version.len() as u64)?;
    installed.sync_data()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessibility_transition_can_trigger_a_restart_before_events_are_allowed() {
        let mut progress = GrantProgress {
            awaiting_accessibility: false,
        };
        assert!(!progress.granted(Permission::Accessibility));
        assert!(!progress.granted(Permission::Missing));
        assert!(progress.granted(Permission::Accessibility));
        assert!(progress.granted(Permission::PostEvents));
    }

    #[test]
    fn each_release_is_recorded_only_on_first_launch() {
        let directory = tempfile::tempdir().unwrap();
        assert!(record_release(directory.path(), "0.2.11").unwrap());
        assert!(!record_release(directory.path(), "0.2.11").unwrap());
        assert!(record_release(directory.path(), "0.3.0").unwrap());
        assert!(!record_release(directory.path(), "0.3.0").unwrap());
    }
}
