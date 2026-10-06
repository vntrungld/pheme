//! The test rig's runner: turns observations from the VMs into verdicts and a
//! report. See `testrig/` and `docs/superpowers/specs/2026-09-30-test-rig-design.md`.

pub mod deploy;
pub mod lifecycle;
pub mod report;
pub mod rows;
pub mod verdict;
pub mod vm;
