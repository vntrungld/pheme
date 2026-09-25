//! The thread that owns the monitor handle.
//!
//! Everything slow about DDC/CI lives here. Enumeration costs about a
//! second and a write costs hundreds of milliseconds, so the router thread
//! only ever does a non-blocking `try_send` into this channel and walks
//! away.

use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use pheme_display::{DisplayError, DisplaySwitch, Monitor};
use tracing::{debug, info, warn};

use crate::config::DisplayCfg;

/// How the service acquires its monitor.
///
/// Injected so the tests can hand it a `MockMonitor`; production passes a
/// closure that has already captured `display.monitor`. A closure's `Send`
/// depends on what it captures, not on what it returns, so this stays
/// `Send` even though `Monitor` is not.
pub type OpenFn = Box<dyn FnOnce() -> Result<Box<dyn Monitor>, DisplayError> + Send>;

enum Req {
    Switch(u16),
    Force(u16),
}

/// Four, the same depth the status channel uses. A full queue means four
/// crossings are already waiting on a monitor that takes seconds to answer,
/// and the oldest of them is no longer anyone's intention.
const QUEUE: usize = 4;

/// Cheap to clone: every clone shares the same channel to the display
/// thread and the same `last` counter, so two owners commanding the same
/// monitor never disagree about what was last confirmed. Needed so the
/// front-end's IPC command loop -- a task independent of any one session --
/// can hold its own handle alongside the one a session already borrows.
#[derive(Clone)]
pub struct DisplayService {
    tx: Sender<Req>,
    /// The input last *successfully* commanded, for the front-end's status
    /// line. `u32::MAX` means "nothing yet", so one atomic carries both
    /// states and the status path takes no lock at all -- it is read once a
    /// second from the thread that also carries input.
    last: Arc<AtomicU32>,
}

/// `last`'s sentinel for "no input has been confirmed".
const NO_INPUT: u32 = u32::MAX;

impl DisplayService {
    /// `None` when the feature is off, that is when `[display] input` is
    /// unset.
    ///
    /// This returns before the monitor is opened, so it cannot and does not
    /// report whether one answered: enumeration costs about a second and
    /// runs on the spawned thread. A thread that finds no monitor logs once
    /// and exits, which disconnects the channel and turns every later
    /// `switch_to` into a dropped send.
    pub fn spawn(cfg: &DisplayCfg, open: OpenFn) -> Option<DisplayService> {
        // Off unless `[display] input` names the input this machine is
        // cabled to. Written with `?` because clippy's `question_mark` lint
        // rejects the longhand `if .is_none() { return None }`.
        cfg.input?;
        let cooldown = Duration::from_millis(cfg.cooldown_ms);
        let (tx, rx) = crossbeam_channel::bounded(QUEUE);
        let last = Arc::new(AtomicU32::new(NO_INPUT));
        let thread_last = Arc::clone(&last);
        if let Err(e) = std::thread::Builder::new()
            .name("pheme-display".into())
            .spawn(move || run(rx, open, cooldown, thread_last))
        {
            warn!(error = %e, "could not start the display thread; switching is off");
            return None;
        }
        Some(DisplayService { tx, last })
    }

    /// The input this machine last successfully commanded, or `None` when
    /// nothing has been. Read by the status path once a second.
    pub fn last_input(&self) -> Option<u16> {
        match self.last.load(Ordering::Relaxed) {
            NO_INPUT => None,
            v => Some(v as u16),
        }
    }

    /// A pointer crossing asks for `value`. Never blocks and never fails.
    pub fn switch_to(&self, value: u16) {
        self.send(Req::Switch(value));
    }

    /// The recovery hotkey asks for `value`, bypassing the policy's dedupe
    /// and cooldown. Never blocks and never fails.
    pub fn force(&self, value: u16) {
        self.send(Req::Force(value));
    }

    fn send(&self, req: Req) {
        // A dropped crossing and a dropped rescue are not the same event.
        // The queue's rationale -- four crossings deep, the oldest is no
        // longer anyone's intention -- is an argument about pointer
        // crossings, and it does not carry over to the hotkey somebody
        // pressed *because* the monitor is already wedged. That one is worth
        // saying out loud.
        let forced = matches!(req, Req::Force(_));
        match self.tx.try_send(req) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) if forced => {
                warn!("the display queue is full; the forced switch was dropped")
            }
            Err(TrySendError::Full(_)) => {
                debug!("the display queue is full; dropping a switch")
            }
            // The thread found no monitor and exited. Ordinary, and already
            // logged once by the thread itself.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn run(rx: Receiver<Req>, open: OpenFn, cooldown: Duration, last: Arc<AtomicU32>) {
    let mut mon = match open() {
        Ok(m) => m,
        Err(e) => {
            // Once, and never retried on any schedule. A machine whose
            // monitor does not speak DDC/CI is an ordinary machine, not a
            // fault worth repeating.
            warn!(error = %e, "display switching is off: no usable monitor");
            return;
        }
    };
    info!(
        monitor = mon.identity(),
        at = mon.location(),
        "display switching is on"
    );
    let mut policy = DisplaySwitch::new(cooldown);
    match mon.get_input() {
        Ok(v) => policy.observe(v),
        Err(e) => debug!(
            error = %e,
            "could not read the current input; the first switch will be issued blind"
        ),
    }
    let mut warned = false;
    loop {
        // `None` here means the deadline fired rather than a request
        // arriving: a request held by the cooldown needs a wakeup that no
        // incoming message will provide. With nothing held, `deadline` is
        // `None` and this blocks indefinitely -- which is also what makes
        // `cooldown_ms = 0` cost no spinning, since nothing is ever held.
        let req = match policy.deadline() {
            Some(t) => match rx.recv_deadline(t) {
                Ok(r) => Some(r),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => break,
            },
        };
        let now = Instant::now();
        let issue = match req {
            Some(Req::Switch(v)) => policy.request(v, now),
            Some(Req::Force(v)) => Some(policy.force(v, now)),
            None => policy.poll(now),
        };
        if let Some(v) = issue {
            apply(mon.as_mut(), &mut policy, v, &mut warned, &last);
        }
    }
}

fn apply(
    mon: &mut dyn Monitor,
    policy: &mut DisplaySwitch,
    value: u16,
    warned: &mut bool,
    last: &AtomicU32,
) {
    match mon.set_input(value) {
        Ok(()) => {
            policy.confirm(value);
            last.store(u32::from(value), Ordering::Relaxed);
            debug!(input = value, "monitor input switched");
        }
        Err(e) => {
            // `forget`, never `confirm`. Recording a value the command did
            // not deliver would leave the policy believing the monitor shows
            // something it does not, and rule 1 would then refuse every
            // attempt to correct it.
            policy.forget();
            if *warned {
                debug!(input = value, error = %e, "setting the monitor input failed");
            } else {
                *warned = true;
                warn!(
                    input = value,
                    error = %e,
                    "setting the monitor input failed; further failures log at debug"
                );
            }
        }
    }
}
