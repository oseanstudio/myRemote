use std::ffi::{c_char, c_void};
use std::io;
use std::process::Command;
use std::ptr::null_mut;

type Event = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CGEventCreate(source: *mut c_void) -> Event;
    fn CGEventGetLocation(event: Event) -> Point;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> Event;
    fn CGEventCreateMouseEvent(source: *mut c_void, kind: u32, point: Point, button: u32) -> Event;
    fn CGEventPost(tap: u32, event: Event);
    fn CFRelease(object: *const c_void);
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

pub struct Desktop;

impl Desktop {
    pub fn open() -> io::Result<Self> {
        if unsafe { AXIsProcessTrusted() } {
            Ok(Self)
        } else {
            Err(io::Error::new(io::ErrorKind::PermissionDenied,
                "grant myRemote or your terminal access in System Settings → Privacy & Security → Accessibility"))
        }
    }

    pub fn button(&mut self, name: &str) {
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

    pub fn move_pointer(&mut self, dx: i32, dy: i32) {
        if let Some(point) = pointer_location() {
            let next = Point {
                x: point.x + f64::from(dx),
                y: point.y + f64::from(dy),
            };
            post_owned(unsafe { CGEventCreateMouseEvent(null_mut(), 5, next, 0) });
        }
    }

    pub fn click(&mut self) {
        if let Some(point) = pointer_location() {
            for kind in [1, 2] {
                post_owned(unsafe { CGEventCreateMouseEvent(null_mut(), kind, point, 0) });
            }
        }
    }

    pub fn volume(&self) -> Option<f64> {
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

    pub fn set_volume(&self, volume: f64) {
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
