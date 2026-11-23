#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackpadMode {
    Mouse,
    Direction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoSource {
    Jellyfin,
    YouTube,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackState {
    pub source: VideoSource,
    pub playing: Option<bool>,
    pub captions: Option<bool>,
    pub duration_seconds: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackAction {
    SkipForward,
    SkipBackward,
    Play,
    Pause,
    ToggleCaptions,
    SetCaptions(bool),
}

pub trait RemoteInput: Send + Sync + 'static {
    fn button(&self, name: &str);
    fn move_pointer(&self, dx: f64, dy: f64);
    fn click(&self);
    fn double_click(&self) {
        self.click();
    }
    fn right_click(&self) {}
    fn start_drag(&self) -> bool {
        false
    }
    fn move_drag(&self, dx: f64, dy: f64) {
        self.move_pointer(dx, dy);
    }
    fn end_drag(&self) {}
    /// Whether to monitor the foreground player for native Remote controls.
    fn playback_enabled(&self) -> bool {
        false
    }
    fn playback_state(&self) -> Option<PlaybackState> {
        None
    }
    fn playback_action(&self, _source: VideoSource, _action: PlaybackAction) -> bool {
        false
    }
    /// Identifies the focused text field without reading its value.
    fn focused_text_field(&self) -> Option<u64> {
        None
    }
    /// Applies a keyboard operation only if the advertised field still has focus.
    fn type_text(&self, _field: u64, _backspaces: u8, _deletes: u8, _text: &str) -> bool {
        false
    }
    fn set_volume(&self, _volume: f64) {}
    fn volume(&self) -> Option<f64> {
        None
    }
}
