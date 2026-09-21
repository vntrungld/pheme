//! OS-independent KVM logic: screen layout, active-screen state machine and key tracking.
//! This crate must never call the OS or use `cfg(target_os)`.

pub mod geometry;

pub use geometry::{Rect, Side};
