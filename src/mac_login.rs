use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use objc2::rc::Retained;
use objc2_foundation::{ns_string, NSError, NSUserDefaults};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

pub struct LoginItem(Retained<SMAppService>);

impl LoginItem {
    pub fn for_installed_app() -> Option<Self> {
        // A login item opens the app without command-line options. Only
        // register the bundled app opened in its normal menu mode.
        if std::env::args_os().nth(1).is_some() {
            return None;
        }
        let bundle = crate::update::app_bundle_path().ok()?;
        if !can_register(&bundle) {
            return None;
        }
        // The fully installed, running bundle is the main application service.
        Some(Self(unsafe { SMAppService::mainAppService() }))
    }

    pub fn status(&self) -> SMAppServiceStatus {
        // This queries the retained service for the current user.
        unsafe { self.0.status() }
    }

    pub fn register_default(&self) {
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = ns_string!("DidSetUpLoginItem");
        if defaults.boolForKey(key) {
            return;
        }
        if self.status() == SMAppServiceStatus::NotRegistered {
            if let Err(error) = self.register() {
                if self.status() != SMAppServiceStatus::RequiresApproval {
                    tracing::warn!("Could not register myRemote as a login item: {error:?}");
                    return;
                }
            }
        }
        if matches!(
            self.status(),
            SMAppServiceStatus::Enabled | SMAppServiceStatus::RequiresApproval
        ) {
            // Once macOS has a record, preserve user changes in Login Items.
            defaults.setBool_forKey(true, key);
        }
    }

    pub fn toggle(&self) -> Result<(), Retained<NSError>> {
        match self.status() {
            SMAppServiceStatus::RequiresApproval => {
                // The user selected the pending item, so take them to the
                // native approval control.
                unsafe { SMAppService::openSystemSettingsLoginItems() };
                return Ok(());
            }
            // Main-app unregistration leaves the current app running.
            SMAppServiceStatus::Enabled => unsafe { self.0.unregisterAndReturnError()? },
            _ => self.register()?,
        }
        NSUserDefaults::standardUserDefaults()
            .setBool_forKey(true, ns_string!("DidSetUpLoginItem"));
        Ok(())
    }

    fn register(&self) -> Result<(), Retained<NSError>> {
        // SMAppService registers the calling bundle and enforces user approval.
        unsafe { self.0.registerAndReturnError() }
    }
}

fn can_register(path: &Path) -> bool {
    if path
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
        || std::env::temp_dir()
            .canonicalize()
            .is_ok_and(|temp| path.starts_with(temp))
    {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut volume = MaybeUninit::<libc::statfs>::uninit();
    // statfs writes its result only on success. A mounted, read-only DMG
    // cannot supply the app after the user ejects it.
    unsafe {
        libc::statfs(path.as_ptr(), volume.as_mut_ptr()) == 0
            && volume.assume_init().f_flags & libc::MNT_RDONLY as u32 == 0
    }
}
