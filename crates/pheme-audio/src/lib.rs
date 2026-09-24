//! Audio capture and playback with per-OS backends, plus the OS-free jitter buffer and
//! drift controller that sit between them.
//!
//! The whole crate speaks one format and only one: 48 kHz, 2 channels, interleaved i16,
//! 240 samples per channel per frame (5 ms, 960 bytes on the wire).

pub mod device;
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

/// Whether anything is consuming what a playback backend emits.
///
/// Only `Idle` may close a microphone. `Unknown` is the trait default, so a backend that
/// cannot tell — which is every backend except the Linux virtual source — keeps the
/// microphone open without having to opt in. The asymmetry is deliberate: a microphone
/// wrongly held open wastes bandwidth and lights an indicator, while one wrongly held
/// shut makes the whole feature fail silently, and silent failure is the defect class
/// that has reached a user three times in this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    /// Something is recording from this device right now.
    Wanted,
    /// Nothing is recording, and the backend is sure of it.
    Idle,
    /// The backend cannot tell. Treated as `Wanted`.
    Unknown,
}

/// Reads audio out of the machine: the virtual sink on Linux, loopback of the default
/// output on Windows.
pub trait AudioCapture: Send {
    /// Opens the device and starts writing interleaved i16 samples into `sink`.
    ///
    /// **Synchronous**: returns only once the device is running, or with the error that
    /// stopped it. The device callback must never block — when `sink` is full the
    /// backend drops the *newest* samples and counts the overrun. Newest, not oldest:
    /// `rtrb::Producer::push` fails on a full ring, so the sample in hand is the one
    /// that goes, and nothing already accepted is ever rewritten.
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()>;
    /// The device actually in use, for logs. Valid only after `start` succeeded.
    fn device_name(&self) -> String;
    /// False once the backend's device thread has died — a PipeWire daemon restart, a
    /// device that cannot be reopened. The supervisor polls this and rebuilds the
    /// backend, which is the only way a failure after a successful `start` is noticed.
    /// Backends with no thread to lose keep the default.
    ///
    /// **A backend recovers one way or the other, never half of each.** A backend may
    /// reopen a new device internally and stay healthy throughout, as WASAPI does for
    /// `AUDCLNT_E_DEVICE_INVALIDATED`; or it may end its thread and report false, as
    /// PipeWire does when the daemon goes away, and let the supervisor build a fresh
    /// one. What it must not do is survive a change the rest of the pipeline was
    /// configured against — `rate()` above all — while still reporting true, because
    /// nothing downstream will ever be told. Reporting false is always safe: the
    /// supervisor rebuilds everything from `detect_*` down.
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
    /// `AudioCapture::healthy`, and the same choice of recovery mechanism: reopen
    /// internally and stay healthy, or end the thread and report false. The playback
    /// worker reads `rate()` once and bakes it into its resampler, so a backend that
    /// reopens internally at a different rate must report false rather than quietly
    /// republish the rate — the pipeline would otherwise play at the wrong pitch for
    /// the life of the process.
    fn healthy(&self) -> bool {
        true
    }
    /// Whether anything is consuming what this backend emits.
    ///
    /// Meaningful only for a backend that presents a device to other applications — the
    /// client's virtual microphone. A backend that writes to real speakers keeps the
    /// default, and nothing reads it.
    fn demand(&self) -> Demand {
        Demand::Unknown
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
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::wasapi::WasapiPlayback::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        Err(Error::Unsupported(
            "no audio playback backend for this platform".into(),
        ))
    }
}

/// Picks the client's virtual-microphone backend for this OS.
///
/// `device` is accepted for symmetry with the other detectors and is never used: on
/// Linux we create the node rather than choosing one, and no other platform has a
/// virtual microphone at all. There is no config key that could set it.
pub fn detect_virtual_mic(_device: Option<&str>) -> Result<Box<dyn AudioPlayback>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_pipewire::PipewireVirtualSource::new()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows has no user-mode API that creates an audio endpoint, so a virtual
        // microphone there needs a signed kernel driver (VB-CABLE). Deferred; see §13 of
        // the sub-project 3 spec. Returning `Unsupported` is what makes a client without
        // one report no demand, so the server never opens its microphone for it.
        Err(Error::Unsupported(
            "this platform has no virtual microphone; the server's microphone will stay closed"
                .into(),
        ))
    }
}

/// Picks the server's microphone backend for this OS. `device` names a specific device;
/// `None` means the platform default.
pub fn detect_mic(device: Option<&str>) -> Result<Box<dyn AudioCapture>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_pipewire::PipewireMic::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::wasapi::WasapiMic::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        Err(Error::Unsupported(
            "no microphone capture backend for this platform".into(),
        ))
    }
}
