use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::input;

const PROBE_ARG: &str = "--probe-input-permission";
pub const RELAUNCHED_ENV: &str = "MYREMOTE_ACCESSIBILITY_RELAUNCHED";
const WATCH_TIME: Duration = Duration::from_secs(5 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(3);

pub fn run_probe() -> Option<ExitCode> {
    if !env::args_os().nth(1).is_some_and(|arg| arg == PROBE_ARG) {
        return None;
    }
    println!(
        "{}",
        if input::can_post_input() {
            "granted"
        } else {
            "denied"
        }
    );
    Some(ExitCode::SUCCESS)
}

pub struct ApprovalWatch {
    pub bundle: PathBuf,
    deadline: Instant,
    granted: oneshot::Receiver<()>,
    worker: JoinHandle<()>,
}

impl ApprovalWatch {
    pub fn start(bundle: PathBuf, runtime: &Runtime) -> Self {
        let deadline = Instant::now() + WATCH_TIME;
        let (notify, granted) = oneshot::channel();
        let worker = runtime.spawn({
            let bundle = bundle.clone();
            async move {
                let watch = async {
                    loop {
                        sleep(PROBE_INTERVAL).await;
                        if probe_fresh_process(&bundle).await? {
                            let _ = notify.send(());
                            return Ok::<_, io::Error>(());
                        }
                    }
                };
                if let Ok(Err(error)) = timeout(WATCH_TIME, watch).await {
                    tracing::debug!("Could not check Accessibility in a fresh process: {error}");
                }
            }
        });
        Self {
            bundle,
            deadline,
            granted,
            worker,
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn granted(&mut self) -> bool {
        input::can_post_input() || self.granted.try_recv().is_ok()
    }
}

impl Drop for ApprovalWatch {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn probe_fresh_process(bundle: &Path) -> io::Result<bool> {
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
    match std::fs::read(output.path())?.as_slice() {
        b"granted\n" => Ok(true),
        b"denied\n" => Ok(false),
        _ => Err(io::Error::other("Unexpected permission check response")),
    }
}
