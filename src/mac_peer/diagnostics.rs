use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const CAPTURE: &str = concat!(include_str!("diagnostics.sh"), "\ndiagnostic\n");
const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(75);
const AUTHORIZE: &str = r#"
on run argv
    set commandText to "/usr/bin/env -i LC_ALL=C /bin/bash -c " & quoted form of (item 1 of argv)
    set resultText to do shell script commandText with administrator privileges without altering line endings
    return resultText
end run
"#;

/// A canceled authorization returns no report.
pub async fn capture() -> io::Result<Option<String>> {
    // Only the embedded script crosses the privilege boundary. It runs with a clean
    // environment and prints four counts without writing a packet capture to disk.
    let mut authorize = Command::new("/usr/bin/osascript");
    authorize
        .env_clear()
        .current_dir("/")
        .args(["-e", AUTHORIZE, CAPTURE])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let result = timeout(DIAGNOSTIC_DEADLINE, authorize.output())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "The check timed out. Start it again and approve the macOS administrator prompt if it appears.",
            )
        })??;
    if !result.status.success() {
        let error = String::from_utf8_lossy(&result.stderr);
        if error.contains("(-128)") {
            return Ok(None);
        }
        if error.contains("No address on awdl0") {
            return Err(io::Error::other(
                "The Mac’s direct Wi-Fi interface is unavailable. Check that Wi-Fi is turned on.",
            ));
        }
        tracing::debug!("Nearby capture failed: {error}");
        return Err(io::Error::other("macOS could not complete the diagnostic."));
    }
    Ok(Some(report(&result.stdout)?))
}

fn report(output: &[u8]) -> io::Result<String> {
    let invalid = || io::Error::other("The diagnostic returned an invalid summary.");
    let text = std::str::from_utf8(output).map_err(|_| invalid())?;
    let counts = text
        .split_ascii_whitespace()
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    let [packets, peer_queries, mac_queries, answers] = counts.as_slice() else {
        return Err(invalid());
    };
    Ok(format!(
        "myRemote {} · 30-second Nearby check\n\nDirect Wi-Fi mDNS packets: {packets}\nCompanion queries from peers: {peer_queries}\nCompanion queries from this Mac: {mac_queries}\nNearby replies or announcements from this Mac: {answers}",
        crate::version_label(crate::VERSION)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn summarizes_only_the_expected_packets() {
        let script = concat!(include_str!("diagnostics.sh"), "\nsummarize \"$1\"\n");
        let mut test = Command::new("/bin/bash")
            .args(["-c", script, "nearby", "fe80::1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        test.stdin.take().unwrap().write_all(b"\
IP6 (hlim 255, next-header UDP)\n\
    fe80::2.5353 > ff02::fb.5353: 0 PTR (QM)? _companion-link._tcp.local.\n\
    fe80::1%awdl0.5353 > ff02::fb.5353: 0 PTR (QU)? _companion-link._tcp.local.\n\
    fe80::1.5353 > ff02::fb.5353: 0*- [0q] 1/0/0 _companion-link._tcp.local. PTR Mac-Nearby._companion-link._tcp.local.\n\
    fe80::3.5353 > ff02::fb.5353: 0*- 1/0/0 _companion-link._tcp.local. PTR Other-Nearby._companion-link._tcp.local.\n\
    fe80::2.5353 > ff02::fb.5353: 0 PTR (QM)? _airdrop._tcp.local.\n").unwrap();
        let output = test.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"5 1 1 1\n");
        let formatted = report(&output.stdout).unwrap();
        assert!(formatted.contains("Companion queries from peers: 1"));
        assert!(formatted.contains("Nearby replies or announcements from this Mac: 1"));
        assert!(report(b"5 1 1 1 unexpected").is_err());
    }

    #[tokio::test]
    async fn returns_counts_when_the_capture_ignores_termination() {
        let script = concat!(
            include_str!("diagnostics.sh"),
            r#"
capture 1 /bin/bash -c '
    trap "" TERM
    printf "%s\n" "fe80::2.5353 > ff02::fb.5353: 0 PTR (QM)? _companion-link._tcp.local."
    exec /bin/sleep 30
' | summarize "fe80::1"
"#,
        );
        let output = timeout(
            Duration::from_secs(6),
            tokio::process::Command::new("/bin/bash")
                .args(["-c", script])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("capture must close even when the child ignores TERM")
        .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, b"1 1 0 0\n");
    }

    #[test]
    fn compiles_the_native_authorization_prompt() {
        let temporary = tempfile::tempdir().unwrap();
        let output = Command::new("/usr/bin/osacompile")
            .arg("-o")
            .arg(temporary.path().join("authorize.scpt"))
            .args(["-e", AUTHORIZE])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = AUTHORIZE.replace(" with administrator privileges", "");
        let result = Command::new("/usr/bin/osascript")
            .env_clear()
            .current_dir("/")
            .args(["-e", &script, "printf '%s\\n' '1 0 0 0'"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(report(&result.stdout)
            .unwrap()
            .contains("Direct Wi-Fi mDNS packets: 1"));
    }
}
