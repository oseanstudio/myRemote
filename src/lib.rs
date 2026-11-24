//! Apple TV Remote receiver and verified Companion sessions.
#![forbid(unsafe_code)]

pub mod crypto;
mod delegate;
mod error;
mod keyed_archive;
pub mod opack;
mod pairing;
mod remote_keyboard;
mod remote_media;
mod server;
mod session;
pub mod srp;
mod tlv;

pub use delegate::{PlaybackAction, PlaybackState, RemoteInput, TrackpadMode, VideoSource};
pub use error::{Error, Result};
pub use pairing::PairingState;
pub use server::{detect_local_ip, DiscoveryRecord, Receiver, TAILNET_PORTS};
#[cfg(target_os = "macos")]
pub use server::{PeerTarget, PEER_AIRPLAY_SERVICE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FrameType {
    Noop = 1,
    PsStart = 3,
    PsNext = 4,
    PvStart = 5,
    PvNext = 6,
    UOpack = 7,
    EOpack = 8,
    POpack = 9,
}

impl FrameType {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Noop,
            3 => Self::PsStart,
            4 => Self::PsNext,
            5 => Self::PvStart,
            6 => Self::PvNext,
            7 => Self::UOpack,
            8 => Self::EOpack,
            9 => Self::POpack,
            _ => return None,
        })
    }
}
