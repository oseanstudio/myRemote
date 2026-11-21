use std::env;
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "macos")]
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use semver::Version;
use tempfile::TempDir;

use super::Result;

#[cfg(target_os = "macos")]
pub const ASSET: &str = "myRemote-macOS-Universal.dmg";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const ASSET: &str = "myRemote-Linux-x86_64.AppImage";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const ASSET: &str = "myRemote-Linux-aarch64.AppImage";
#[cfg(windows)]
pub const ASSET: &str = "myRemote-Windows-x64.zip";

#[cfg(target_os = "macos")]
const PAYLOAD: &str = "myRemote.app";
#[cfg(target_os = "linux")]
const PAYLOAD: &str = "myRemote.AppImage";
#[cfg(windows)]
const PAYLOAD: &str = "myremote.exe";
#[cfg(windows)]
const HELPER: &str = "myremote-update.exe";
#[cfg(unix)]
const HELPER: &str = "myremote-update";
static EXIT_LOCK: OnceLock<File> = OnceLock::new();
#[cfg(target_os = "macos")]
pub const ACCESSIBILITY_RELAUNCH_ARG: &str = "--relaunch-after-accessibility";
#[cfg(target_os = "macos")]
static ACCESSIBILITY_HELPER: OnceLock<std::process::Child> = OnceLock::new();

pub struct Ready {
    pub stage: TempDir,
    pub target: PathBuf,
    pub prepared: PathBuf,
}

pub fn destination() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let executable = env::current_exe()?.canonicalize()?;
        let bundle = executable
            .ancestors()
            .nth(3)
            .filter(|directory| {
                directory
                    .extension()
                    .is_some_and(|extension| extension == "app")
            })
            .ok_or("install myRemote from the DMG before using the updater")?;
        Ok(bundle.to_owned())
    }
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(
            env::var_os("APPIMAGE")
                .ok_or("launch myRemote from its AppImage to use the updater")?,
        )
        .canonicalize()?;
        confirm_appimage(&path)?;
        Ok(path)
    }
    #[cfg(windows)]
    {
        Ok(env::current_exe()?.canonicalize()?)
    }
}

#[cfg(target_os = "linux")]
fn confirm_appimage(path: &Path) -> Result<()> {
    let mut header = [0; 11];
    File::open(path)?.read_exact(&mut header)?;
    if &header[..4] != b"\x7fELF" || &header[8..] != b"AI\x02" {
        return Err("expected a type 2 AppImage".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn prepare(stage: &Path, package: &Path, _version: &Version) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    confirm_appimage(package)?;
    fs::set_permissions(package, fs::Permissions::from_mode(0o755))?;
    let target = stage.join(PAYLOAD);
    // Package and staging file are different names for both Linux architectures.
    fs::rename(package, &target)?;
    Ok(target)
}

#[cfg(windows)]
pub fn prepare(stage: &Path, package: &Path, version: &Version) -> Result<PathBuf> {
    let target = stage.join(PAYLOAD);
    let file = File::create_new(&target)?;
    // Stream the named member into a file we opened. Archive paths cannot write onto the host.
    let status = system_program("tar.exe")
        .arg("-xOf")
        .arg(package)
        .arg("myremote.exe")
        .stdout(file)
        .status()?;
    if !status.success() || fs::metadata(&target)?.len() > super::MAX_PACKAGE {
        return Err("Windows package could not be extracted".into());
    }
    let mut header = [0; 2];
    File::open(&target)?.read_exact(&mut header)?;
    if &header != b"MZ" {
        return Err("invalid Windows executable".into());
    }
    let actual = quiet_command(&target).arg("--version").output()?;
    if !actual.status.success()
        || String::from_utf8(actual.stdout)?.trim() != format!("myRemote {version}")
    {
        return Err("Windows executable version does not match its release".into());
    }
    Ok(target)
}

#[cfg(target_os = "macos")]
pub fn prepare(stage: &Path, package: &Path, version: &Version) -> Result<PathBuf> {
    let mount = stage.join("mnt");
    fs::create_dir(&mount)?;
    command_ok(
        Command::new("/usr/bin/hdiutil").arg("verify").arg(package),
        "disk image verification failed",
    )?;
    command_ok(
        Command::new("/usr/bin/hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(package),
        "could not open the downloaded disk image",
    )?;
    let staged = stage.join(PAYLOAD);
    let copied = command_ok(
        Command::new("/usr/bin/ditto")
            .arg(mount.join(PAYLOAD))
            .arg(&staged),
        "could not copy the downloaded app",
    );
    let detached = command_ok(
        Command::new("/usr/bin/hdiutil").arg("detach").arg(&mount),
        "could not eject the downloaded disk image",
    );
    copied?;
    detached?;
    command_ok(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&staged),
        "downloaded app failed code signature verification",
    )?;
    let plist = staged.join("Contents/Info.plist");
    let read = |key: &str| -> Result<String> {
        let output = Command::new("/usr/libexec/PlistBuddy")
            .arg("-c")
            .arg(format!("Print :{key}"))
            .arg(&plist)
            .output()?;
        if !output.status.success() {
            return Err("invalid app bundle metadata".into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    if read("CFBundleIdentifier")? != "com.oseanstudio.myremote"
        || read("CFBundleShortVersionString")? != version.to_string()
    {
        return Err("app bundle does not match the myRemote release".into());
    }
    Ok(staged)
}

#[cfg(target_os = "macos")]
fn command_ok(command: &mut Command, message: &'static str) -> Result<()> {
    let result = command.output()?;
    if result.status.success() {
        Ok(())
    } else {
        Err(message.into())
    }
}

pub fn launch(ready: Ready) -> Result<()> {
    if ready.prepared != ready.stage.path().join(PAYLOAD) {
        return Err("invalid update staging location".into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(ready.stage.path().join("exit.lock"))?;
    lock.try_lock()?;
    let helper_dir = tempfile::Builder::new()
        .prefix("myremote-helper-")
        .tempdir()?;
    let helper = helper_dir.path().join(HELPER);
    fs::copy(env::current_exe()?, &helper)?;
    quiet_command(&helper)
        .arg("--apply-update")
        .arg(ready.stage.path())
        .arg(&ready.target)
        .args(env::args_os().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Keep the old process's lock until the OS exits it and releases its executable.
    let _ = EXIT_LOCK.set(lock);
    let _ = ready.stage.keep();
    let _ = helper_dir.keep();
    Ok(())
}

pub fn run_helper() -> Result<()> {
    let mut args = env::args_os().skip(2);
    let stage = PathBuf::from(args.next().ok_or("staging directory is missing")?).canonicalize()?;
    let target = PathBuf::from(args.next().ok_or("application path is missing")?).canonicalize()?;
    if stage.parent() != target.parent()
        || !stage
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".myremote-update-"))
    {
        return Err("invalid update staging directory".into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(stage.join("exit.lock"))?;
    lock.lock()?;
    let arguments: Vec<_> = args.collect();
    let candidate = stage.join(PAYLOAD);
    #[cfg(target_os = "macos")]
    let backup = candidate.clone();
    #[cfg(not(target_os = "macos"))]
    let backup = stage.join("previous");
    let install = || -> Result<()> {
        // On macOS the exchange saves the previous bundle at the candidate's path.
        #[cfg(not(target_os = "macos"))]
        fs::hard_link(&target, &backup).or_else(|_| fs::copy(&target, &backup).map(|_| ()))?;
        replace_with_retry(&candidate, &target)?;
        if let Err(error) = relaunch(&target, &arguments) {
            if let Err(restore) = replace_with_retry(&backup, &target) {
                return Err(format!(
                    "relaunch and rollback failed ({error}, {restore}). Previous app saved at {}",
                    backup.display()
                )
                .into());
            }
            return Err(error);
        }
        Ok(())
    };
    let result = install();
    drop(lock);
    if let Err(error) = &result {
        if let Some(directory) = crate::default_state_dir() {
            let _ = fs::create_dir_all(&directory);
            let _ = fs::write(
                directory.join("update-error.txt"),
                format!("Could not install update: {error}"),
            );
        }
        relaunch(&target, &arguments)?;
    }
    // Retain a recoverable backup if the replacement or rollback could not complete.
    if result.is_ok() || !backup.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    #[cfg(unix)]
    remove_own_helper();
    result
}

fn replace_with_retry(candidate: &Path, target: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::raw::{c_char, c_int, c_uint};
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
        }
        let from = CString::new(candidate.as_os_str().as_bytes())?;
        let to = CString::new(target.as_os_str().as_bytes())?;
        // Both bundles are on the same volume. RENAME_SWAP atomically exchanges them.
        if unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 0x00000002) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        for attempt in 0..20 {
            match fs::rename(candidate, target) {
                Ok(()) => return Ok(()),
                Err(error)
                    if cfg!(windows)
                        && attempt < 19
                        && matches!(error.raw_os_error(), Some(5 | 32 | 33)) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("retry loop always finishes")
    }
}

#[cfg(target_os = "macos")]
pub fn prepare_accessibility_relaunch() -> Result<()> {
    destination()?;
    let helper = Command::new(env::current_exe()?)
        .arg(ACCESSIBILITY_RELAUNCH_ARG)
        .args(env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Hold the helper’s stdin writer until the operating system exits this process.
    ACCESSIBILITY_HELPER.set(helper).map_err(|mut helper| {
        let _ = helper.kill();
        let _ = helper.wait();
        "A relaunch is already pending"
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn run_accessibility_relaunch() -> Result<()> {
    let mut signal = [0];
    if io::stdin().read(&mut signal)? != 0 {
        return Err("Invalid relaunch signal".into());
    }
    let arguments: Vec<_> = env::args_os().skip(2).collect();
    relaunch_mac(&destination()?, &arguments, true)
}

#[cfg(target_os = "macos")]
fn relaunch(target: &Path, arguments: &[std::ffi::OsString]) -> Result<()> {
    relaunch_mac(target, arguments, false)
}

#[cfg(target_os = "macos")]
fn relaunch_mac(
    target: &Path,
    arguments: &[std::ffi::OsString],
    after_accessibility_grant: bool,
) -> Result<()> {
    let mut command = Command::new("/usr/bin/open");
    command.arg("-n");
    if after_accessibility_grant {
        command.args(["-g", "--env"]);
        command.arg(format!("{}=1", crate::mac_accessibility::RELAUNCHED_ENV));
    }
    command.arg(target);
    if !arguments.is_empty() {
        command.arg("--args").args(arguments);
    }
    if !command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success()
    {
        return Err("Launch Services could not relaunch myRemote".into());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn relaunch(target: &Path, arguments: &[std::ffi::OsString]) -> Result<()> {
    let mut command = quiet_command(target);
    #[cfg(windows)]
    if let Ok(exe) = env::current_exe() {
        if let Some(directory) = exe.parent() {
            command.env("MYREMOTE_UPDATE_HELPER", directory);
        }
    }
    #[cfg(target_os = "linux")]
    command
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("ARGV0");
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn remove_own_helper() {
    if let Ok(exe) = env::current_exe() {
        if let Some(directory) = exe.parent() {
            if helper_directory(directory) {
                let _ = fs::remove_dir_all(directory);
            }
        }
    }
}

pub fn clean_helper() {
    #[cfg(windows)]
    if let Some(directory) = env::var_os("MYREMOTE_UPDATE_HELPER").map(PathBuf::from) {
        if helper_directory(&directory) {
            std::thread::spawn(move || {
                for _ in 0..20 {
                    if fs::remove_dir_all(&directory).is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
            });
        }
    }
}

fn helper_directory(path: &Path) -> bool {
    let temp = env::temp_dir().canonicalize();
    let actual = path.canonicalize();
    match (temp, actual) {
        (Ok(temp), Ok(actual)) => {
            actual.parent() == Some(&temp)
                && actual
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("myremote-helper-"))
        }
        _ => false,
    }
}

fn quiet_command(program: &Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(program);
        command.creation_flags(0x08000000);
        command
    }
    #[cfg(unix)]
    Command::new(program)
}

#[cfg(windows)]
pub fn system_program(filename: &str) -> Command {
    let path = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join(filename);
    quiet_command(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_only_the_installed_payload() {
        let scratch = tempfile::tempdir().unwrap();
        let from = scratch.path().join("next");
        let to = scratch.path().join("current");
        #[cfg(target_os = "macos")]
        {
            fs::create_dir(&from).unwrap();
            fs::create_dir(&to).unwrap();
            fs::write(from.join("version"), b"next").unwrap();
            fs::write(to.join("version"), b"current").unwrap();
            replace_with_retry(&from, &to).unwrap();
            assert_eq!(fs::read(to.join("version")).unwrap(), b"next");
            assert_eq!(fs::read(from.join("version")).unwrap(), b"current");
        }
        #[cfg(not(target_os = "macos"))]
        {
            fs::write(&from, b"next").unwrap();
            fs::write(&to, b"current").unwrap();
            replace_with_retry(&from, &to).unwrap();
            assert_eq!(fs::read(&to).unwrap(), b"next");
            assert!(!from.exists());
        }
    }
}
