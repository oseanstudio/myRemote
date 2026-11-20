#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackpadMode {
    Mouse,
    Direction,
}

pub trait RemoteInput: Send + Sync + 'static {
    fn button(&self, name: &str);
    fn move_pointer(&self, dx: f64, dy: f64);
    fn click(&self);
    fn set_volume(&self, _volume: f64) {}
    fn volume(&self) -> Option<f64> {
        None
    }
}
