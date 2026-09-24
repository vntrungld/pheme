//! The lock hotkey on Wayland, through `org.freedesktop.portal.GlobalShortcuts`.
//!
//! A Wayland server sees no keyboard events while it is not capturing, so the X11
//! approach — watch every key and match one — cannot work. This portal is the only
//! alternative, and it comes with a behavioural difference: the compositor owns the
//! binding. `hotkeys.lock` in the configuration is a request, not a decision.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use crossbeam_channel::Sender;
use futures_lite::StreamExt;
use tracing::{info, warn};

/// The shortcut id used in `BindShortcuts` and matched on `Activated`. The portal
/// reports the id back, and a session may hold several shortcuts, so it must match.
const LOCK_ID: &str = "lock";

/// How long `Drop` waits for the shortcut thread to confirm `run()` has returned
/// before detaching it instead of joining.
///
/// `bind_shortcuts` drives the compositor's permission dialog, and a user is free
/// to leave that dialog open indefinitely -- every `.await` in `run()` below is
/// raced against the stop signal so a drop unblocks it almost immediately, but
/// this bound is what keeps a drop from hanging even if some step turns out not
/// to be cancellation-safe. Same shape and same reasoning as `STOP_TIMEOUT` in
/// `portal::mod` (`stop()`), which solved the identical hazard for the
/// InputCapture session thread.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

pub struct LockShortcut {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: async_channel::Sender<()>,
    /// Signalled right after `run()` returns, so `drop()` can bound its wait on
    /// the thread instead of joining blindly.
    done: std::sync::mpsc::Receiver<()>,
}

impl LockShortcut {
    /// Binds the lock shortcut and sends `()` on `toggle` each time it fires.
    ///
    /// `preferred_trigger` uses the portal's shortcut syntax, for example
    /// `"CTRL+ALT+l"`. The compositor may bind something else entirely; what it
    /// chose comes back in the response and is logged, because the user's
    /// configuration file will not match what actually works.
    pub fn bind(preferred_trigger: String, toggle: Sender<()>) -> LockShortcut {
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pheme-shortcut".into())
            .spawn(move || {
                futures_lite::future::block_on(run(preferred_trigger, toggle, stop_rx));
                let _ = done_tx.send(());
            })
            .ok();
        LockShortcut {
            thread,
            stop: stop_tx,
            done: done_rx,
        }
    }
}

impl Drop for LockShortcut {
    /// Sends the stop signal and waits, bounded by `STOP_TIMEOUT`, for `run()` to
    /// return. A hung compositor -- most plausibly a permission dialog nobody
    /// answered -- must never turn dropping this value, and therefore
    /// `run_server`'s return, into an unbounded wait.
    fn drop(&mut self) {
        let _ = self.stop.send_blocking(());
        let Some(thread) = self.thread.take() else {
            return;
        };
        match self.done.recv_timeout(STOP_TIMEOUT) {
            Ok(()) => {
                // `run()` has returned; the thread has nothing left to do but
                // unwind, which is effectively instant.
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Disconnected) => {
                // `done_tx` was dropped without sending, which only happens if the
                // thread ended before reaching the send (e.g. it panicked).
                // Joining an already-finished thread is instant either way.
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                warn!(
                    "the lock-shortcut thread did not confirm it had stopped within \
                     {STOP_TIMEOUT:?}; detaching it rather than blocking shutdown on \
                     a possibly hung compositor call"
                );
                drop(thread);
            }
        }
    }
}

/// Races `fut` against `stop`. `None` if the stop signal fires first, which drops
/// (and so cancels) `fut` rather than waiting for it -- the reason a compositor
/// that never answers `bind_shortcuts` cannot hang a shutdown.
async fn stoppable<T>(
    stop: &async_channel::Receiver<()>,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    futures_lite::future::or(
        async {
            let _ = stop.recv().await;
            None
        },
        async { Some(fut.await) },
    )
    .await
}

async fn run(preferred_trigger: String, toggle: Sender<()>, stop: async_channel::Receiver<()>) {
    // Every failure here is logged and ends the thread. A lock hotkey that could not
    // be bound must never take keyboard and mouse sharing down with it. Every step
    // is raced against `stop` (see `stoppable`) so a shutdown is never left waiting
    // on the compositor -- most importantly `bind_shortcuts`, which drives the
    // permission dialog and can otherwise stay pending for as long as the user
    // leaves it open.
    let Some(portal_result) = stoppable(&stop, GlobalShortcuts::new()).await else {
        return;
    };
    let portal = match portal_result {
        Ok(p) => p,
        Err(e) => return warn!("no GlobalShortcuts portal; the lock hotkey is unavailable: {e}"),
    };
    // `CreateSessionOptions` here is `ashpd::desktop::session::CreateSessionOptions`,
    // which the crate does not re-export (unlike `input_capture`'s own
    // `CreateSessionOptions`), so this leans on inference from `create_session`'s
    // parameter type rather than naming it.
    let Some(session_result) = stoppable(&stop, portal.create_session(Default::default())).await
    else {
        return;
    };
    let session = match session_result {
        Ok(s) => s,
        Err(e) => return warn!("could not create a GlobalShortcuts session: {e}"),
    };
    let shortcut = NewShortcut::new(LOCK_ID, "Lock input to the current screen")
        .preferred_trigger(preferred_trigger.as_str());
    let Some(bind_result) = stoppable(
        &stop,
        portal.bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default()),
    )
    .await
    else {
        return;
    };
    let bound = match bind_result.and_then(|r| r.response()) {
        Ok(b) => b,
        Err(e) => return warn!("binding the lock hotkey failed: {e}"),
    };
    for s in bound.shortcuts() {
        // The trigger the desktop actually assigned, which may differ from the
        // configured one. Without this line a user whose compositor chose something
        // else has no way to find out what.
        info!(
            id = s.id(),
            trigger = s.trigger_description(),
            "lock hotkey bound"
        );
    }

    let Some(activated_result) = stoppable(&stop, portal.receive_activated()).await else {
        return;
    };
    let mut activated = match activated_result {
        Ok(s) => s,
        Err(e) => return warn!("subscribing to shortcut activations failed: {e}"),
    };
    loop {
        let go_on = futures_lite::future::or(
            async {
                let _ = stop.recv().await;
                false
            },
            async {
                match activated.next().await {
                    Some(a) => {
                        if a.shortcut_id() == LOCK_ID && toggle.send(()).is_err() {
                            return false;
                        }
                        true
                    }
                    None => false,
                }
            },
        )
        .await;
        if !go_on {
            break;
        }
    }
}
