//! Audio wiring between the backends in `pheme-audio` and the QUIC session.
//!
//! Both roles run both directions. Each builds one `SendSide` — a capture device packed
//! into datagrams — and one `RecvSide` — datagrams played into a device. They differ
//! only in which backend they detect and which `AudioStream` tag they carry, so the
//! supervisor shape they share lives here: rebuild a failing backend every five seconds,
//! keep a permanently broken machine from flooding the log, and never let any of it
//! reach `run_client` or `run_server` as an error.

mod recv;
mod send;

pub use recv::{InStats, PlaybackSource, RecvSide};
pub use send::{CaptureSource, OutCounters, SendSide};

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, warn};

/// How long a failed backend waits before it is rebuilt.
pub(crate) const RETRY: Duration = Duration::from_secs(5);
/// How often the supervisor threads wake up.
pub(crate) const TICK: Duration = Duration::from_millis(2);

/// Why a pump returned.
pub(crate) enum PumpEnd {
    /// A stop was requested, or the channel was dropped: do not rebuild.
    Stopped,
    /// The gate closed: stop the device and wait for it to open again. Not a failure, so
    /// it is neither logged as one nor made to serve the five-second retry delay.
    Unwanted,
    /// The device, or something the pump owns, failed: rebuild after the retry delay.
    Failed(String),
}

/// How long a `RecvSide` keeps reporting demand after its last consumer left.
///
/// Applications probe recording devices — enumerate, open briefly, close — and without a
/// debounce each probe would open and close the far end's microphone, which is visible to
/// the user and hard on the device. Only the closing edge waits; a consumer arriving is
/// reported at once.
pub(crate) const LINGER: Duration = Duration::from_secs(3);

/// How long a gated `SendSide` sleeps between checks while its gate is shut.
pub(crate) const GATE_POLL: Duration = Duration::from_millis(20);

/// Logs a repeating failure loudly once and quietly while it repeats unchanged.
///
/// `detect_*` never returns `Unsupported` on Linux or Windows — a daemon that is not
/// running at startup but appears later must still be picked up — so a machine with no
/// usable audio fails the same way every five seconds for the life of the process. At
/// one `warn!` each that is roughly 17 000 lines a day, which buries the input logs
/// this project actually needs. The first failure of a kind stays at `warn!`; identical
/// repeats drop to `debug!`; a success restores the volume.
#[derive(Default)]
pub(crate) struct FailureLog {
    last: Option<String>,
}

impl FailureLog {
    /// Records `msg` and returns whether it deserves a `warn!` rather than a `debug!`.
    pub(crate) fn is_new(&mut self, msg: &str) -> bool {
        if self.last.as_deref() == Some(msg) {
            return false;
        }
        self.last = Some(msg.to_string());
        true
    }

    pub(crate) fn report(&mut self, msg: String) {
        if self.is_new(&msg) {
            warn!("{msg}");
        } else {
            debug!("{msg}");
        }
    }

    /// The backend came up: the next failure is loud again.
    pub(crate) fn cleared(&mut self) {
        self.last = None;
    }
}

/// Sleeps in short steps so `stop` is noticed promptly. Returns false if asked to stop.
pub(crate) fn nap(total: Duration, stop: &AtomicBool) -> bool {
    let mut left = total;
    while left > Duration::ZERO {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let step = left.min(Duration::from_millis(50));
        std::thread::sleep(step);
        left -= step;
    }
    !stop.load(Ordering::SeqCst)
}

/// How long a test waits for a background worker to reach a state.
///
/// Generous on purpose. These tests wait on worker threads that tick every `TICK`, and
/// the budget has to survive a saturated machine — a parallel suite plus another build,
/// or a CI runner with two cores. A test whose condition is met promptly still finishes
/// promptly; the budget only costs wall-clock on the runs that were going to fail.
/// Measured before this was raised: `a_reset_empties_the_buffer` failed 2 times in 12
/// full-suite runs under external load, and never in 30 isolated ones.
#[cfg(test)]
pub(crate) const SETTLE: Duration = Duration::from_secs(10);

/// Polls `f` until it returns true or `timeout` elapses, sleeping briefly between tries.
/// Shared by `send.rs`'s and `recv.rs`'s tests, which both wait on a supervisor thread.
#[cfg(test)]
pub(crate) fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let t = std::time::Instant::now();
    while t.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    f()
}
