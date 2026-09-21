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
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()>;
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
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
    Err(Error::Unsupported("not yet implemented".into()))
}

/// Picks the injection backend for this OS.
pub fn detect_inject() -> Result<Box<dyn InputInject>> {
    Err(Error::Unsupported("not yet implemented".into()))
}
