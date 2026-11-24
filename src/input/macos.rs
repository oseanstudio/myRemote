use std::ffi::{c_char, c_void};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

type Event = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct TextRange {
    location: isize,
    length: isize,
}

const AX_VALUE_CF_RANGE: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct DisplayBounds {
    origin: Point,
    size: Point,
}

impl DisplayBounds {
    fn contains(self, point: Point) -> bool {
        (self.origin.x..self.origin.x + self.size.x).contains(&point.x)
            && (self.origin.y..self.origin.y + self.size.y).contains(&point.y)
    }

    fn clamp(self, point: Point) -> Point {
        Point {
            x: point
                .x
                .clamp(self.origin.x, self.origin.x + self.size.x - 1.0),
            y: point
                .y
                .clamp(self.origin.y, self.origin.y + self.size.y - 1.0),
        }
    }
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
    fn AXUIElementCopyParameterizedAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        parameter: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *const c_void;
    fn AXValueGetValue(value: *const c_void, kind: u32, result: *mut c_void) -> u8;
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
    fn CFStringCreateWithBytes(
        allocator: *const c_void,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> *const c_void;
    fn CFEqual(first: *const c_void, second: *const c_void) -> bool;
    fn CFGetTypeID(object: *const c_void) -> usize;
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
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> DisplayBounds;
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
    displays: Vec<DisplayBounds>,
    displays_checked: Option<Instant>,
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

struct FocusedField {
    id: u64,
    element: OwnedCf,
    secure: bool,
}

impl FocusedField {
    fn caret(&self) -> Option<isize> {
        if self.secure {
            return None;
        }
        let value = self.element.attribute(c"AXSelectedTextRange").ok()?;
        let mut range = TextRange::default();
        let valid = unsafe {
            CFGetTypeID(value.0) == AXValueGetTypeID()
                && AXValueGetValue(
                    value.0,
                    AX_VALUE_CF_RANGE,
                    (&mut range as *mut TextRange).cast(),
                ) != 0
        };
        (valid && range.length == 0 && range.location >= 0).then_some(range.location)
    }

    fn ends_with(&self, caret: isize, text: &str) -> Option<bool> {
        const UTF8: u32 = 0x0800_0100;
        let length = text.encode_utf16().count() as isize;
        if length == 0 || caret < length {
            return Some(false);
        }
        let range = TextRange {
            location: caret - length,
            length,
        };
        let argument = OwnedCf::new(unsafe {
            AXValueCreate(AX_VALUE_CF_RANGE, (&range as *const TextRange).cast())
        })?;
        let attribute = OwnedCf::string(c"AXStringForRange")?;
        let mut result = null();
        let status = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.element.0,
                attribute.0,
                argument.0,
                &mut result,
            )
        };
        let actual = OwnedCf::new(result)?;
        if status != 0 {
            return None;
        }
        let expected = OwnedCf::new(unsafe {
            CFStringCreateWithBytes(null(), text.as_ptr(), text.len() as isize, UTF8, 0)
        })?;
        Some(unsafe { CFEqual(actual.0, expected.0) })
    }
}

fn focused_field() -> Option<FocusedField> {
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
    Some(FocusedField {
        id: id.finish(),
        element: field,
        secure,
    })
}

fn suggestion_keys<'a>(
    typed: &str,
    word: &'a str,
    typed_matches: bool,
    word_matches: bool,
) -> (u8, &'a str) {
    if word_matches {
        return (0, "");
    }
    if typed_matches && !typed.is_empty() {
        if let Some(suffix) = word.strip_prefix(typed) {
            return (0, suffix);
        }
        let count = typed.graphemes(true).take(65).count();
        if count <= 64 {
            return (count as u8, word);
        }
    }
    (0, word)
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
            displays: Vec::new(),
            displays_checked: None,
        })
    }

    pub fn focused_text_field() -> Option<u64> {
        focused_field().map(|field| field.id)
    }

    pub fn type_text(field: u64, backspaces: u8, deletes: u8, text: &str) -> bool {
        if Self::focused_text_field() != Some(field) {
            return false;
        }
        Self::post_text(backspaces, deletes, text)
    }

    pub fn clear_text(field: u64) -> bool {
        const COMMAND: u64 = 1 << 20;
        // Use the focused text control's regular editing path so browsers
        // get input events and secure fields do not need to expose a value.
        Self::focused_text_field() == Some(field)
            && post_key(0, COMMAND, &[]) // Command-A
            && post_key(51, 0, &[]) // Delete
    }

    pub fn accept_suggestion(field: u64, typed: &str, word: &str, following: &str) -> bool {
        let Some(focus) = focused_field().filter(|focus| focus.id == field) else {
            return false;
        };
        let caret = focus.caret();
        let word_matches = caret.and_then(|position| focus.ends_with(position, word)) == Some(true);
        let typed_matches = !word_matches
            && caret.and_then(|position| focus.ends_with(position, typed)) == Some(true);
        let (backspaces, insertion) = suggestion_keys(typed, word, typed_matches, word_matches);
        if insertion.is_empty() && following.is_empty() {
            return false;
        }
        // Only erase a prefix we observed immediately before the caret.
        if Self::focused_text_field() != Some(field)
            || ((typed_matches || word_matches) && focus.caret() != caret)
        {
            return false;
        }
        Self::post_text(backspaces, 0, &format!("{insertion}{following}"))
    }

    fn post_text(backspaces: u8, deletes: u8, text: &str) -> bool {
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

    pub fn move_pointer(&mut self, dx: f64, dy: f64, dragging: bool) {
        let point = match self.recent_pointer() {
            Some(point) => point,
            None => {
                if !can_post_input() {
                    return;
                }
                let Some(point) = pointer_location() else {
                    return;
                };
                // Refresh the cursor after short gaps without re-enumerating
                // displays on every fresh batch of motion.
                if self
                    .displays_checked
                    .is_none_or(|checked| checked.elapsed() >= Duration::from_secs(1))
                {
                    self.displays = active_displays();
                    self.displays_checked = Some(Instant::now());
                }
                point
            }
        };
        let next = desktop_destination(
            &self.displays,
            point,
            Point {
                x: point.x + dx,
                y: point.y + dy,
            },
        );
        if next != point {
            let kind = if dragging { 6 } else { 5 };
            if post_mouse(kind, next, 0, self.drag_number, None) {
                self.pointer = Some((Instant::now(), next));
            }
        } else {
            self.pointer = Some((Instant::now(), point));
        }
    }

    pub fn click(&mut self, count: i64) {
        if !can_post_input() {
            return;
        }
        if let Some(point) = self.pointer_sample() {
            let number = self.next_mouse_event();
            for kind in [1, 2] {
                post_mouse(kind, point, 0, Some(number), Some(count));
            }
        }
    }

    pub fn right_click(&mut self) {
        if can_post_input() {
            if let Some(point) = self.pointer_sample() {
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
        let Some(point) = self.pointer_sample() else {
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
        if let Some(point) = self.pointer_sample() {
            post_mouse(2, point, 0, number, Some(1));
        }
    }

    fn next_mouse_event(&mut self) -> i64 {
        self.mouse_number = (self.mouse_number + 1) % i64::from(i32::MAX);
        self.mouse_number
    }

    fn recent_pointer(&self) -> Option<Point> {
        // Quartz can briefly report an earlier position after posting a move.
        // Accumulate a continuous gesture from the last position we posted.
        self.pointer
            .filter(|(posted, _)| posted.elapsed() < Duration::from_millis(48))
            .map(|(_, point)| point)
    }

    fn pointer_sample(&self) -> Option<Point> {
        self.recent_pointer().or_else(pointer_location)
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

fn active_displays() -> Vec<DisplayBounds> {
    let mut displays = [0; 32];
    let mut count = 0;
    if unsafe { CGGetActiveDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count) }
        != 0
    {
        return Vec::new();
    }
    displays
        .into_iter()
        .take(count as usize)
        .map(|display| unsafe { CGDisplayBounds(display) })
        .filter(|bounds| bounds.size.x >= 1.0 && bounds.size.y >= 1.0)
        .collect()
}

fn desktop_destination(displays: &[DisplayBounds], current: Point, next: Point) -> Point {
    if displays.iter().any(|display| display.contains(next)) {
        return next;
    }
    // Keep edge overshoot out of the accumulated position. Otherwise it would
    // take several movements in the opposite direction to leave a screen edge.
    displays
        .iter()
        .find(|display| display.contains(current))
        .map_or(next, |display| display.clamp(next))
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
    use super::{desktop_destination, post_text_graphemes, suggestion_keys, DisplayBounds, Point};

    #[test]
    fn keyboard_suggestions_complete_only_the_verified_prefix() {
        assert_eq!(suggestion_keys("hel", "hello", true, false), (0, "lo"));
        assert_eq!(suggestion_keys("teh", "the", true, false), (3, "the"));
        assert_eq!(suggestion_keys("swi", "Swipe", false, false), (0, "Swipe"));
        assert_eq!(suggestion_keys("hel", "hello", true, true), (0, ""));
    }

    #[test]
    fn text_events_preserve_spaces_and_combined_characters() {
        let mut events = Vec::new();
        post_text_graphemes("e\u{301} 👩‍💻\r\n\u{1b}", |units| {
            events.push(String::from_utf16(units).unwrap());
        });
        assert_eq!(events, ["e\u{301}", " ", "👩‍💻", "\n"]);
    }

    #[test]
    fn pointer_keeps_fractional_motion_and_crosses_adjacent_displays() {
        let main = DisplayBounds {
            origin: Point { x: 0.0, y: 0.0 },
            size: Point {
                x: 1920.0,
                y: 1080.0,
            },
        };
        let left = DisplayBounds {
            origin: Point {
                x: -1440.0,
                y: 30.0,
            },
            size: Point {
                x: 1440.0,
                y: 900.0,
            },
        };
        let position = Point { x: 0.0, y: 100.0 };
        let adjacent = Point { x: -0.75, y: 100.5 };
        assert_eq!(
            desktop_destination(&[main, left], position, adjacent),
            adjacent
        );
        assert_eq!(
            desktop_destination(&[main], position, adjacent),
            Point { x: 0.0, y: 100.5 }
        );
    }
}
