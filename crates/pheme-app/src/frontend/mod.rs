//! The GUI front-end, built in layers.
//!
//! [`supervisor`] spawns the core the configured role implies, watches it,
//! restarts it after a configuration change, and stops it -- kept free of
//! any windowing or tray code so it can be driven headlessly from a test.
//! [`tray`] is the system tray icon and menu built on top of it; [`window`]
//! is the other consumer -- the window and its status panel, plus [`run`],
//! which owns both and is the whole front-end.

pub mod supervisor;
pub mod tray;
pub mod window;

pub use supervisor::{CoreState, Supervisor};
pub use tray::{Tray, TrayEvent};
pub use window::{run, StatusView};
