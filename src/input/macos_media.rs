use std::ffi::c_void;
use std::ptr::null;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;

use super::platform::{can_post_input, send_key_to, Desktop, OwnedCf};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCopyAttributeValues(
        element: *const c_void,
        attribute: *const c_void,
        index: isize,
        max_values: isize,
        values: *mut *const c_void,
    ) -> i32;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFRetain(object: *const c_void) -> *const c_void;
    fn CFEqual(first: *const c_void, second: *const c_void) -> bool;
    static kCFBooleanTrue: *const c_void;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Player {
    Browser,
    Mpv,
    QuickTime,
    Vlc,
    AppleTv,
}

impl Player {
    fn identify(bundle: &str) -> Option<Self> {
        Some(match bundle {
            "com.apple.Safari"
            | "com.apple.SafariTechnologyPreview"
            | "com.google.Chrome"
            | "org.chromium.Chromium"
            | "org.mozilla.firefox"
            | "com.microsoft.edgemac"
            | "com.brave.Browser"
            | "company.thebrowser.Browser"
            | "com.vivaldi.Vivaldi" => Self::Browser,
            "com.colliderli.iina" | "io.mpv" => Self::Mpv,
            "com.apple.QuickTimePlayerX" => Self::QuickTime,
            "org.videolan.vlc" => Self::Vlc,
            "com.apple.TV" => Self::AppleTv,
            _ => return None,
        })
    }

    fn shortcut(self, action: &str) -> Option<(u16, u64)> {
        const CMD: u64 = 1 << 20;
        const CTRL: u64 = 1 << 18;
        const SHIFT: u64 = 1 << 17;
        const SPACE: u16 = 49;
        const F: u16 = 3;
        const L: u16 = 37;
        const RIGHT: u16 = 124;
        Some(match (action, self) {
            ("play_pause", _) => (SPACE, 0),
            ("home", Self::QuickTime) => (F, CMD),
            ("home", Self::AppleTv) => (F, CTRL | CMD),
            ("home", _) => (F, 0),
            ("menu", Self::Browser) => (L, 0),
            ("menu", Self::Mpv | Self::AppleTv) => (RIGHT, 0),
            ("menu", Self::Vlc) => (RIGHT, SHIFT),
            _ => return None,
        })
    }
}

pub(super) fn handle(action: &str) -> bool {
    if !matches!(action, "play_pause" | "home" | "menu") || !can_post_input() {
        return false;
    }
    autoreleasepool(|_| {
        let workspace = NSWorkspace::sharedWorkspace();
        let app = workspace.frontmostApplication()?;
        let player = Player::identify(&app.bundleIdentifier()?.to_string())?;
        let (key, flags) = player.shortcut(action)?;
        if Desktop::focused_text_field().is_some() {
            // A browser search box should keep its normal Escape behavior.
            return Some(action != "menu");
        }
        let pid = app.processIdentifier();
        if action == "menu" && player == Player::Browser {
            if let Some(button) = intro_button(pid) {
                let press = OwnedCf::string(c"AXPress")?;
                if app.isActive() && unsafe { AXUIElementPerformAction(button.0, press.0) } == 0 {
                    return Some(true);
                }
            }
        }
        if app.isActive() {
            send_key_to(pid, key, flags, &[]);
        }
        Some(true)
    })
    .unwrap_or(false)
}

fn intro_button(pid: i32) -> Option<OwnedCf> {
    let app = OwnedCf::new(unsafe { AXUIElementCreateApplication(pid) })?;
    let window = app.attribute(c"AXFocusedWindow").ok()?;
    let children = OwnedCf::string(c"AXChildren")?;
    let mut pending = vec![window];
    let deadline = Instant::now() + Duration::from_millis(200);
    for _ in 0..200 {
        if Instant::now() >= deadline {
            break;
        }
        let element = pending.pop()?;
        let is_button = element
            .attribute(c"AXRole")
            .is_ok_and(|role| role.is(c"AXButton"));
        if is_button {
            let has_label = |name| {
                element
                    .attribute(name)
                    .is_ok_and(|label| label.is(c"Skip Intro") || label.is(c"Skip intro"))
            };
            let has_flag = |name| {
                element
                    .attribute(name)
                    .is_ok_and(|value| unsafe { CFEqual(value.0, kCFBooleanTrue) })
            };
            if (has_label(c"AXTitle") || has_label(c"AXDescription"))
                && has_flag(c"AXEnabled")
                && !has_flag(c"AXHidden")
            {
                return Some(element);
            }
            continue;
        }
        let mut result = null();
        let status =
            unsafe { AXUIElementCopyAttributeValues(element.0, children.0, 0, 64, &mut result) };
        if let Some(array) = OwnedCf::new(result).filter(|_| status == 0) {
            for index in 0..unsafe { CFArrayGetCount(array.0) } {
                let child = unsafe { CFArrayGetValueAtIndex(array.0, index) };
                if !child.is_null() {
                    pending.extend(OwnedCf::new(unsafe { CFRetain(child) }));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::Player;

    #[test]
    fn browser_uses_player_keys_instead_of_global_media_keys() {
        let browser = Player::identify("com.apple.Safari").unwrap();
        assert_eq!(browser.shortcut("play_pause"), Some((49, 0)));
        assert_eq!(browser.shortcut("home"), Some((3, 0)));
        assert_eq!(browser.shortcut("menu"), Some((37, 0)));
        assert!(Player::identify("com.spotify.client").is_none());
    }
}
