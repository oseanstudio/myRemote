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
const FAST_WATCH_TIME: Duration = Duration::from_secs(5 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
const SLOW_PROBE_INTERVAL: Duration = Duration::from_secs(30);
const BUNDLE_ID: &str = "com.oseanstudio.myremote";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Missing,
    Accessibility,
    PostEvents,
}

impl Permission {
    pub fn current() -> Self {
        if !input::accessibility_is_trusted() {
            Self::Missing
        } else if input::can_post_input() {
            Self::PostEvents
        } else {
            Self::Accessibility
        }
    }

    pub fn previous_relaunch() -> Self {
        match env::var(RELAUNCHED_ENV).as_deref() {
            Ok("accessibility" | "1") => Self::Accessibility,
            Ok("granted") => Self::PostEvents,
            _ => Self::Missing,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "denied",
            Self::Accessibility => "accessibility",
            Self::PostEvents => "granted",
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

pub fn run_probe() -> Option<ExitCode> {
    if !env::args_os().nth(1).is_some_and(|arg| arg == PROBE_ARG) {
        return None;
    }
    println!("{}", Permission::current().label());
    Some(ExitCode::SUCCESS)
}

pub struct ApprovalWatch {
    baseline: Permission,
    granted: oneshot::Receiver<Permission>,
    worker: JoinHandle<()>,
}

impl ApprovalWatch {
    pub fn start(bundle: PathBuf, runtime: &Runtime, baseline: Permission) -> Self {
        let (notify, granted) = oneshot::channel();
        let worker = runtime.spawn(async move {
            let started = Instant::now();
            loop {
                let interval = if started.elapsed() < FAST_WATCH_TIME {
                    PROBE_INTERVAL
                } else {
                    SLOW_PROBE_INTERVAL
                };
                sleep(interval).await;
                match probe_fresh_process(&bundle).await {
                    Ok(permission) if permission > baseline => {
                        let _ = notify.send(permission);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!("Could not check Accessibility in a fresh process: {error}")
                    }
                }
            }
        });
        Self {
            baseline,
            granted,
            worker,
        }
    }

    pub fn granted(&mut self) -> Option<Permission> {
        let fresh = self.granted.try_recv().unwrap_or(Permission::Missing);
        new_grant(self.baseline, Permission::current(), fresh)
    }
}

impl Drop for ApprovalWatch {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

fn new_grant(baseline: Permission, current: Permission, fresh: Permission) -> Option<Permission> {
    let granted = current.max(fresh);
    (granted > baseline).then_some(granted)
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
    fn watches_both_grants_without_relaunching_twice_for_the_same_permission() {
        use Permission::*;

        assert_eq!(new_grant(Missing, Missing, Missing), None);
        assert_eq!(
            new_grant(Missing, Missing, Accessibility),
            Some(Accessibility)
        );
        assert_eq!(new_grant(Accessibility, Accessibility, Accessibility), None);
        assert_eq!(
            new_grant(Accessibility, Accessibility, PostEvents),
            Some(PostEvents)
        );
        assert_eq!(
            new_grant(Accessibility, PostEvents, Missing),
            Some(PostEvents)
        );
        assert_eq!(new_grant(PostEvents, Accessibility, PostEvents), None);
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
