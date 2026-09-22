//! Audio capture and playback with per-OS backends, plus the OS-free jitter buffer and
//! drift controller that sit between them.
//!
//! The whole crate speaks one format and only one: 48 kHz, 2 channels, interleaved i16,
//! 240 samples per channel per frame (5 ms, 960 bytes on the wire).

pub mod drift;
pub mod frame;
pub mod jitter;
#[cfg(target_os = "linux")]
pub mod linux_pipewire;
pub mod mock;
pub mod pack;
#[cfg(target_os = "windows")]
pub mod windows;

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

/// Reads audio out of the machine: the virtual sink on Linux, loopback of the default
/// output on Windows.
pub trait AudioCapture: Send {
    /// Opens the device and starts writing interleaved i16 samples into `sink`.
    ///
    /// **Synchronous**: returns only once the device is running, or with the error that
    /// stopped it. The device callback must never block — when `sink` is full the
    /// backend drops samples and counts the overrun.
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()>;
    /// The device actually in use, for logs. Valid only after `start` succeeded.
    fn device_name(&self) -> String;
    /// False once the backend's device thread has died — a PipeWire daemon restart, a
    /// device that cannot be reopened. The supervisor polls this and rebuilds the
    /// backend, which is the only way a failure after a successful `start` is noticed.
    /// Backends with no thread to lose keep the default.
    fn healthy(&self) -> bool {
        true
    }
    /// Idempotent. Joins the device thread before returning.
    fn stop(&mut self);
}

/// Plays audio out of the machine's speakers.
pub trait AudioPlayback: Send {
    /// Opens the device and starts draining interleaved i16 samples from `source`.
    ///
    /// **Synchronous**, same contract as `AudioCapture::start`. On underrun the backend
    /// writes silence rather than blocking.
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()>;
    /// The device's own sample rate, valid only after `start` succeeded. The playback
    /// worker uses `rate() / RATE` as the base resample ratio.
    fn rate(&self) -> u32;
    /// The device actually in use, for logs. Valid only after `start` succeeded.
    fn device_name(&self) -> String;
    /// False once the backend's device thread has died. Same contract as
    /// `AudioCapture::healthy`.
    fn healthy(&self) -> bool {
        true
    }
    /// Idempotent. Joins the device thread before returning.
    fn stop(&mut self);
}

/// Picks the capture backend for this OS. `device` names a specific device; `None` means
/// the platform default.
pub fn detect_capture(device: Option<&str>) -> Result<Box<dyn AudioCapture>> {
    #[cfg(target_os = "linux")]
    {
        // On Linux we *are* the device: the sink we create is what the user selects.
        // A named capture device is a Windows-only concept (see the spec, §5).
        if let Some(d) = device {
            tracing::warn!(
                device = d,
                "audio.capture_device is ignored on Linux; applications select \"Pheme Speaker\" instead"
            );
        }
        Ok(Box::new(linux_pipewire::PipewireCapture::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::wasapi::WasapiCapture::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        Err(Error::Unsupported(
            "no audio capture backend for this platform".into(),
        ))
    }
}

/// Picks the playback backend for this OS. `device` names a specific device; `None`
/// means the platform default.
pub fn detect_playback(device: Option<&str>) -> Result<Box<dyn AudioPlayback>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_pipewire::PipewirePlayback::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        Err(Error::Unsupported(
            "no audio playback backend for this platform".into(),
        ))
    }
}
