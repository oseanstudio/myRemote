//! Apple TV Remote receiver and verified Companion sessions.
#![forbid(unsafe_code)]

pub mod crypto;
mod delegate;
mod error;
pub mod opack;
mod pairing;
mod server;
mod session;
pub mod srp;
mod tlv;

pub use delegate::{RemoteInput, TrackpadMode};
pub use error::{Error, Result};
pub use pairing::PairingState;
pub use server::{detect_local_ip, DiscoveryRecord, Receiver};

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
