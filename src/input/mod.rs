use std::io;
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use myremote::{PlaybackAction, PlaybackState, RemoteInput, VideoSource};

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{
    accessibility_is_trusted, can_post_input, request_accessibility_entry, request_input_permission,
};
#[cfg(target_os = "macos")]
#[path = "macos_media.rs"]
mod macos_media;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
const SCROLL_SCALE: f64 = 1.0; // pixels per touch unit
#[cfg(not(target_os = "macos"))]
const SCROLL_SCALE: f64 = 4.0; // 120 units per wheel detent

pub struct Desktop {
    state: Mutex<PointerState>,
    speed: f64,
    #[cfg(target_os = "macos")]
    media_controls: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    playback: Mutex<PlaybackCache>,
    #[cfg(target_os = "macos")]
    keyboard: Mutex<()>,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct PlaybackCache {
    checked: Option<std::time::Instant>,
    media: Option<macos_media::BrowserPlayback>,
}

struct PointerState {
    device: platform::Desktop,
    #[cfg(not(target_os = "macos"))]
    remainder: (f64, f64),
    scroll_remainder: f64,
    dragging: bool,
}

impl Desktop {
    pub fn open(
        speed: f64,
        #[cfg(target_os = "macos")] media_controls: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        Ok(Self {
            state: Mutex::new(PointerState {
                device: platform::Desktop::open()?,
                #[cfg(not(target_os = "macos"))]
                remainder: (0.0, 0.0),
                scroll_remainder: 0.0,
                dragging: false,
            }),
            speed,
            #[cfg(target_os = "macos")]
            media_controls,
            #[cfg(target_os = "macos")]
            playback: Mutex::new(PlaybackCache::default()),
            #[cfg(target_os = "macos")]
            keyboard: Mutex::new(()),
        })
    }

    fn pointer_move(&self, dx: f64, dy: f64, dragging: bool) {
        let mut state = self.state.lock().unwrap();
        if state.dragging != dragging {
            return;
        }
        let (dx, dy) = (dx * self.speed, dy * self.speed);
        #[cfg(target_os = "macos")]
        if dx != 0.0 || dy != 0.0 {
            state.device.move_pointer(dx, dy, dragging);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let x = state.remainder.0 + dx;
            let y = state.remainder.1 + dy;
            let (dx, dy) = (x.trunc() as i32, y.trunc() as i32);
            state.remainder = (x.fract(), y.fract());
            if dx != 0 || dy != 0 {
                state.device.move_pointer(dx, dy, dragging);
            }
        }
    }

    fn click_count(&self, count: i64) {
        let mut state = self.state.lock().unwrap();
        if !state.dragging {
            state.device.click(count);
        }
    }
}

impl RemoteInput for Desktop {
    fn button(&self, name: &str) {
        #[cfg(target_os = "macos")]
        if self.media_controls.load(Ordering::Relaxed) && macos_media::handle(name) {
            return;
        }
        self.state.lock().unwrap().device.button(name);
    }

    fn move_pointer(&self, dx: f64, dy: f64) {
        self.pointer_move(dx, dy, false);
    }

    fn scroll_vertical(&self, delta: f64) {
        let mut state = self.state.lock().unwrap();
        if state.dragging {
            return;
        }
        let total = state.scroll_remainder + delta * SCROLL_SCALE;
        state.scroll_remainder = total.fract();
        let units = total as i32;
        if units != 0 {
            state.device.scroll_vertical(units);
        }
    }

    fn click(&self) {
        self.click_count(1);
    }

    fn double_click(&self) {
        self.click_count(2);
    }

    fn triple_click(&self) {
        self.click_count(3);
    }

    fn right_click(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.dragging {
            state.device.right_click();
        }
    }

    fn start_drag(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.dragging || !state.device.start_drag() {
            return false;
        }
        state.dragging = true;
        true
    }

    fn move_drag(&self, dx: f64, dy: f64) {
        self.pointer_move(dx, dy, true);
    }

    fn end_drag(&self) {
        let mut state = self.state.lock().unwrap();
        if state.dragging {
            state.device.end_drag();
            state.dragging = false;
        }
    }

    #[cfg(target_os = "macos")]
    fn playback_enabled(&self) -> bool {
        self.media_controls.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "macos")]
    fn playback_state(&self) -> Option<PlaybackState> {
        let mut playback = self.playback.lock().unwrap();
        if playback
            .checked
            .is_none_or(|time| time.elapsed() >= std::time::Duration::from_millis(750))
        {
            playback.media = macos_media::playback_state(playback.media);
            playback.checked = Some(std::time::Instant::now());
        }
        playback.media.map(|video| video.state)
    }

    #[cfg(target_os = "macos")]
    fn playback_action(&self, source: VideoSource, action: PlaybackAction) -> bool {
        if !self.playback_enabled() {
            return false;
        }
        let applied = macos_media::playback_action(source, action);
        if applied {
            self.playback.lock().unwrap().checked = None;
        }
        applied
    }

    #[cfg(target_os = "macos")]
    fn focused_text_field(&self) -> Option<u64> {
        platform::Desktop::focused_text_field()
    }

    #[cfg(target_os = "macos")]
    fn type_text(&self, field: u64, backspaces: u8, deletes: u8, text: &str) -> bool {
        let _keyboard = self.keyboard.lock().unwrap();
        platform::Desktop::type_text(field, backspaces, deletes, text)
    }

    #[cfg(target_os = "macos")]
    fn clear_text(&self, field: u64) -> bool {
        let _keyboard = self.keyboard.lock().unwrap();
        platform::Desktop::clear_text(field)
    }

    #[cfg(target_os = "macos")]
    fn accept_suggestion(&self, field: u64, typed: &str, word: &str, following: &str) -> bool {
        let _keyboard = self.keyboard.lock().unwrap();
        platform::Desktop::accept_suggestion(field, typed, word, following)
    }

    fn volume(&self) -> Option<f64> {
        #[cfg(target_os = "macos")]
        return platform::Desktop::volume();
        #[cfg(not(target_os = "macos"))]
        self.state.lock().unwrap().device.volume()
    }

    fn set_volume(&self, volume: f64) {
        #[cfg(target_os = "macos")]
        platform::Desktop::set_volume(volume);
        #[cfg(not(target_os = "macos"))]
        self.state.lock().unwrap().device.set_volume(volume);
    }
}

#[derive(Default)]
pub struct DryRun {
    text_focus: Mutex<(u64, bool)>,
    playback: Mutex<Option<PlaybackState>>,
}

impl RemoteInput for DryRun {
    fn button(&self, name: &str) {
        if name == "menu" {
            self.text_focus.lock().unwrap().1 = false;
        }
        // Media buttons also simulate entering and leaving a player in smoke tests.
        match name {
            "play_pause" | "home" => {
                let source = if name == "home" {
                    VideoSource::Jellyfin
                } else {
                    VideoSource::YouTube
                };
                *self.playback.lock().unwrap() = Some(PlaybackState {
                    source,
                    playing: Some(true),
                    captions: (source == VideoSource::YouTube).then_some(false),
                    duration_seconds: Some(600),
                });
            }
            "menu" => *self.playback.lock().unwrap() = None,
            _ => {}
        }
        println!("Received button {name}");
    }

    fn move_pointer(&self, dx: f64, dy: f64) {
        println!("Received pointer motion ({dx:.1}, {dy:.1})");
    }

    fn scroll_vertical(&self, delta: f64) {
        println!("Received vertical scroll ({delta:.1})");
    }

    fn click(&self) {
        // In a dry run, a tap stands in for focusing a text field.
        let mut focus = self.text_focus.lock().unwrap();
        *focus = (focus.0 + 1, true);
        println!("Received click");
    }

    fn double_click(&self) {
        println!("Received double-click");
    }

    fn triple_click(&self) {
        println!("Received triple-click");
    }

    fn right_click(&self) {
        println!("Received right-click");
    }

    fn start_drag(&self) -> bool {
        println!("Received drag start");
        true
    }

    fn move_drag(&self, dx: f64, dy: f64) {
        println!("Received drag motion ({dx:.1}, {dy:.1})");
    }

    fn end_drag(&self) {
        println!("Received drag end");
    }

    fn playback_enabled(&self) -> bool {
        true
    }

    fn playback_state(&self) -> Option<PlaybackState> {
        *self.playback.lock().unwrap()
    }

    fn playback_action(&self, source: VideoSource, action: PlaybackAction) -> bool {
        let mut state = self.playback.lock().unwrap();
        let Some(state) = state.as_mut().filter(|state| state.source == source) else {
            return false;
        };
        match action {
            PlaybackAction::Play => state.playing = Some(true),
            PlaybackAction::Pause => state.playing = Some(false),
            PlaybackAction::ToggleCaptions => {
                state.captions = state.captions.map(|enabled| !enabled)
            }
            PlaybackAction::SetCaptions(enabled) => {
                state.captions = state.captions.map(|_| enabled)
            }
            PlaybackAction::SkipForward | PlaybackAction::SkipBackward => {}
        }
        println!("Received playback {source:?} {action:?}");
        true
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

    fn clear_text(&self, field: u64) -> bool {
        if self.focused_text_field() != Some(field) {
            return false;
        }
        println!("Received keyboard clear");
        true
    }
}
