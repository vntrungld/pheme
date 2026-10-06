//! The rig's instrument. Runs inside the test VMs: emits input on the machine
//! acting as server, and records what arrives on the machine acting as client.
//! See `testrig/` and `docs/superpowers/specs/2026-09-30-test-rig-design.md`.

pub mod emit;
pub mod gesture;
pub mod record;
pub mod selfcheck;
pub mod tally;

#[cfg(target_os = "linux")]
mod record_linux;
#[cfg(target_os = "windows")]
mod record_windows;
