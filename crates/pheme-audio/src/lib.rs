//! Audio capture and playback with per-OS backends, plus the OS-free jitter buffer and
//! drift controller that sit between them.
//!
//! The whole crate speaks one format and only one: 48 kHz, 2 channels, interleaved i16,
//! 240 samples per channel per frame (5 ms, 960 bytes on the wire).

pub mod frame;
pub mod jitter;
pub mod pack;

/// Sample rate on the wire, in hertz.
pub const RATE: u32 = 48_000;
/// Channel count on the wire.
pub const CHANNELS: usize = 2;
/// Samples per channel in one frame.
pub const FRAME_SAMPLES: usize = 240;
/// Interleaved samples in one frame.
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS;
/// Bytes in one frame on the wire.
pub const FRAME_BYTES: usize = FRAME_INTERLEAVED * 2;
/// Duration of one frame, in microseconds.
pub const FRAME_US: u64 = 5_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No backend exists for this platform or session.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A device could not be opened or does not support a usable format.
    #[error("device error: {0}")]
    Device(String),
    /// The backend failed internally.
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, Error>;
