use std::io;
use std::sync::Mutex;

use myremote::RemoteInput;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{
    accessibility_is_trusted, can_post_input, request_accessibility_entry, request_input_permission,
};
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

pub struct Desktop {
    device: Mutex<platform::Desktop>,
    remainder: Mutex<(f64, f64)>,
    speed: f64,
}

impl Desktop {
    pub fn open(speed: f64) -> io::Result<Self> {
        Ok(Self {
            device: Mutex::new(platform::Desktop::open()?),
            remainder: Mutex::new((0.0, 0.0)),
            speed,
        })
    }
}

impl RemoteInput for Desktop {
    fn button(&self, name: &str) {
        self.device.lock().unwrap().button(name);
    }

    fn move_pointer(&self, dx: f64, dy: f64) {
        let mut remainder = self.remainder.lock().unwrap();
        let x = remainder.0 + dx * self.speed;
        let y = remainder.1 + dy * self.speed;
        let (dx, dy) = (x.trunc() as i32, y.trunc() as i32);
        *remainder = (x.fract(), y.fract());
        if dx != 0 || dy != 0 {
            self.device.lock().unwrap().move_pointer(dx, dy);
        }
    }

    fn click(&self) {
        self.device.lock().unwrap().click();
    }

    #[cfg(target_os = "macos")]
    fn focused_text_field(&self) -> Option<u64> {
        platform::Desktop::focused_text_field()
    }

    #[cfg(target_os = "macos")]
    fn type_text(&self, field: u64, backspaces: u8, deletes: u8, text: &str) -> bool {
        self.device
            .lock()
            .unwrap()
            .type_text(field, backspaces, deletes, text)
    }

    fn volume(&self) -> Option<f64> {
        self.device.lock().unwrap().volume()
    }

    fn set_volume(&self, volume: f64) {
        self.device.lock().unwrap().set_volume(volume);
    }
}

#[derive(Default)]
pub struct DryRun {
    text_focus: Mutex<(u64, bool)>,
}

impl RemoteInput for DryRun {
    fn button(&self, name: &str) {
        if name == "menu" {
            self.text_focus.lock().unwrap().1 = false;
        }
        println!("Received button {name}");
    }

    fn move_pointer(&self, dx: f64, dy: f64) {
        println!("Received pointer motion ({dx:.1}, {dy:.1})");
    }

    fn click(&self) {
        // In a dry run, a tap stands in for focusing a text field.
        let mut focus = self.text_focus.lock().unwrap();
        *focus = (focus.0 + 1, true);
        println!("Received click");
    }

    fn focused_text_field(&self) -> Option<u64> {
        let (field, focused) = *self.text_focus.lock().unwrap();
        focused.then_some(field)
    }

    fn type_text(&self, field: u64, backspaces: u8, deletes: u8, text: &str) -> bool {
        if self.focused_text_field() != Some(field) {
            return false;
        }
        println!(
            "Received keyboard edit ({backspaces} backspaces, {deletes} deletes, {} characters)",
            text.chars().count()
        );
        true
    }
}
