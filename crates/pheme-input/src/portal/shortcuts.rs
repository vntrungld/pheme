//! The lock hotkey on Wayland, through `org.freedesktop.portal.GlobalShortcuts`.
//!
//! A Wayland server sees no keyboard events while it is not capturing, so the X11
//! approach — watch every key and match one — cannot work. This portal is the only
//! alternative, and it comes with a behavioural difference: the compositor owns the
//! binding. `hotkeys.lock` in the configuration is a request, not a decision.

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use crossbeam_channel::Sender;
use futures_lite::StreamExt;
use tracing::{info, warn};

/// The shortcut id used in `BindShortcuts` and matched on `Activated`. The portal
/// reports the id back, and a session may hold several shortcuts, so it must match.
const LOCK_ID: &str = "lock";

pub struct LockShortcut {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: async_channel::Sender<()>,
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
        let thread = std::thread::Builder::new()
            .name("pheme-shortcut".into())
            .spawn(move || futures_lite::future::block_on(run(preferred_trigger, toggle, stop_rx)))
            .ok();
        LockShortcut {
            thread,
            stop: stop_tx,
        }
    }
}

impl Drop for LockShortcut {
    fn drop(&mut self) {
        let _ = self.stop.send_blocking(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn run(preferred_trigger: String, toggle: Sender<()>, stop: async_channel::Receiver<()>) {
    // Every failure here is logged and ends the thread. A lock hotkey that could not
    // be bound must never take keyboard and mouse sharing down with it.
    let portal = match GlobalShortcuts::new().await {
        Ok(p) => p,
        Err(e) => return warn!("no GlobalShortcuts portal; the lock hotkey is unavailable: {e}"),
    };
    // `CreateSessionOptions` here is `ashpd::desktop::session::CreateSessionOptions`,
    // which the crate does not re-export (unlike `input_capture`'s own
    // `CreateSessionOptions`), so this leans on inference from `create_session`'s
    // parameter type rather than naming it.
    let session = match portal.create_session(Default::default()).await {
        Ok(s) => s,
        Err(e) => return warn!("could not create a GlobalShortcuts session: {e}"),
    };
    let shortcut = NewShortcut::new(LOCK_ID, "Lock input to the current screen")
        .preferred_trigger(preferred_trigger.as_str());
    let bound = match portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await
        .and_then(|r| r.response())
    {
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

    let mut activated = match portal.receive_activated().await {
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
