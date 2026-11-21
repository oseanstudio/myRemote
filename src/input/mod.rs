use std::io;
use std::sync::Mutex;

use myremote::RemoteInput;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{can_post_input, request_input_permission};
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

    fn volume(&self) -> Option<f64> {
        self.device.lock().unwrap().volume()
    }

    fn set_volume(&self, volume: f64) {
        self.device.lock().unwrap().set_volume(volume);
    }
}

pub struct DryRun;

impl RemoteInput for DryRun {
    fn button(&self, name: &str) {
        println!("Received button {name}");
    }

    fn move_pointer(&self, dx: f64, dy: f64) {
        println!("Received pointer motion ({dx:.1}, {dy:.1})");
    }

    fn click(&self) {
        println!("Received click");
    }
}
