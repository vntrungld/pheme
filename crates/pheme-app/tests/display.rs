//! `DisplayService` driven end to end over a `MockMonitor`.
//!
//! The service runs on its own thread, so every assertion waits for the
//! effect rather than assuming it has happened. Asserting immediately would
//! produce a test that passes or fails on timing, which is worse than one
//! that fails honestly.
//!
//! Where a test needs to know that something did *not* happen, it does not
//! sleep and look: it sends a later request whose effect is visible, waits
//! for that, and then counts. The channel is FIFO and one thread drains it,
//! so the later effect proves the earlier request was already handled.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pheme_app::config::DisplayCfg;
use pheme_app::display::DisplayService;
use pheme_display::mock::{opens_once, MockMonitor, MockMonitorHandle};
use pheme_display::DisplayError;

fn cfg(input: u16, cooldown_ms: u64) -> DisplayCfg {
    DisplayCfg {
        input: Some(input),
        monitor: None,
        cooldown_ms,
    }
}

/// A service over a mock that starts on `start_input`.
fn service(cooldown_ms: u64, start_input: u16) -> (DisplayService, MockMonitorHandle) {
    let (mon, handle) = MockMonitor::new("MOCK", "mock", start_input);
    let svc = DisplayService::spawn(&cfg(0x11, cooldown_ms), Box::new(opens_once(mon)))
        .expect("the feature is configured on");
    (svc, handle)
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what} did not happen within 5 s");
}

/// Break it by removing `spawn`'s `cfg.input?;` guard: pheme then starts a
/// thread and enumerates monitors for every person who never asked for this
/// feature, costing a second of startup each time.
#[test]
fn an_unconfigured_input_means_no_service() {
    let (mon, _handle) = MockMonitor::new("MOCK", "mock", 0x11);
    let off = DisplayCfg {
        input: None,
        monitor: None,
        cooldown_ms: 1000,
    };
    assert!(DisplayService::spawn(&off, Box::new(opens_once(mon))).is_none());
}

/// Review Focus 4, in its local form: no monitor to talk to.
///
/// Break it by opening the monitor inside `spawn` instead of on the thread:
/// the `OpenFn` below blocks until the test releases it, and the test can
/// only release it after `spawn` has returned. An inline open therefore
/// leaves the gate unreleased and `released` false. That matters because
/// enumeration costs about a second, and it would be a second of every
/// start, spent before the input path exists.
///
/// `released` is the one load-bearing assertion here. Of the other three,
/// `opens == 1` says that a `switch_to` or a `force` never retries the
/// open -- which is worth saying now that `became_displayed` does, and is
/// broken by moving the retry into the `Switch` arm; the two-second bound
/// cannot fail, because a send that is dropped returns at once whether it
/// blocks or not; and `last_input()` is `None` because that is the
/// sentinel every implementation starts at.
///
/// What this test deliberately does *not* claim: it cannot catch an
/// implementation that unwraps the `OpenFn`'s result. That panic happens on
/// a detached thread, and the test harness fails a test only for a panic on
/// its own thread. The observable aftermath is identical either way -- the
/// receiver is dropped and every send is refused -- so the panic is invisible
/// from here.
#[test]
fn a_failing_open_leaves_a_service_that_does_nothing() {
    let (gate_tx, gate_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let opens = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let (o, r) = (Arc::clone(&opens), Arc::clone(&released));
    let svc = DisplayService::spawn(
        &cfg(0x11, 1000),
        Box::new(move || {
            // Times out rather than waiting forever so an inline open fails
            // the test instead of hanging it.
            r.store(
                gate_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                Ordering::SeqCst,
            );
            o.fetch_add(1, Ordering::SeqCst);
            Err(DisplayError::NoMonitor)
        }),
    )
    .expect("the feature is configured on");

    // Reached while the open is still blocked. A rendezvous channel, so this
    // returns only once the service thread has taken it.
    gate_tx
        .send(())
        .expect("the service thread should be waiting inside the OpenFn");
    wait_for("the open to be attempted", || {
        opens.load(Ordering::SeqCst) == 1
    });
    assert!(
        released.load(Ordering::SeqCst),
        "spawn must return before the monitor is opened"
    );

    let start = Instant::now();
    for _ in 0..10 {
        svc.switch_to(0x0f);
        svc.force(0x0f);
    }
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "no call on the input path may block"
    );
    assert_eq!(svc.last_input(), None, "nothing was ever switched");
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "a monitor that does not answer is not retried on any schedule, and \
         switching or forcing is not a schedule"
    );
}

/// Break it by having the thread ignore `Req::Switch`: nothing ever
/// reaches the monitor.
#[test]
fn a_switch_reaches_the_monitor() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the input to change", || handle.input() == 0x0f);
}

/// Break it by calling `policy.confirm` instead of `policy.forget` on a
/// failed `set_input`: the retry is then refused by rule 1 and the monitor
/// stays wrong forever. This is the `ClipSync` defect from sub-project 5,
/// pinned end to end rather than on the policy alone.
#[test]
fn a_failed_command_does_not_refuse_the_retry() {
    let (svc, handle) = service(0, 0x11);
    handle.fail_with("i2c timeout");
    svc.switch_to(0x0f);
    wait_for("the failing attempt", || handle.sets() == 1);
    assert_eq!(
        handle.input(),
        0x11,
        "a failed set must not change the input"
    );

    handle.stop_failing();
    svc.switch_to(0x0f);
    wait_for("the retry to land", || handle.input() == 0x0f);
    assert_eq!(handle.sets(), 2);
}

/// Break it by routing `force` through `policy.request`: the deduplicated
/// value is refused and the recovery path does nothing at all.
///
/// The repeat and the force are both followed by a switch to a third value,
/// and only that value's arrival is waited for. One thread drains a FIFO
/// channel, so seeing the third value proves the first two were handled,
/// and the count is then exact. Sleeping a fixed time instead would prove
/// nothing on a loaded machine: a service that had not yet dequeued the
/// repeat looks exactly like one that refused it, so the wrong count would
/// read as the right one.
#[test]
fn force_reaches_the_monitor_where_a_switch_would_be_deduplicated() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    let after_first = handle.sets();

    // Deduplicated: the monitor is already on 0x0f.
    svc.switch_to(0x0f);
    // Forced: the same value, commanded anyway.
    svc.force(0x0f);
    // A marker, so the two above are known to be behind us.
    svc.switch_to(0x12);

    wait_for("the marker switch", || handle.input() == 0x12);
    assert_eq!(
        handle.sets(),
        after_first + 2,
        "the repeat must be deduplicated and the force must be commanded"
    );
}

/// Review Focus 2. Break it by treating a zero cooldown as "always in
/// cooldown": the second value is held behind a deadline that has already
/// passed, and the loop re-arms it forever without ever issuing it.
///
/// Two things this test is not. The break it names is not in this crate: it
/// is `DisplaySwitch::in_cooldown` in `crates/pheme-display/src/switch.rs`,
/// where `saturating_duration_since(t) < self.cooldown` is false for a zero
/// cooldown. From the service's side, what this covers is a loop that
/// stalls after its first command. And it does not reach the `poll` arm:
/// with `cooldown_ms = 0` nothing is ever pending, so deleting
/// `None => policy.poll(now)` leaves this test green.
/// `a_request_made_during_the_cooldown_still_arrives` is that arm's only
/// cover.
#[test]
fn a_zero_cooldown_switches_every_time() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    svc.switch_to(0x12);
    wait_for("the second switch", || handle.input() == 0x12);
}

/// Break it by storing the value before `set_input` rather than after it:
/// the status line then reports an input the monitor refused, which is
/// exactly the reading a person uses to tell "switched" from "ignored".
#[test]
fn only_a_confirmed_switch_is_reported() {
    let (svc, handle) = service(0, 0x11);
    assert_eq!(svc.last_input(), None);

    handle.fail_with("i2c timeout");
    svc.switch_to(0x0f);
    wait_for("the failing attempt", || handle.sets() == 1);
    assert_eq!(
        svc.last_input(),
        None,
        "a refused command must not be reported"
    );

    handle.stop_failing();
    svc.switch_to(0x0f);
    wait_for("the retry to land", || handle.input() == 0x0f);
    wait_for("the report", || svc.last_input() == Some(0x0f));
}

/// Rule 3 through the real thread: a value asked for during the cooldown
/// is issued when the cooldown ends, not dropped. Break it by dropping the
/// held request in the service loop, or by passing `rx.recv()` where the
/// deadline arm belongs: the second value never arrives.
///
/// The only test covering the `poll` arm and the deadline wakeup at all.
/// Every other test here runs with `cooldown_ms = 0`, where nothing is ever
/// held and deleting `None => policy.poll(now)` changes nothing. Do not
/// weaken it without replacing that cover.
///
/// The precondition is asserted rather than assumed. Holding only happens
/// if the second request reaches the policy inside the cooldown; a >200 ms
/// preemption on a loaded runner would put it outside, where it is issued
/// at once and the test passes while testing nothing. `t0` is taken before
/// the first command is issued, so it is an upper bound on the time since
/// the command went out.
#[test]
fn a_request_made_during_the_cooldown_still_arrives() {
    let (svc, handle) = service(200, 0x11);
    let t0 = Instant::now();
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "the second request must be sent inside the cooldown for it to be held"
    );
    svc.switch_to(0x12);
    wait_for("the held switch", || handle.input() == 0x12);
}

/// Critical 2 from the final review, in its local form: on the hardware
/// design section 2 is premised on, the machine that must command the
/// monitor is the one that cannot open it at startup, because the monitor
/// was showing the other machine at the time. It gets another go at the
/// one instant the answer can have changed.
///
/// Break it by deleting the `open_monitor` call in the `BecameDisplayed`
/// arm: the second open never happens, `opens` stays at 1, and the later
/// switch reaches nothing. Break it just as well by leaving the `OpenFn`
/// as `FnOnce` -- which is how this shipped, and is why it needed a
/// finding to notice.
#[test]
fn an_open_that_found_nothing_is_retried_when_this_machine_is_displayed() {
    let (mon, handle) = MockMonitor::new("MOCK", "mock", 0x11);
    let opens = Arc::new(AtomicUsize::new(0));
    let o = Arc::clone(&opens);
    let mut slot = Some(mon);
    let svc = DisplayService::spawn(
        &cfg(0x0f, 0),
        Box::new(move || {
            // Fails the first time the way a real enumeration does when
            // this machine is not the input on screen, and answers once it
            // is.
            if o.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(DisplayError::NoMonitor);
            }
            match slot.take() {
                Some(m) => Ok(Box::new(m) as Box<dyn pheme_display::Monitor>),
                None => Err(DisplayError::NoMonitor),
            }
        }),
    )
    .expect("the feature is configured on");

    // Nothing to command yet: the startup open found no monitor.
    svc.switch_to(0x0f);
    assert_eq!(handle.sets(), 0);

    svc.became_displayed(0x0f);
    wait_for("the second open", || opens.load(Ordering::SeqCst) == 2);
    // And the service works from here on, which is the whole point of
    // retrying at all.
    svc.switch_to(0x12);
    wait_for("the switch after the reopen", || handle.input() == 0x12);
}

/// The bound on that retry. Design section 7 says nothing retries on a
/// schedule and nothing retries for ever, and a crossing is not a schedule
/// but it is unbounded: on the very common hardware that answers no
/// DDC/CI at all, an unbounded retry would spend a second of the display
/// thread on every crossing for the life of the program.
///
/// Break it by dropping the `attempts >= OPEN_ATTEMPTS` check: the thread
/// never gives up, so it never exits, the `OpenFn` is never dropped, and
/// `gone` stays false while `opens` climbs past three.
///
/// The exit is observed through the `OpenFn`'s own drop rather than a
/// sleep. A thread that has given up is gone, so nothing it could report
/// is left to wait for; dropping the boxed closure is the last thing the
/// thread does, and it is the one event that cannot happen early.
#[test]
fn a_monitor_that_never_answers_stops_being_reopened() {
    struct Bell(Arc<AtomicBool>);
    impl Drop for Bell {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let opens = Arc::new(AtomicUsize::new(0));
    let gone = Arc::new(AtomicBool::new(false));
    let (o, bell) = (Arc::clone(&opens), Bell(Arc::clone(&gone)));
    let svc = DisplayService::spawn(
        &cfg(0x11, 0),
        Box::new(move || {
            let _keep = &bell;
            o.fetch_add(1, Ordering::SeqCst);
            Err(DisplayError::NoMonitor)
        }),
    )
    .expect("the feature is configured on");

    for _ in 0..20 {
        svc.became_displayed(0x11);
    }
    wait_for("the service thread to give up", || {
        gone.load(Ordering::SeqCst)
    });
    assert_eq!(
        opens.load(Ordering::SeqCst),
        3,
        "three attempts in all: one at startup and one at each of the first two \
         crossings that put this machine on screen"
    );
}
