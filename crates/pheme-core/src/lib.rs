//! OS-independent KVM logic: screen layout, active-screen state machine and key tracking.
//! This crate must never call the OS or use `cfg(target_os)`.

pub mod client;
pub mod geometry;
pub mod server;

pub use client::{ClientCore, InjectAction};
pub use geometry::{Rect, Side};
pub use server::{Action, Active, CaptureEvent, ClientPlacement, Hotkeys, Layout, ServerCore};
