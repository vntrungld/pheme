//! OS-independent KVM logic: screen layout, active-screen state machine and key tracking.
//! This crate must never call the OS or use `cfg(target_os)`.

pub mod geometry;
pub mod server;

pub use geometry::{Rect, Side};
pub use server::{Action, Active, CaptureEvent, ClientPlacement, Hotkeys, Layout, ServerCore};
