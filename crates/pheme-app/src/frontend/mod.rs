//! The GUI front-end, built in layers.
//!
//! [`supervisor`] spawns the core the configured role implies, watches it,
//! restarts it after a configuration change, and stops it -- kept free of
//! any windowing or tray code so it can be driven headlessly from a test.
//! [`tray`] is the system tray icon and menu built on top of it; the window
//! (not yet written) is the other consumer, and neither this module nor
//! `supervisor` knows about either of them.

pub mod supervisor;
pub mod tray;

pub use supervisor::{CoreState, Supervisor};
pub use tray::{Tray, TrayEvent};
