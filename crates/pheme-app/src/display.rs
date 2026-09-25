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
///
/// `FnMut`, not `FnOnce`: an open that found nothing is retried at the one
/// instant its answer can have changed, which is when this machine becomes
/// the input the monitor is displaying (see `Req::BecameDisplayed`).
/// `pheme_display::mock::opens_once` is the shape a test wants.
pub type OpenFn = Box<dyn FnMut() -> Result<Box<dyn Monitor>, DisplayError> + Send>;

enum Req {
    Switch(u16),
    Force(u16),
    /// This machine has just become the input the monitor is displaying.
    /// Carries this machine's own `display.input`.
    BecameDisplayed(u16),
}

/// How many times a monitor is opened before the service gives up for good:
/// once at startup, then once at each of the first two moments this machine
/// becomes the displayed input.
///
/// A bound is needed because DDC/CI is answered only by the displayed
/// input, so an enumeration that found nothing at startup may simply have
/// been run on the machine that was off screen. Retrying at every crossing
/// for ever would cost a second of the display thread on every crossing on
/// the very common hardware that answers nothing at all; three attempts is
/// enough to cover a monitor that was mid-switch or re-syncing on the first
/// one, and after them the thread exits exactly as design section 7 says --
/// nothing retries on a schedule, and nothing retries for ever.
const OPEN_ATTEMPTS: u32 = 3;

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
    /// line, or one of two sentinels: `NO_INPUT` for "nothing yet" and
    /// `NO_MONITOR` for "nothing answered DDC/CI". One atomic carries all
    /// three, so the status path takes no lock at all -- it is read once a
    /// second from the thread that also carries input.
    last: Arc<AtomicU32>,
}

/// `last`'s sentinel for "no input has been confirmed".
const NO_INPUT: u32 = u32::MAX;

/// `last`'s sentinel for "no monitor answered, so nothing can be
/// commanded".
///
/// Distinct from `NO_INPUT` because the two read identically to a person
/// and mean opposite things: one is a feature waiting for its first
/// crossing, the other a feature that will never do anything on this
/// hardware. Neither can collide with a real value, which is a `u16`.
const NO_MONITOR: u32 = u32::MAX - 1;

impl DisplayService {
    /// `None` when the feature is off, that is when `[display] input` is
    /// unset.
    ///
    /// This returns before the monitor is opened, so it cannot and does not
    /// report whether one answered: enumeration costs about a second and
    /// runs on the spawned thread. A thread that finds no monitor says so
    /// once and holds no handle; every `switch_to` it is then sent is
    /// dropped, and it tries again only when `became_displayed` says this
    /// machine is on screen, at most `OPEN_ATTEMPTS` times in all. When
    /// those run out it exits, which disconnects the channel and turns
    /// every later send into a dropped one.
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
            NO_INPUT | NO_MONITOR => None,
            v => Some(v as u16),
        }
    }

    /// Whether the last attempt to open a monitor found none.
    ///
    /// The difference between "on, and nothing has been commanded yet" and
    /// "on, and nothing ever will be" -- which is the common case on
    /// hardware that ignores DDC/CI, and the one a person needs told. The
    /// service knows it; without this it threw it away.
    pub fn no_monitor(&self) -> bool {
        self.last.load(Ordering::Relaxed) == NO_MONITOR
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

    /// This machine has just become the input the monitor is displaying,
    /// and `own_input` is the value it is cabled to. Never blocks and never
    /// fails.
    ///
    /// The peer commands the same monitor, so the policy's belief about
    /// what is on screen goes stale every time the peer switches it. This
    /// is the one moment that belief can be refreshed: the monitor is
    /// listening to this machine's cable, so it will answer a read, and a
    /// machine whose startup enumeration found nothing because it was off
    /// screen can finally find something.
    ///
    /// The server calls it when the pointer returns to local -- the client
    /// has just commanded the monitor to this machine's input -- and the
    /// client on `Msg::Enter`, where the server has just done the same.
    pub fn became_displayed(&self, own_input: u16) {
        self.send(Req::BecameDisplayed(own_input));
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

fn run(rx: Receiver<Req>, mut open: OpenFn, cooldown: Duration, last: Arc<AtomicU32>) {
    let mut policy = DisplaySwitch::new(cooldown);
    let mut attempts = 0u32;
    let mut mon = open_monitor(&mut open, &mut attempts, &last);
    if let Some(m) = mon.as_mut() {
        match m.get_input() {
            Ok(v) => policy.observe(v),
            Err(e) => debug!(
                error = %e,
                "could not read the current input; the first switch will be issued blind"
            ),
        }
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
            Some(Req::BecameDisplayed(own)) => {
                if mon.is_none() {
                    mon = open_monitor(&mut open, &mut attempts, &last);
                    if mon.is_none() && attempts >= OPEN_ATTEMPTS {
                        warn!(
                            attempts,
                            "display switching stays off: no monitor answered DDC/CI even \
                             with this machine on screen"
                        );
                        return;
                    }
                }
                if mon.is_some() {
                    // `own`, and deliberately not a fresh `get_input()`.
                    // The event itself is the stronger evidence: it means
                    // the peer has just commanded the monitor to this
                    // machine's cable, so this machine's own input is what
                    // is on screen by construction.
                    //
                    // A read here races that command. The peer issues it
                    // on *its* display thread and sends the crossing at
                    // once, so a read taken on this side can still return
                    // the peer's input -- and observing that value is the
                    // very defect this hook exists to fix, because rule 1
                    // would then refuse this machine's next correct
                    // command. On the hardware design section 2 describes
                    // the read would merely fail and fall back to `own`,
                    // but on a monitor that answers from a non-displayed
                    // input (section 15) it succeeds with a stale value,
                    // and `display_crossing.rs` fails on it deterministically.
                    //
                    // Believing `own` wrongly cannot wedge anything: the
                    // only command rule 1 can refuse is one for this
                    // machine's own input, and a crossing never asks for
                    // that -- it asks for the peer's. The recovery hotkey
                    // bypasses rule 1 outright.
                    policy.observe(own);
                    debug!(input = own, "this machine is the displayed input");
                }
                None
            }
            None => policy.poll(now),
        };
        if let (Some(v), Some(m)) = (issue, mon.as_mut()) {
            apply(m.as_mut(), &mut policy, v, &mut warned, &last);
        }
    }
}

/// One attempt at acquiring a monitor, counted against `OPEN_ATTEMPTS`.
///
/// A failure is ordinary: a machine whose monitor does not speak DDC/CI is
/// an ordinary machine. It is said once at `warn` and afterwards at
/// `debug`, so a person sees it without a retry filling the log.
fn open_monitor(
    open: &mut OpenFn,
    attempts: &mut u32,
    last: &AtomicU32,
) -> Option<Box<dyn Monitor>> {
    *attempts += 1;
    match open() {
        Ok(m) => {
            info!(
                monitor = m.identity(),
                at = m.location(),
                "display switching is on"
            );
            // A retry that succeeded: the front-end is told the feature is
            // alive again, not that no monitor answered. Only this thread
            // writes `last`, so a plain load and store cannot race.
            if last.load(Ordering::Relaxed) == NO_MONITOR {
                last.store(NO_INPUT, Ordering::Relaxed);
            }
            Some(m)
        }
        Err(e) if *attempts == 1 => {
            last.store(NO_MONITOR, Ordering::Relaxed);
            warn!(
                error = %e,
                "display switching is off: no usable monitor. DDC/CI is answered only by \
                 the input the monitor is showing, so this is retried if this machine \
                 becomes that input"
            );
            None
        }
        Err(e) => {
            last.store(NO_MONITOR, Ordering::Relaxed);
            debug!(error = %e, attempt = *attempts, "still no usable monitor");
            None
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
