use std::io;
use std::process::{Command, Stdio};

use evdev::{uinput::VirtualDevice, AttributeSet, InputEvent, KeyCode, RelativeAxisCode};

const KEYS: &[(&str, KeyCode)] = &[
    ("up", KeyCode::KEY_UP),
    ("down", KeyCode::KEY_DOWN),
    ("left", KeyCode::KEY_LEFT),
    ("right", KeyCode::KEY_RIGHT),
    ("select", KeyCode::KEY_ENTER),
    ("menu", KeyCode::KEY_ESC),
    ("home", KeyCode::KEY_HOME),
    ("channel_up", KeyCode::KEY_PAGEUP),
    ("channel_down", KeyCode::KEY_PAGEDOWN),
    ("guide", KeyCode::KEY_TAB),
    ("play_pause", KeyCode::KEY_PLAYPAUSE),
    ("volume_up", KeyCode::KEY_VOLUMEUP),
    ("volume_down", KeyCode::KEY_VOLUMEDOWN),
    ("mute", KeyCode::KEY_MUTE),
];

pub struct Desktop {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
}

impl Desktop {
    pub fn open() -> io::Result<Self> {
        let create = |error: io::Error| {
            io::Error::new(
                error.kind(),
                format!("cannot create a virtual input device via /dev/uinput: {error}"),
            )
        };
        let builder = VirtualDevice::builder().map_err(create)?;
        // Compositors identify keyboards by their standard key capabilities.
        let standard_keys = (KeyCode::KEY_ESC.code()..=KeyCode::KEY_SPACE.code()).map(KeyCode::new);
        let keys = standard_keys.chain(KEYS.iter().map(|&(_, key)| key));
        let keyboard = builder
            .name("myRemote keyboard")
            .with_keys(&AttributeSet::from_iter(keys))?
            .build()?;
        let mouse = VirtualDevice::builder()?
            .name("myRemote pointer")
            .with_keys(&AttributeSet::from_iter([KeyCode::BTN_LEFT]))?
            .with_relative_axes(&AttributeSet::from_iter([
                RelativeAxisCode::REL_X,
                RelativeAxisCode::REL_Y,
            ]))?
            .build()?;
        Ok(Self { keyboard, mouse })
    }

    pub fn button(&mut self, name: &str) {
        if let Some(&(_, key)) = KEYS.iter().find(|&&(button, _)| button == name) {
            press(&mut self.keyboard, key);
        }
    }

    pub fn click(&mut self) {
        press(&mut self.mouse, KeyCode::BTN_LEFT);
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) {
        emit(
            &mut self.mouse,
            &[
                InputEvent::new(2, RelativeAxisCode::REL_X.0, dx),
                InputEvent::new(2, RelativeAxisCode::REL_Y.0, dy),
            ],
        );
    }

    pub fn volume(&self) -> Option<f64> {
        let output = Command::new("wpctl")
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        if text.contains("[MUTED]") {
            return Some(0.0);
        }
        let level: f64 = text.split_whitespace().nth(1)?.parse().ok()?;
        Some(level.clamp(0.0, 1.0))
    }

    pub fn set_volume(&self, volume: f64) {
        let level = format!("{:.0}%", volume * 100.0);
        let result = Command::new("wpctl")
            .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &level])
            .stderr(Stdio::null())
            .status();
        if result.is_ok_and(|status| status.success()) && volume > 0.0 {
            let _ = Command::new("wpctl")
                .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "0"])
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn press(device: &mut VirtualDevice, key: KeyCode) {
    emit(device, &[InputEvent::new(1, key.code(), 1)]);
    emit(device, &[InputEvent::new(1, key.code(), 0)]);
}

fn emit(device: &mut VirtualDevice, events: &[InputEvent]) {
    if let Err(error) = device.emit(events) {
        tracing::warn!("cannot send desktop input: {error}");
    }
}
