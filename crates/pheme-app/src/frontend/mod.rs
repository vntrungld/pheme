//! The front-end without a user interface.
//!
//! Spawns the core the configured role implies, watches it, restarts it
//! after a configuration change, and stops it. Kept free of any windowing
//! or tray code so it can be driven headlessly from a test, before the tray
//! and the window are built on top of it.

pub mod supervisor;

pub use supervisor::{CoreState, Supervisor};
