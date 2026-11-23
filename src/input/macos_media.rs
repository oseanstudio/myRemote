use std::ffi::{c_char, c_void, CStr};
use std::ptr::null;
use std::time::{Duration, Instant};

use myremote::{PlaybackAction, PlaybackState, VideoSource};
use objc2::rc::{autoreleasepool, Retained};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};

use super::platform::{can_post_input, send_key_to, Desktop, OwnedCf};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCreateSystemWide() -> *const c_void;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
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
    static kCFBooleanFalse: *const c_void;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFURLGetTypeID() -> usize;
    fn CFURLGetString(url: *const c_void) -> *const c_void;
    fn CFStringGetCString(s: *const c_void, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> u8;
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

struct BrowserVideo {
    app: Retained<NSRunningApplication>,
    page: OwnedCf,
    source: VideoSource,
}

impl BrowserVideo {
    fn foreground() -> Option<Self> {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        if Player::identify(&app.bundleIdentifier()?.to_string()) != Some(Player::Browser) {
            return None;
        }
        if let Some(system) = OwnedCf::new(unsafe { AXUIElementCreateSystemWide() }) {
            unsafe { AXUIElementSetMessagingTimeout(system.0, 0.1) };
        }
        let page = find_element(focused_window(app.processIdentifier())?, |element| {
            element
                .attribute(c"AXRole")
                .is_ok_and(|role| role.is(c"AXWebArea"))
                && flag(element, c"AXHidden") != Some(true)
        })?;
        let source = video_source(&text(&page.attribute(c"AXURL").ok()?)?)?;
        app.isActive().then_some(Self { app, page, source })
    }

    fn state(self) -> PlaybackState {
        let mut state = PlaybackState {
            source: self.source,
            playing: None,
            captions: None,
        };
        find_element(self.page, |element| {
            if !element.attribute(c"AXRole").is_ok_and(|role| {
                role.is(c"AXButton") || role.is(c"AXCheckBox") || role.is(c"AXToggleButton")
            }) || flag(element, c"AXHidden") == Some(true)
                || flag(element, c"AXEnabled") == Some(false)
            {
                return false;
            }
            for attribute in [c"AXTitle", c"AXDescription", c"AXHelp"] {
                let label = element
                    .attribute(attribute)
                    .ok()
                    .and_then(|value| text(&value));
                let Some(label) = label else { continue };
                match label.trim().to_ascii_lowercase().as_str() {
                    "pause" | "pause (k)" if state.playing.is_none() => state.playing = Some(true),
                    "play" | "play (k)" if state.playing.is_none() => state.playing = Some(false),
                    label
                        if state.source == VideoSource::YouTube
                            && state.captions.is_none()
                            && (label.contains("closed captions") || label == "subtitles (c)") =>
                    {
                        state.captions =
                            flag(element, c"AXValue").or_else(|| flag(element, c"AXPressed"));
                    }
                    _ => {}
                }
            }
            state.playing.is_some()
                && (state.source == VideoSource::Jellyfin || state.captions.is_some())
        });
        state
    }
}

pub(super) fn playback_state() -> Option<PlaybackState> {
    if !can_post_input() {
        return None;
    }
    autoreleasepool(|_| BrowserVideo::foreground().map(BrowserVideo::state))
}

pub(super) fn playback_action(source: VideoSource, action: PlaybackAction) -> bool {
    if !can_post_input() || Desktop::focused_text_field().is_some() {
        return false;
    }
    autoreleasepool(|_| {
        let video = BrowserVideo::foreground().filter(|video| video.source == source)?;
        let app = video.app.clone();
        let key = match action {
            PlaybackAction::SkipForward => 37,  // L
            PlaybackAction::SkipBackward => 38, // J
            PlaybackAction::Play | PlaybackAction::Pause => {
                let playing = video.state().playing?;
                if playing == (action == PlaybackAction::Play) {
                    return Some(true);
                }
                40 // K
            }
            PlaybackAction::ToggleCaptions | PlaybackAction::SetCaptions(_) => {
                let enabled = video.state().captions?;
                if action == PlaybackAction::SetCaptions(enabled) {
                    return Some(true);
                }
                8 // C
            }
        };
        if !app.isActive() || Desktop::focused_text_field().is_some() {
            return None;
        }
        send_key_to(app.processIdentifier(), key, 0, &[]);
        Some(true)
    })
    .unwrap_or(false)
}

fn video_source(url: &str) -> Option<VideoSource> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, address) = rest.split_at(host_end);
    let host = authority
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .to_ascii_lowercase();
    let (page, fragment) = address.split_once('#').unwrap_or((address, ""));
    let (path, query) = page.split_once('?').unwrap_or((page, ""));
    if host == "youtube.com" || host.ends_with(".youtube.com") {
        let has_video = query
            .split('&')
            .any(|pair| pair.strip_prefix("v=").is_some_and(|id| !id.is_empty()));
        return (path == "/watch" && has_video
            || path
                .strip_prefix("/embed/")
                .is_some_and(|id| !id.is_empty()))
        .then_some(VideoSource::YouTube);
    }
    let route = fragment.trim_start_matches(['!', '/']).split('?').next()?;
    (path.split('/').any(|part| part == "web")
        && (matches!(route, "video" | "videoosd.html")
            || route.ends_with("/videoosd.html")
            || path.ends_with("/video")
            || path.ends_with("/videoosd.html")))
    .then_some(VideoSource::Jellyfin)
}

fn text(value: &OwnedCf) -> Option<String> {
    unsafe {
        let kind = CFGetTypeID(value.0);
        let string = if kind == CFURLGetTypeID() {
            CFURLGetString(value.0)
        } else if kind == CFStringGetTypeID() {
            value.0
        } else {
            return None;
        };
        let mut buffer = [0; 4096];
        if string.is_null()
            || CFStringGetCString(
                string,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                0x0800_0100,
            ) == 0
        {
            return None;
        }
        CStr::from_ptr(buffer.as_ptr())
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

fn flag(element: &OwnedCf, name: &CStr) -> Option<bool> {
    let value = element.attribute(name).ok()?;
    unsafe {
        if CFEqual(value.0, kCFBooleanTrue) {
            return Some(true);
        }
        if CFEqual(value.0, kCFBooleanFalse) {
            return Some(false);
        }
        let mut number = 0_i32;
        if CFGetTypeID(value.0) == CFNumberGetTypeID()
            && CFNumberGetValue(value.0, 3, (&mut number as *mut i32).cast()) != 0
        {
            return match number {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            };
        }
        None
    }
}

fn focused_window(pid: i32) -> Option<OwnedCf> {
    let app = OwnedCf::new(unsafe { AXUIElementCreateApplication(pid) })?;
    app.attribute(c"AXFocusedWindow").ok()
}

fn intro_button(pid: i32) -> Option<OwnedCf> {
    find_element(focused_window(pid)?, |element| {
        let has_label = |name| {
            element
                .attribute(name)
                .is_ok_and(|label| label.is(c"Skip Intro") || label.is(c"Skip intro"))
        };
        element
            .attribute(c"AXRole")
            .is_ok_and(|role| role.is(c"AXButton"))
            && (has_label(c"AXTitle") || has_label(c"AXDescription"))
            && flag(element, c"AXEnabled") == Some(true)
            && flag(element, c"AXHidden") != Some(true)
    })
}

fn find_element(root: OwnedCf, mut matches: impl FnMut(&OwnedCf) -> bool) -> Option<OwnedCf> {
    let children = OwnedCf::string(c"AXChildren")?;
    let mut pending = vec![root];
    let deadline = Instant::now() + Duration::from_millis(200);
    for _ in 0..200 {
        if Instant::now() >= deadline {
            break;
        }
        let element = pending.pop()?;
        if matches(&element) {
            return Some(element);
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
    use super::{video_source, Player, VideoSource};

    #[test]
    fn browser_uses_player_keys_instead_of_global_media_keys() {
        let browser = Player::identify("com.apple.Safari").unwrap();
        assert_eq!(browser.shortcut("play_pause"), Some((49, 0)));
        assert_eq!(browser.shortcut("home"), Some((3, 0)));
        assert_eq!(browser.shortcut("menu"), Some((37, 0)));
        assert!(Player::identify("com.spotify.client").is_none());
    }

    #[test]
    fn video_detection_requires_a_player_route() {
        assert_eq!(
            video_source("https://www.youtube.com/watch?v=demo"),
            Some(VideoSource::YouTube)
        );
        assert_eq!(
            video_source("http://example.test:8096/jellyfin/web/index.html#!/video"),
            Some(VideoSource::Jellyfin)
        );
        assert_eq!(
            video_source("https://video.test/web/#/video"),
            Some(VideoSource::Jellyfin)
        );
        for page in [
            "https://youtube.com/",
            "https://youtube.com/watch",
            "https://youtube.com.example.test/watch?v=demo",
            "https://youtube.com@example.test/watch?v=demo",
            "https://example.test/web/#/home",
        ] {
            assert_eq!(video_source(page), None, "{page}");
        }
    }
}
