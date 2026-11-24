use std::env;
use std::ffi::{c_char, c_int, CStr};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::{input, VERSION};

const PROBE_ARG: &str = "--probe-input-permission";
pub const RELAUNCHED_ENV: &str = "MYREMOTE_ACCESSIBILITY_RELAUNCHED";
const FAST_WATCH_TIME: Duration = Duration::from_secs(5 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const SLOW_PROBE_INTERVAL: Duration = Duration::from_secs(30);
const BUNDLE_ID: &str = "com.oseanstudio.myremote";
const PRIVACY_CHANGED: &CStr = c"com.apple.tcc.access.changed";

unsafe extern "C" {
    fn notify_register_check(name: *const c_char, token: *mut c_int) -> u32;
    fn notify_check(token: c_int, changed: *mut c_int) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

struct PrivacyChanges(c_int);

impl PrivacyChanges {
    fn subscribe(name: &CStr) -> Option<Self> {
        let mut token = 0;
        if unsafe { notify_register_check(name.as_ptr(), &mut token) } != 0 {
            return None;
        }
        let subscription = Self(token);
        subscription.changed(); // The first check reports a change even before a notification.
        Some(subscription)
    }

    fn changed(&self) -> bool {
        let mut changed = 0;
        unsafe { notify_check(self.0, &mut changed) == 0 && changed != 0 }
    }
}

impl Drop for PrivacyChanges {
    fn drop(&mut self) {
        unsafe { notify_cancel(self.0) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Missing,
    Accessibility,
    PostEvents,
}

impl Permission {
    pub fn current() -> Self {
        Self::from_accessibility(input::accessibility_is_trusted())
    }

    fn live() -> Self {
        Self::from_accessibility(input::probe_accessibility())
    }

    fn from_accessibility(allowed: bool) -> Self {
        if !allowed {
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
    println!("{}", Permission::live().label());
    Some(ExitCode::SUCCESS)
}

pub struct ApprovalWatch {
    changes: Option<PrivacyChanges>,
    wake: Arc<Notify>,
    observed: watch::Receiver<Permission>,
    granted: oneshot::Receiver<Permission>,
    detected_grant: Option<Permission>,
    worker: JoinHandle<()>,
}

impl ApprovalWatch {
    pub fn start(bundle: PathBuf, runtime: &Runtime, initial: Permission) -> Self {
        let changes = PrivacyChanges::subscribe(PRIVACY_CHANGED);
        let wake = Arc::new(Notify::new());
        let worker_wake = wake.clone();
        let (report, observed) = watch::channel(initial);
        let (notify, granted) = oneshot::channel();
        let mut progress = ApprovalProgress {
            last: initial,
            relaunched: Permission::previous_relaunch(),
        };
        let mut in_process = progress;
        let worker = runtime.spawn(async move {
            let mut active_until = Instant::now() + FAST_WATCH_TIME;
            let grant = loop {
                // This can catch a live grant even if Launch Services is slow or unavailable.
                if let Some(grant) = in_process.observe(Permission::live()) {
                    break grant;
                }
                match probe_fresh_process(&bundle).await {
                    Ok(permission) => {
                        if permission < progress.last {
                            active_until = Instant::now() + FAST_WATCH_TIME;
                        }
                        if let Some(grant) = progress.observe(permission) {
                            break grant;
                        }
                        report.send_replace(permission);
                    }
                    Err(error) => {
                        tracing::debug!("Could not check Accessibility in a fresh process: {error}")
                    }
                }
                let interval =
                    if progress.last < Permission::PostEvents && Instant::now() < active_until {
                        PROBE_INTERVAL
                    } else {
                        SLOW_PROBE_INTERVAL
                    };
                tokio::select! {
                    _ = sleep(interval) => {}
                    _ = worker_wake.notified() => {
                        active_until = Instant::now() + FAST_WATCH_TIME;
                    }
                }
            };
            let _ = notify.send(grant);
        });
        Self {
            changes,
            wake,
            observed,
            granted,
            detected_grant: None,
            worker,
        }
    }

    pub fn permission(&self) -> Permission {
        *self.observed.borrow()
    }

    pub fn check_now(&self) {
        self.wake.notify_one();
    }

    pub fn granted(&mut self) -> Option<Permission> {
        if self.changes.as_ref().is_some_and(PrivacyChanges::changed) {
            // A notification can concern any app. Only a successful probe can trigger a relaunch.
            self.check_now();
        }
        if let Ok(permission) = self.granted.try_recv() {
            self.detected_grant = Some(permission);
        }
        self.detected_grant
    }
}

impl Drop for ApprovalWatch {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Clone, Copy)]
struct ApprovalProgress {
    last: Permission,
    relaunched: Permission,
}

impl ApprovalProgress {
    fn observe(&mut self, permission: Permission) -> Option<Permission> {
        let gained = permission > self.last.max(self.relaunched);
        self.last = permission;
        if permission >= self.relaunched {
            self.relaunched = Permission::Missing;
        }
        gained.then_some(permission)
    }
}

async fn probe_fresh_process(bundle: &Path) -> io::Result<Permission> {
    // Launch the same signed bundle so the check uses a fresh process with the same app identity.
    // The probe exits before initializing AppKit, starting a receiver or requesting permission.
    let output = tempfile::NamedTempFile::new()?;
    let status = timeout(
        Duration::from_secs(8),
        Command::new("/usr/bin/open")
            .args(["-n", "-g", "-j", "--stdout"])
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
    // Read the probe response as soon as it arrives, independently of app termination.
    timeout(Duration::from_secs(3), async {
        loop {
            let answer = fs::read(output.path())?;
            if answer.ends_with(b"\n") {
                return Permission::from_probe(&answer);
            }
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?
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
    let recorded = (|| -> io::Result<bool> {
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
    })();
    // Release the lock explicitly in case a concurrently spawned child inherited
    // the file description before close-on-exec takes effect.
    let unlocked = installed.unlock();
    let recorded = recorded?;
    unlocked?;
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_notification_wakes_a_waiting_check() {
        unsafe extern "C" {
            fn notify_post(name: *const c_char) -> u32;
        }
        let name = std::ffi::CString::new(format!(
            "com.oseanstudio.myremote.test.{}",
            std::process::id()
        ))
        .unwrap();
        let changes = PrivacyChanges::subscribe(&name).unwrap();
        assert!(!changes.changed());
        assert_eq!(unsafe { notify_post(name.as_ptr()) }, 0);
        let until = Instant::now() + Duration::from_secs(2);
        while !changes.changed() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn watches_both_grants_without_relaunching_twice_for_the_same_permission() {
        use Permission::*;

        let mut waiting = ApprovalProgress {
            last: Missing,
            relaunched: Missing,
        };
        assert_eq!(waiting.observe(Missing), None);
        assert_eq!(waiting.observe(Accessibility), Some(Accessibility));

        let mut reopened = ApprovalProgress {
            last: Missing,
            relaunched: Accessibility,
        };
        assert_eq!(reopened.observe(Missing), None);
        assert_eq!(reopened.observe(Accessibility), None);
        assert_eq!(reopened.observe(PostEvents), Some(PostEvents));
    }

    #[test]
    fn grant_after_a_running_app_loses_permissions_triggers_relaunch() {
        use Permission::*;

        let mut running = ApprovalProgress {
            last: PostEvents,
            relaunched: PostEvents,
        };
        assert_eq!(running.observe(PostEvents), None);
        assert_eq!(running.observe(Missing), None);
        assert_eq!(running.observe(PostEvents), Some(PostEvents));
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
