//! Monitor input switching over DDC/CI.
//!
//! One trait, two backends and a policy. The crate knows nothing about
//! pheme's protocol or its configuration: it is handed a number and told to
//! put it in the monitor's Input Select register.

pub mod switch;

pub use switch::DisplaySwitch;

/// VCP feature code for Input Select (MCCS 2.2 section 8.4).
pub const INPUT_SELECT: u8 = 0x60;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// Nothing on this machine answered a read of VCP 0x60.
    #[error("no monitor answered DDC/CI")]
    NoMonitor,
    /// Monitors were found but none matched `display.monitor`.
    #[error("no monitor matches {0:?}")]
    NoMatch(String),
    #[error("DDC/CI: {0}")]
    Backend(String),
}

/// One monitor that answers DDC/CI.
///
/// Deliberately **not** `Send`. `ddc_winapi::Monitor` wraps a
/// `PHYSICAL_MONITOR`, which holds a raw `HANDLE` and carries no
/// `unsafe impl Send`, so a `Send` bound here would fail to compile on
/// Windows and nowhere else -- the CI leg this repository cannot run
/// locally. It does not need one: the handle is created by the `OpenFn`
/// *on* `DisplayService`'s own thread and never leaves it. A closure's
/// `Send` depends on what it captures, not on what it returns, so the
/// boxed `OpenFn` stays `Send` regardless.
pub trait Monitor {
    /// A stable, human-readable name. Matched case-insensitively against
    /// `display.monitor`, and printed by `pheme displays`.
    fn identity(&self) -> &str;
    /// Where the backend found it: an i2c device path on Linux, the physical
    /// monitor description on Windows. Printed, never matched.
    fn location(&self) -> &str;
    fn get_input(&mut self) -> Result<u16, DisplayError>;
    fn set_input(&mut self, value: u16) -> Result<(), DisplayError>;
    /// The raw capability string, when the monitor returns one.
    fn capabilities(&mut self) -> Result<String, DisplayError>;
}
