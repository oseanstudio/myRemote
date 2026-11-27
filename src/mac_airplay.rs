use objc2_core_foundation::{
    kCFPreferencesCurrentHost, kCFPreferencesCurrentUser, CFBoolean, CFNumber,
    CFPreferencesAppSynchronize, CFPreferencesAppValueIsForced, CFPreferencesCopyAppValue,
    CFPreferencesSetValue, CFPreferencesSynchronize, CFPropertyList, CFString,
};
use objc2_foundation::NSProcessInfo;

const RECEIVER: &str = "AirplayReceiverEnabled";
const LEGACY_RECEIVER: &str = "AirplayRecieverEnabled";

pub struct ReceiverSetting {
    pub enabled: bool,
    pub editable: bool,
}

pub fn setting() -> ReceiverSetting {
    let restrictions = CFString::from_static_str("com.apple.applicationaccess");
    CFPreferencesAppSynchronize(&restrictions);
    if preference(&restrictions, "allowAirPlayIncomingRequests") == Some(false) {
        return ReceiverSetting {
            enabled: false,
            editable: false,
        };
    }
    let domain = CFString::from_static_str("com.apple.controlcenter");
    CFPreferencesAppSynchronize(&domain);
    let keys = receiver_keys(
        NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion,
    );
    ReceiverSetting {
        // AirPlay Receiver defaults to on on Macs that support AirPlay to Mac.
        enabled: keys
            .iter()
            .find_map(|key| preference(&domain, key))
            .unwrap_or(true),
        editable: !keys
            .iter()
            .any(|key| CFPreferencesAppValueIsForced(&CFString::from_static_str(key), &domain)),
    }
}

pub fn set_enabled(enabled: bool) -> bool {
    let domain = CFString::from_static_str("com.apple.controlcenter");
    store(&domain, Some(CFBoolean::new(enabled))) && setting().enabled == enabled
}

fn receiver_keys(os_major: isize) -> [&'static str; 2] {
    if os_major >= 26 {
        [RECEIVER, LEGACY_RECEIVER]
    } else {
        [LEGACY_RECEIVER, RECEIVER]
    }
}

fn preference(domain: &CFString, key: &'static str) -> Option<bool> {
    let value = CFPreferencesCopyAppValue(&CFString::from_static_str(key), domain)?;
    value
        .downcast_ref::<CFBoolean>()
        .map(CFBoolean::as_bool)
        .or_else(|| {
            value
                .downcast_ref::<CFNumber>()?
                .as_i64()
                .map(|number| number != 0)
        })
}

fn store(domain: &CFString, enabled: Option<&CFPropertyList>) -> bool {
    // Before macOS 26, Control Center used the misspelled key. Keep both spellings
    // in the current-host domain so the preference also survives an OS upgrade.
    // Both constants are Apple's special CFPreferences selectors.
    let (user, host) = unsafe { (kCFPreferencesCurrentUser, kCFPreferencesCurrentHost) };
    for key in [RECEIVER, LEGACY_RECEIVER] {
        // The only values stored here are CFBooleans or removal of a test key.
        unsafe {
            CFPreferencesSetValue(&CFString::from_static_str(key), enabled, domain, user, host);
        }
    }
    CFPreferencesSynchronize(domain, user, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn airplay_toggle_persists_both_states_for_the_current_host() {
        let name = format!(
            "com.oseanstudio.myremote.airplay-test.{:016x}",
            rand::random::<u64>()
        );
        let domain = CFString::from_str(&name);
        let observations = [true, false].map(|enabled| {
            let saved = store(&domain, Some(CFBoolean::new(enabled)));
            let persisted = [RECEIVER, LEGACY_RECEIVER].map(|key| {
                let read = Command::new("/usr/bin/defaults")
                    .args(["-currentHost", "read", &name, key])
                    .output();
                (preference(&domain, key), read)
            });
            (enabled, saved, persisted)
        });
        assert!(store(&domain, None));
        for (enabled, saved, persisted) in observations {
            assert!(saved);
            for (readback, output) in persisted {
                assert_eq!(readback, Some(enabled));
                let output = output.unwrap();
                assert!(output.status.success());
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap().trim(),
                    if enabled { "1" } else { "0" }
                );
            }
        }
    }
}
