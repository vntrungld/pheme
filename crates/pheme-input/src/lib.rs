//! Input capture (server side) and injection (client side) with per-OS backends.

use crossbeam_channel::Sender;
use pheme_core::CaptureEvent;
use pheme_proto::{Button, KeyCode, ScreenInfo};

pub mod keymap;
pub mod mock;

#[cfg(target_os = "linux")]
pub mod linux_screens;
#[cfg(target_os = "linux")]
pub mod linux_uinput;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Watch pointer position and keys without blocking anything.
    Observe,
    /// Swallow all input, hide and confine the cursor, report relative motion.
    Grab,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait InputCapture: Send {
    /// Starts the backend thread; events are delivered on `tx`.
    ///
    /// The backend must deliver events with `Sender::try_send`, dropping events when the
    /// channel is full (never block). `stop()` must stop the backend thread and drop every
    /// clone of the `Sender` before returning, so the receiver observes disconnection.
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()>;
    /// Switches between observing and grabbing. **Synchronous**: returns only after the
    /// backend has applied the mode (grab acquired / released) or failed to, so a
    /// `warp_cursor` issued right after `set_mode(Observe)` is not undone by a still-active
    /// grab/clip, and a failed grab is reported to the caller instead of leaving the backend
    /// silently in the previous mode. Backends that hand the request to their own thread
    /// must wait for that thread's acknowledgement with an internal timeout of 1 s, mapping
    /// a timeout to `Error::Backend("mode change timed out")`. On `Err` the mode is
    /// unchanged.
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()>;
    /// Stops capturing and places the pointer at (x, y).
    ///
    /// **Synchronous**, under the same contract as `set_mode`: returns only once the
    /// backend has applied the change or failed to, with a 1 s internal timeout mapped
    /// to `Error::Backend("mode change timed out")`.
    ///
    /// A backend that implements this as an ungrab followed by a warp must attempt the
    /// warp **even when the ungrab fails**, and report the first error. The caller logs
    /// the error and continues, and a pointer left outside the screen because an ungrab
    /// failed is worse than a pointer that came back under a stale grab.
    fn release(&mut self, x: i32, y: i32) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
    /// Stops the backend thread.
    ///
    /// The backend must deliver events with `Sender::try_send`, dropping events when the
    /// channel is full (never block). `stop()` must stop the backend thread and drop every
    /// clone of the `Sender` before returning, so the receiver observes disconnection.
    fn stop(&mut self);
}

pub trait InputInject: Send {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, btn: Button, down: bool) -> Result<()>;
    /// Units of 1/120 notch.
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn key(&mut self, code: KeyCode, down: bool) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
}

/// Picks the capture backend for this OS and session.
pub fn detect_capture() -> Result<Box<dyn InputCapture>> {
    #[cfg(target_os = "linux")]
    {
        linux_x11::X11Capture::new().map(|c| Box::new(c) as Box<dyn InputCapture>)
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsCapture::new()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(Error::Unsupported("not yet implemented".into()))
    }
}

/// Picks the injection backend for this OS.
pub fn detect_inject() -> Result<Box<dyn InputInject>> {
    #[cfg(target_os = "linux")]
    {
        linux_uinput::UinputInject::new().map(|i| Box::new(i) as Box<dyn InputInject>)
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsInject::new()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(Error::Unsupported("not yet implemented".into()))
    }
}
