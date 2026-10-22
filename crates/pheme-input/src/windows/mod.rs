//! Windows backends: low-level hooks + Raw Input for capture, SendInput for injection.

pub mod capture;
pub mod inject;
pub mod screens;
mod settle;

pub use capture::WindowsCapture;
pub use inject::WindowsInject;
