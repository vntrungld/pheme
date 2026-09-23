//! The start/stop/healthy handshake every device backend needs, implemented once.
//!
//! Every backend in this crate owns an OS thread that talks to a sound device, and every
//! one of them needs the same four things: a bounded wait for the thread to say it is
//! running, a way to ask it to stop, an honest answer to "is it still alive", and an
//! idempotent teardown. Writing that four times produced two bugs that reached a user —
//! a backend that reported readiness at the wrong moment, and a backend whose health
//! flag could never change — so it is written once here and the backends supply only the
//! parts that differ: the thread body and how that body is asked to stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

use crate::{Error, Result};

/// Handed to a device thread, which calls `ok()` or `fail(e)` exactly once as soon as its
/// device is running or has failed to open.
///
/// "As soon as" is the whole contract. Reporting readiness at the end of a session
/// instead of at its start makes every `start` time out while the device works perfectly,
/// which is invisible to tests and lints and fatal at runtime.
pub struct Ready(mpsc::Sender<Result<()>>);

impl Ready {
    /// The device is open and running.
    pub fn ok(&self) {
        let _ = self.0.send(Ok(()));
    }

    /// The device could not be opened. `start` returns this error verbatim.
    pub fn fail(&self, e: Error) {
        let _ = self.0.send(Err(e));
    }
}

struct Running {
    stop: Box<dyn Fn() + Send>,
    thread: JoinHandle<()>,
    alive: Arc<AtomicBool>,
}

/// Clears `alive` when the device thread's stack is torn down, however it was torn down.
///
/// A guard rather than a statement after the body, so a panic inside a device callback —
/// which unwinds the thread without reaching any statement after it — also flips
/// `healthy()`. A thread that has died is a thread that has died, and the supervisor's
/// rebuild is what brings audio back either way.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// One device thread and its lifecycle.
#[derive(Default)]
pub struct DeviceThread {
    running: Option<Running>,
}

impl DeviceThread {
    pub fn new() -> DeviceThread {
        DeviceThread::default()
    }

    /// Whether a device thread is currently owned. False after a failed or timed-out
    /// `start`, and after `stop`.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Spawns `body` on a thread named `name` and blocks until it reports readiness or
    /// `timeout` elapses. Starting an already-running thread is a no-op.
    ///
    /// On timeout, `stop` is invoked and the thread is **detached, never joined**. A
    /// thread hung inside device construction — a driver that never returns from its
    /// initialise call, a PipeWire loop that has not yet reached the point where it can
    /// observe a stop request — may never act on that request, and joining it would turn
    /// this bounded wait into an unbounded one, which is exactly what the trait contract
    /// promises it is not.
    pub fn start(
        &mut self,
        name: &str,
        timeout: Duration,
        stop: impl Fn() + Send + 'static,
        body: impl FnOnce(Ready) + Send + 'static,
    ) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _alive = AliveGuard(thread_alive);
                body(Ready(ready_tx));
            })
            .map_err(|e| Error::Backend(format!("spawning the {name} thread: {e}")))?;

        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    stop: Box::new(stop),
                    thread,
                    alive,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop();
                warn!(
                    "the {name} thread did not report readiness within {timeout:?}; abandoning \
                     it detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(format!(
                    "the {name} thread did not report readiness within {timeout:?}"
                )))
            }
        }
    }

    /// False once the device thread's stack has been torn down, however it died. A
    /// backend with no thread to lose does not use this type at all.
    pub fn healthy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    /// Idempotent. Asks the thread to stop and joins it.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            (r.stop)();
            let _ = r.thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const QUICK: Duration = Duration::from_millis(500);

    #[test]
    fn a_thread_that_reports_success_is_running_and_healthy() {
        let mut d = DeviceThread::new();
        let (tx, rx) = mpsc::channel::<()>();
        d.start(
            "test-ok",
            QUICK,
            || {},
            move |ready| {
                ready.ok();
                // Hold the thread open until the test drops the sender.
                let _ = rx.recv();
            },
        )
        .expect("start");
        assert!(d.is_running());
        assert!(d.healthy());
        drop(tx);
        d.stop();
    }

    #[test]
    fn a_thread_that_reports_failure_returns_that_error_and_does_not_run() {
        let mut d = DeviceThread::new();
        let e = d
            .start(
                "test-fail",
                QUICK,
                || {},
                |ready| {
                    ready.fail(Error::Device("no such device".into()));
                },
            )
            .expect_err("start must fail");
        assert!(matches!(e, Error::Device(m) if m == "no such device"));
        assert!(!d.is_running(), "a failed start leaves nothing running");
        assert!(!d.healthy());
    }

    #[test]
    fn a_thread_that_never_reports_times_out_and_is_asked_to_stop() {
        let stopped = Arc::new(AtomicBool::new(false));
        let asked = stopped.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let mut d = DeviceThread::new();
        let e = d
            .start(
                "test-hang",
                Duration::from_millis(50),
                move || asked.store(true, Ordering::SeqCst),
                move |_ready| {
                    // Never reports readiness; releases only when the test says so, which
                    // is what makes this a *detached* thread rather than a joined one.
                    let _ = rx.recv();
                },
            )
            .expect_err("start must time out");
        assert!(matches!(e, Error::Backend(_)));
        assert!(!d.is_running(), "a timed-out start owns nothing");
        assert!(
            stopped.load(Ordering::SeqCst),
            "the hung thread must be asked to stop even though it is not joined"
        );
        drop(tx);
    }

    #[test]
    fn a_thread_that_returns_stops_being_healthy() {
        let mut d = DeviceThread::new();
        d.start("test-short", QUICK, || {}, |ready| ready.ok())
            .expect("start");
        // The body returned immediately; `healthy` must notice without a `stop` call,
        // because that is the only signal the supervisor has to rebuild on.
        let mut healthy = true;
        for _ in 0..200 {
            if !d.healthy() {
                healthy = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!healthy, "a thread that ended must report ill health");
        d.stop();
    }

    #[test]
    fn a_thread_that_panics_stops_being_healthy() {
        let mut d = DeviceThread::new();
        d.start(
            "test-panic",
            QUICK,
            || {},
            |ready| {
                ready.ok();
                panic!("a driver callback blew up");
            },
        )
        .expect("start");
        let mut healthy = true;
        for _ in 0..200 {
            if !d.healthy() {
                healthy = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !healthy,
            "an unwinding thread must flip healthy too, or a panicking callback is invisible"
        );
        d.stop();
    }

    #[test]
    fn stop_is_idempotent_and_safe_on_a_thread_that_never_started() {
        let mut d = DeviceThread::new();
        d.stop();
        d.stop();
        assert!(!d.is_running());
    }

    #[test]
    fn starting_an_already_running_thread_is_a_no_op() {
        let mut d = DeviceThread::new();
        let (tx, rx) = mpsc::channel::<()>();
        d.start(
            "test-once",
            QUICK,
            || {},
            move |ready| {
                ready.ok();
                let _ = rx.recv();
            },
        )
        .expect("first start");
        let started_twice = Arc::new(AtomicBool::new(false));
        let flag = started_twice.clone();
        d.start(
            "test-once",
            QUICK,
            || {},
            move |ready| {
                flag.store(true, Ordering::SeqCst);
                ready.ok();
            },
        )
        .expect("second start returns Ok");
        assert!(
            !started_twice.load(Ordering::SeqCst),
            "a second start must not spawn a second device thread"
        );
        drop(tx);
        d.stop();
    }
}
