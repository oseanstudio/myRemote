#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackpadMode {
    Mouse,
    Direction,
}

pub trait RemoteInput: Send + Sync + 'static {
    fn button(&self, name: &str);
    fn move_pointer(&self, dx: f64, dy: f64);
    fn click(&self);
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
