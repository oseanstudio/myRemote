use std::ffi::{c_char, c_void};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

type Event = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    fn AXUIElementCreateSystemWide() -> *const c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: *const c_void,
        attribute: *const c_void,
        settable: *mut u8,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    fn AXUIElementGetPid(element: *const c_void, pid: *mut i32) -> i32;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFEqual(first: *const c_void, second: *const c_void) -> bool;
    fn CFHash(object: *const c_void) -> usize;
    fn CFRelease(object: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *mut c_void) -> Event;
    fn CGEventGetLocation(event: Event) -> Point;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> Event;
    fn CGEventKeyboardSetUnicodeString(event: Event, length: usize, text: *const u16);
    fn CGEventCreateMouseEvent(source: *mut c_void, kind: u32, point: Point, button: u32) -> Event;
    fn CGEventPost(tap: u32, event: Event);
    fn CGEventSetFlags(event: Event, flags: u64);
    fn CGEventSetIntegerValueField(event: Event, field: u32, value: i64);
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

#[link(name = "AppKit", kind = "framework")]
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

pub struct Desktop {
    mouse_number: i64,
    drag_number: Option<i64>,
    pointer: Option<(Instant, Point)>,
}

pub(super) struct OwnedCf(pub(super) *const c_void);

impl OwnedCf {
    pub(super) fn new(value: *const c_void) -> Option<Self> {
        (!value.is_null()).then(|| Self(value))
    }

    pub(super) fn string(value: &std::ffi::CStr) -> Option<Self> {
        const UTF8: u32 = 0x0800_0100;
        Self::new(unsafe { CFStringCreateWithCString(null(), value.as_ptr(), UTF8) })
    }

    pub(super) fn attribute(&self, name: &std::ffi::CStr) -> Result<Self, i32> {
        let name = Self::string(name).ok_or(-1)?;
        let mut value = null();
        let status = unsafe { AXUIElementCopyAttributeValue(self.0, name.0, &mut value) };
        let value = Self::new(value);
        if status != 0 {
            return Err(status);
        }
        value.ok_or(-1)
    }

    pub(super) fn is(&self, name: &std::ffi::CStr) -> bool {
        Self::string(name).is_some_and(|name| unsafe { CFEqual(self.0, name.0) })
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

fn focused_text_field() -> Option<u64> {
    const AX_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
    const AX_NO_VALUE: i32 = -25212;

    if !can_post_input() || !accessibility_is_trusted() {
        return None;
    }
    let system = OwnedCf::new(unsafe { AXUIElementCreateSystemWide() })?;
    // Avoid blocking input for long periods if the frontmost app is unresponsive.
    unsafe { AXUIElementSetMessagingTimeout(system.0, 0.1) };
    let field = system.attribute(c"AXFocusedUIElement").ok()?;
    let role = field.attribute(c"AXRole").ok()?;
    if !role.is(c"AXTextField") && !role.is(c"AXTextArea") && !role.is(c"AXComboBox") {
        return None;
    }
    let secure = match field.attribute(c"AXSubrole") {
        Ok(subrole) => subrole.is(c"AXSecureTextField"),
        Err(AX_ATTRIBUTE_UNSUPPORTED | AX_NO_VALUE) => false,
        _ => return None,
    };
    // A password field may accept typing while protecting its AXValue.
    // Use the normal event-posting API without reading or setting that value.
    if !secure {
        let value = OwnedCf::string(c"AXValue")?;
        let mut editable = 0;
        if unsafe { AXUIElementIsAttributeSettable(field.0, value.0, &mut editable) } != 0
            || editable == 0
        {
            return None;
        }
    }
    let mut pid = 0;
    if unsafe { AXUIElementGetPid(field.0, &mut pid) } != 0 || pid <= 0 {
        return None;
    }
    let mut id = DefaultHasher::new();
    pid.hash(&mut id);
    unsafe { CFHash(field.0) }.hash(&mut id);
    secure.hash(&mut id);
    Some(id.finish())
}

pub(super) fn post_key(key: u16, flags: u64, text: &[u16]) -> bool {
    const SESSION_EVENT_TAP: u32 = 1;
    for down in [true, false] {
        unsafe {
            let event = CGEventCreateKeyboardEvent(null_mut(), key, down);
            if event.is_null() {
                return false;
            }
            CGEventSetFlags(event, flags);
            if !text.is_empty() {
                CGEventKeyboardSetUnicodeString(event, text.len(), text.as_ptr());
            }
            CGEventPost(SESSION_EVENT_TAP, event);
            CFRelease(event);
        }
    }
    true
}

fn post_text_graphemes(text: &str, mut post: impl FnMut(&[u16])) {
    // Quartz text events work best with one visible character per event and
    // accept up to 20 UTF-16 units. Keep combining marks and emoji together.
    let mut units = [0; 20];
    for grapheme in text.graphemes(true) {
        let grapheme = if grapheme == "\r\n" { "\n" } else { grapheme };
        if grapheme != "\n" && grapheme.chars().any(char::is_control) {
            continue;
        }
        let length = grapheme.encode_utf16().count();
        if length <= units.len() {
            for (slot, unit) in units.iter_mut().zip(grapheme.encode_utf16()) {
                *slot = unit;
            }
            post(&units[..length]);
        } else {
            for character in grapheme.chars() {
                post(character.encode_utf16(&mut units));
            }
        }
    }
}

pub fn can_post_input() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}

pub fn accessibility_is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn request_accessibility_entry() -> bool {
    // The dictionary refers only to process-lifetime Core Foundation constants.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(null(), keys.as_ptr(), values.as_ptr(), 1, null(), null());
        if options.is_null() {
            return AXIsProcessTrusted();
        }
        let trusted = AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
        trusted
    }
}

pub fn request_input_permission() {
    unsafe { CGRequestPostEventAccess() };
}

impl Desktop {
    pub fn open() -> io::Result<Self> {
        Ok(Self {
            mouse_number: 0,
            drag_number: None,
            pointer: None,
        })
    }

    pub fn focused_text_field() -> Option<u64> {
        focused_text_field()
    }

    pub fn type_text(field: u64, backspaces: u8, deletes: u8, text: &str) -> bool {
        if focused_text_field() != Some(field) {
            return false;
        }
        let mut posted = true;
        for _ in 0..backspaces {
            posted &= post_key(51, 0, &[]);
        }
        for _ in 0..deletes {
            posted &= post_key(117, 0, &[]);
        }
        match text {
            "\n" | "\r" | "\r\n" => posted &= post_key(36, 0, &[]),
            "\t" => posted &= post_key(48, 0, &[]),
            _ => post_text_graphemes(text, |units| {
                posted &= if units == [u16::from(b'\n')] {
                    post_key(36, 0, &[])
                } else {
                    post_key(49, 0, units)
                };
            }),
        }
        posted
    }

    pub fn button(&mut self, name: &str) {
        if !can_post_input() {
            return;
        }
        let key = match name {
            "up" => 126,
            "down" => 125,
            "left" => 123,
            "right" => 124,
            "select" => 36,
            "menu" => 53,
            "home" => 115,
            "channel_up" => 116,
            "channel_down" => 121,
            "guide" => 48,
            "volume_up" => return media_key(0),
            "volume_down" => return media_key(1),
            "mute" => return media_key(7),
            "play_pause" => return media_key(16),
            _ => return,
        };
        for down in [true, false] {
            // The returned Core Foundation object is owned by this call.
            post_owned(unsafe { CGEventCreateKeyboardEvent(null_mut(), key, down) });
        }
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32, dragging: bool) {
        if !can_post_input() {
            return;
        }
        if let Some((sampled_at, point)) = self.pointer_sample() {
            let next = Point {
                x: point.x + f64::from(dx),
                y: point.y + f64::from(dy),
            };
            let kind = if dragging { 6 } else { 5 };
            if post_mouse(kind, next, 0, self.drag_number, None) {
                self.pointer = Some((sampled_at, next));
            }
        }
    }

    pub fn click(&mut self, count: i64) {
        if !can_post_input() {
            return;
        }
        if let Some((_, point)) = self.pointer_sample() {
            let number = self.next_mouse_event();
            for kind in [1, 2] {
                post_mouse(kind, point, 0, Some(number), Some(count));
            }
        }
    }

    pub fn right_click(&mut self) {
        if can_post_input() {
            if let Some((_, point)) = self.pointer_sample() {
                let number = self.next_mouse_event();
                for kind in [3, 4] {
                    post_mouse(kind, point, 1, Some(number), Some(1));
                }
            }
        }
    }

    pub fn start_drag(&mut self) -> bool {
        if !can_post_input() {
            return false;
        }
        let Some((_, point)) = self.pointer_sample() else {
            return false;
        };
        let number = self.next_mouse_event();
        if post_mouse(1, point, 0, Some(number), Some(1)) {
            self.drag_number = Some(number);
            true
        } else {
            false
        }
    }

    pub fn end_drag(&mut self) {
        let number = self.drag_number.take();
        if let Some((_, point)) = self.pointer_sample() {
            post_mouse(2, point, 0, number, Some(1));
        }
    }

    fn next_mouse_event(&mut self) -> i64 {
        self.mouse_number = (self.mouse_number + 1) % i64::from(i32::MAX);
        self.mouse_number
    }

    fn pointer_sample(&self) -> Option<(Instant, Point)> {
        // During a short burst, Quartz may not have applied the previous move
        // yet. Accumulate every delta and periodically resync for local input
        // and screen edges.
        self.pointer
            .filter(|(sampled, _)| sampled.elapsed() < Duration::from_millis(16))
            .or_else(|| pointer_location().map(|point| (Instant::now(), point)))
    }

    pub fn volume() -> Option<f64> {
        if !can_post_input() {
            return None;
        }
        let output = Command::new("/usr/bin/osascript").args([
            "-e", "get {output volume of (get volume settings), output muted of (get volume settings)}",
        ]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8(output.stdout).ok()?;
        let (level, muted) = line.trim().split_once(',')?;
        match muted.trim() {
            "true" => Some(0.0),
            "false" => Some(level.trim().parse::<f64>().ok()? / 100.0),
            _ => None,
        }
    }

    pub fn set_volume(volume: f64) {
        if !can_post_input() {
            return;
        }
        let script = format!("set volume output volume {:.0}", volume * 100.0);
        let mut command = Command::new("/usr/bin/osascript");
        command.args(["-e", &script]);
        if volume > 0.0 {
            command.args(["-e", "set volume without output muted"]);
        }
        if !command.status().is_ok_and(|status| status.success()) {
            tracing::warn!("cannot set macOS output volume");
        }
    }
}

fn pointer_location() -> Option<Point> {
    unsafe {
        let event = CGEventCreate(null_mut());
        if event.is_null() {
            return None;
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some(point)
    }
}

fn post_owned(event: Event) {
    if !event.is_null() {
        unsafe {
            CGEventPost(0, event);
            CFRelease(event);
        }
    }
}

fn post_mouse(
    kind: u32,
    point: Point,
    button: u32,
    number: Option<i64>,
    count: Option<i64>,
) -> bool {
    const EVENT_NUMBER: u32 = 0;
    const CLICK_STATE: u32 = 1;
    let event = unsafe { CGEventCreateMouseEvent(null_mut(), kind, point, button) };
    if event.is_null() {
        return false;
    }
    unsafe {
        if let Some(number) = number {
            CGEventSetIntegerValueField(event, EVENT_NUMBER, number);
        }
        if let Some(count) = count {
            CGEventSetIntegerValueField(event, CLICK_STATE, count);
        }
    }
    post_owned(event);
    true
}

fn media_key(key: isize) {
    type CreateEvent = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        usize,
        Point,
        usize,
        f64,
        isize,
        *mut c_void,
        i16,
        isize,
        isize,
    ) -> *mut c_void;
    type GetEvent = unsafe extern "C" fn(*mut c_void, *mut c_void) -> Event;

    // objc_msgSend is called with the ABI of each specific NSEvent selector.
    // Its returned CGEvent is borrowed until the autorelease pool is popped.
    unsafe {
        let send = dlsym(-2isize as *mut c_void, c"objc_msgSend".as_ptr());
        let class = objc_getClass(c"NSEvent".as_ptr());
        if send.is_null() || class.is_null() {
            tracing::warn!("NSEvent is unavailable for macOS media keys");
            return;
        }
        let create: CreateEvent = std::mem::transmute(send);
        let get: GetEvent = std::mem::transmute(send);
        let create_selector = sel_registerName(c"otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:".as_ptr());
        let get_selector = sel_registerName(c"CGEvent".as_ptr());
        let pool = objc_autoreleasePoolPush();
        for state in [0xA, 0xB] {
            let event = create(
                class,
                create_selector,
                14,
                Point { x: 0.0, y: 0.0 },
                state << 8,
                0.0,
                0,
                null_mut(),
                8,
                (key << 16) | ((state as isize) << 8),
                -1,
            );
            if !event.is_null() {
                let event = get(event, get_selector);
                if !event.is_null() {
                    CGEventPost(0, event);
                }
            }
        }
        objc_autoreleasePoolPop(pool);
    }
}

#[cfg(test)]
mod tests {
    use super::post_text_graphemes;

    #[test]
    fn text_events_preserve_spaces_and_combined_characters() {
        let mut events = Vec::new();
        post_text_graphemes("e\u{301} 👩‍💻\r\n\u{1b}", |units| {
            events.push(String::from_utf16(units).unwrap());
        });
        assert_eq!(events, ["e\u{301}", " ", "👩‍💻", "\n"]);
    }
}
