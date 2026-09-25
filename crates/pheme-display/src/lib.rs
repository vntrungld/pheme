//! Monitor input switching over DDC/CI.
//!
//! One trait, two backends and a policy. The crate knows nothing about
//! pheme's protocol or its configuration: it is handed a number and told to
//! put it in the monitor's Input Select register.

pub mod switch;

pub mod caps;
pub mod edid;

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
    /// monitor description on Windows. Printed, and matched against
    /// `display.monitor` as well -- a monitor whose EDID carried no name
    /// has its bus path for an identity, and then this is the only thing
    /// left to choose it by (see `pick`).
    fn location(&self) -> &str;
    fn get_input(&mut self) -> Result<u16, DisplayError>;
    fn set_input(&mut self, value: u16) -> Result<(), DisplayError>;
    /// The raw capability string, when the monitor returns one.
    fn capabilities(&mut self) -> Result<String, DisplayError>;
}

pub mod mock;

#[cfg(target_os = "linux")]
mod backend_i2c;
#[cfg(target_os = "linux")]
use backend_i2c as backend;

#[cfg(windows)]
mod backend_winapi;
#[cfg(windows)]
use backend_winapi as backend;

/// Everywhere else the crate still compiles; the feature is simply never
/// available.
#[cfg(not(any(target_os = "linux", windows)))]
mod backend {
    use super::Monitor;
    pub fn enumerate() -> Vec<Box<dyn Monitor>> {
        Vec::new()
    }
}

/// Every monitor on this machine that answers a read of VCP 0x60.
///
/// Slow: measured at 1.09 s on a two-output Linux laptop, because it opens
/// every i2c bus and reads an EDID from each. Never call it from a thread
/// that carries input.
///
/// Answering the read is the membership test on purpose. A laptop's
/// internal eDP panel enumerates as an i2c bus and returns a valid EDID,
/// but it has no input to select and fails the read, so this keeps it out
/// without special-casing panel types.
pub fn enumerate() -> Vec<Box<dyn Monitor>> {
    backend::enumerate()
}

/// The monitor whose `identity()` or `location()` contains `want`
/// (case-insensitive), or the first one when `want` is `None`.
pub fn open(want: Option<&str>) -> Result<Box<dyn Monitor>, DisplayError> {
    pick(enumerate(), want)
}

/// The choice `open` makes, split out so it is testable without hardware.
fn pick(
    mut found: Vec<Box<dyn Monitor>>,
    want: Option<&str>,
) -> Result<Box<dyn Monitor>, DisplayError> {
    if found.is_empty() {
        return Err(DisplayError::NoMonitor);
    }
    let Some(want) = want else {
        if found.len() > 1 {
            tracing::info!(
                count = found.len(),
                picked = found[0].identity(),
                "more than one monitor answered DDC/CI; set display.monitor to choose"
            );
        }
        return Ok(found.remove(0));
    };
    let needle = want.to_lowercase();
    let idx = found.iter().position(|m| {
        m.identity().to_lowercase().contains(&needle)
            || m.location().to_lowercase().contains(&needle)
    });
    match idx {
        Some(i) => Ok(found.remove(i)),
        None => Err(DisplayError::NoMatch(want.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMonitor;

    fn mon(identity: &str, location: &str) -> Box<dyn Monitor> {
        Box::new(MockMonitor::new(identity, location, 0x11).0)
    }

    /// Break it by returning `Ok` on an empty list: the service then
    /// unwraps a monitor that does not exist.
    #[test]
    fn nothing_enumerated_is_no_monitor() {
        assert!(matches!(
            pick(Vec::new(), None),
            Err(DisplayError::NoMonitor)
        ));
    }

    /// Break it by picking `found.pop()`: the choice becomes the last bus
    /// rather than the first, which reorders every person's monitors.
    #[test]
    fn no_preference_picks_the_first() {
        let found = vec![mon("A", "/dev/i2c-4"), mon("B", "/dev/i2c-9")];
        assert_eq!(pick(found, None).unwrap().identity(), "A");
    }

    /// Break it by comparing with `==` instead of `contains`: a person has
    /// to type the whole identity, serial and all.
    #[test]
    fn a_preference_matches_a_substring_of_the_identity() {
        let found = vec![
            mon("GSM LG ULTRAGEAR (106NTMXE1579)", "/dev/i2c-4"),
            mon("DEL DELL U2720Q (ABC123)", "/dev/i2c-9"),
        ];
        assert_eq!(
            pick(found, Some("u2720")).unwrap().identity(),
            "DEL DELL U2720Q (ABC123)"
        );
    }

    /// Break it by dropping the `location` arm: a monitor whose EDID gave
    /// no name can then never be selected, because its identity is its bus
    /// path and only `location` carries it.
    #[test]
    fn a_preference_also_matches_the_location() {
        let found = vec![mon("A", "/dev/i2c-4"), mon("B", "/dev/i2c-9")];
        assert_eq!(pick(found, Some("i2c-9")).unwrap().identity(), "B");
    }

    /// Break it by falling back to the first monitor: pheme then silently
    /// drives a different screen than the one configured.
    #[test]
    fn a_preference_that_matches_nothing_is_an_error() {
        let found = vec![mon("A", "/dev/i2c-4")];
        assert!(matches!(pick(found, Some("zzz")), Err(DisplayError::NoMatch(w)) if w == "zzz"));
    }

    /// Prints what this machine's monitors actually are. Ignored because it
    /// needs hardware that answers DDC/CI, which CI does not have and which
    /// the development machine does not either -- `ddcutil detect` reports
    /// "No displays implementing DDC/CI found" there.
    ///
    /// Run with:
    /// `cargo test -p pheme-display -- --ignored --nocapture manual_smoke`
    #[test]
    #[ignore = "needs a monitor that answers DDC/CI"]
    fn manual_smoke() {
        let start = std::time::Instant::now();
        let mut found = enumerate();
        println!("{} monitor(s) in {:?}", found.len(), start.elapsed());
        for m in found.iter_mut() {
            println!("  {} at {}", m.identity(), m.location());
            println!("    input   {:?}", m.get_input());
            println!("    caps    {:?}", m.capabilities());
        }
    }
}
