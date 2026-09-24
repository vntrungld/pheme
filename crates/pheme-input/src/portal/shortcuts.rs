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

/// Turns the configured `hotkeys.lock` value into the portal's trigger syntax.
///
/// The two are not the same language. `hotkeys.lock` is a name from pheme's own key
/// table (`keymap::table`) — the default is `"ScrollLock"`. The XDG shortcuts
/// specification's `preferred_trigger` is an XKB keysym name with `CTRL+`, `SHIFT+`,
/// `ALT+` and `SUPER+` prefixes, where that key is `Scroll_Lock`. Handing the portal a
/// pheme name asks for a key that does not exist, and the shipped default did exactly
/// that.
///
/// The rules, in order:
///
/// 1. A value containing `+` is already a portal trigger — no pheme key name contains
///    one — and is passed through untouched, so a user can write `CTRL+ALT+l` and get
///    precisely that.
/// 2. A pheme key name whose keysym spelling differs is translated (the table below).
///    The comparison ignores case, because `keymap::key_by_name` — which is what decides
///    whether `hotkeys.lock` is valid configuration at all — lower-cases its input. Two
///    functions disagreeing about case would make `lock = "scrolllock"` a valid setting
///    that reaches the portal untranslated, as a name no keysym table has.
/// 3. A single letter becomes its lower-case keysym (`A` → `a`).
/// 4. Anything else is passed through: the names that are already identical in both
///    languages (`F1`, `Home`, `Escape`, the digits), and any keysym a user wrote by
///    hand that this table has never heard of. Passing an unknown value through is the
///    only choice that cannot make a working configuration stop working.
///
/// Nothing here can tell a mistyped pheme name from a keysym pheme does not know, so a
/// trigger the compositor refuses is still possible — which is why `run` warns when
/// the bind comes back without the lock shortcut in it.
pub fn portal_trigger(configured: &str) -> String {
    /// (pheme key-table name, XKB keysym name), for every key whose spelling differs.
    const RENAMED: &[(&str, &str)] = &[
        ("ScrollLock", "Scroll_Lock"),
        ("CapsLock", "Caps_Lock"),
        ("NumLock", "Num_Lock"),
        ("PrintScreen", "Print"),
        ("Enter", "Return"),
        ("Space", "space"),
        ("Backspace", "BackSpace"),
        ("PageUp", "Page_Up"),
        ("PageDown", "Page_Down"),
        ("Application", "Menu"),
        ("LeftCtrl", "Control_L"),
        ("RightCtrl", "Control_R"),
        ("LeftShift", "Shift_L"),
        ("RightShift", "Shift_R"),
        ("LeftAlt", "Alt_L"),
        ("RightAlt", "Alt_R"),
        ("LeftGui", "Super_L"),
        ("RightGui", "Super_R"),
        ("Minus", "minus"),
        ("Equal", "equal"),
        ("LeftBracket", "bracketleft"),
        ("RightBracket", "bracketright"),
        ("Backslash", "backslash"),
        ("Semicolon", "semicolon"),
        ("Apostrophe", "apostrophe"),
        ("Grave", "grave"),
        ("Comma", "comma"),
        ("Period", "period"),
        ("Slash", "slash"),
        ("KPDivide", "KP_Divide"),
        ("KPMultiply", "KP_Multiply"),
        ("KPSubtract", "KP_Subtract"),
        ("KPAdd", "KP_Add"),
        ("KPEnter", "KP_Enter"),
        ("KPDecimal", "KP_Decimal"),
        ("KPEqual", "KP_Equal"),
        ("KPComma", "KP_Separator"),
        ("KP0", "KP_0"),
        ("KP1", "KP_1"),
        ("KP2", "KP_2"),
        ("KP3", "KP_3"),
        ("KP4", "KP_4"),
        ("KP5", "KP_5"),
        ("KP6", "KP_6"),
        ("KP7", "KP_7"),
        ("KP8", "KP_8"),
        ("KP9", "KP_9"),
    ];

    if configured.contains('+') {
        return configured.to_string();
    }
    if let Some((_, keysym)) = RENAMED
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(configured))
    {
        return (*keysym).to_string();
    }
    let mut chars = configured.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_lowercase().to_string(),
        _ => configured.to_string(),
    }
}

impl LockShortcut {
    /// Binds the lock shortcut and sends `()` on `toggle` each time it fires.
    ///
    /// `configured` is the raw `hotkeys.lock` value; `portal_trigger` turns it into the
    /// `preferred_trigger` the portal wants. The compositor may bind something else
    /// entirely; what it chose comes back in the response and is logged, because the
    /// user's configuration file will not match what actually works.
    pub fn bind(configured: String, toggle: Sender<()>) -> LockShortcut {
        let preferred_trigger = portal_trigger(&configured);
        if preferred_trigger != configured {
            info!(
                configured = %configured,
                trigger = %preferred_trigger,
                "translated the configured lock hotkey into the portal's trigger syntax"
            );
        }
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
    // Subscribe before binding. `BindShortcuts` is what makes the shortcut exist, so an
    // `Activated` can follow it immediately -- the user may already be leaning on the
    // key when the permission dialog closes. Subscribing afterwards leaves a window in
    // which that activation is simply lost.
    let Some(activated_result) = stoppable(&stop, portal.receive_activated()).await else {
        return;
    };
    let mut activated = match activated_result {
        Ok(s) => s,
        Err(e) => return warn!("subscribing to shortcut activations failed: {e}"),
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
    let mut bound_lock = false;
    for s in bound.shortcuts() {
        // The trigger the desktop actually assigned, which may differ from the
        // configured one. Without this line a user whose compositor chose something
        // else has no way to find out what.
        info!(
            id = s.id(),
            trigger = s.trigger_description(),
            "lock hotkey bound"
        );
        bound_lock |= s.id() == LOCK_ID;
    }
    if !bound_lock {
        // The portal specification says the returned array "includes the set of all
        // shortcuts and the empty set", so a bind that bound nothing succeeds and
        // returns an empty list. Logging only inside the loop above would then produce
        // no line at all, and the user would be left with a lock hotkey that silently
        // does nothing -- against the README's promise that what got bound is logged.
        // The most likely cause is a trigger the compositor could not parse, so name it.
        warn!(
            requested = %preferred_trigger,
            shortcut = LOCK_ID,
            "the compositor bound no lock hotkey. The requested trigger uses the \
             portal's syntax (XKB keysym names, with CTRL+/SHIFT+/ALT+/SUPER+ \
             prefixes) -- check that hotkeys.lock names a key it accepts. The lock \
             hotkey is unavailable until then"
        );
    }

    loop {
        let go_on = futures_lite::future::or(
            async {
                let _ = stop.recv().await;
                false
            },
            async {
                match activated.next().await {
                    Some(a) => {
                        if a.shortcut_id() != LOCK_ID {
                            return true;
                        }
                        // `try_send`, never a blocking send: this future is raced
                        // against the stop signal, and a blocking send on a full
                        // channel would sit here holding the race open while the
                        // receiver is being shut down. Dropping a toggle is a missed
                        // keypress the user can repeat; a wedged shutdown is not.
                        match toggle.try_send(()) {
                            Ok(()) => true,
                            Err(crossbeam_channel::TrySendError::Full(())) => {
                                warn!(
                                    "the lock toggle channel is full; dropping a lock hotkey press"
                                );
                                true
                            }
                            // Nobody is left to toggle anything.
                            Err(crossbeam_channel::TrySendError::Disconnected(())) => false,
                        }
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

#[cfg(test)]
mod tests {
    use super::portal_trigger;

    #[test]
    fn the_shipped_default_becomes_a_keysym_the_portal_knows() {
        // `hotkeys.lock` defaults to "ScrollLock", which is a pheme key-table name.
        // Sent to the portal verbatim it names no key at all, and the compositor binds
        // nothing -- the shipped default was dead on Wayland.
        assert_eq!(portal_trigger("ScrollLock"), "Scroll_Lock");
    }

    #[test]
    fn a_portal_trigger_written_by_hand_is_left_alone() {
        // Modifier prefixes mean the user is already speaking the portal's language.
        assert_eq!(portal_trigger("CTRL+ALT+l"), "CTRL+ALT+l");
        assert_eq!(portal_trigger("SUPER+Scroll_Lock"), "SUPER+Scroll_Lock");
    }

    #[test]
    fn names_that_are_the_same_in_both_languages_pass_through() {
        for name in ["F1", "F12", "Home", "End", "Escape", "Tab", "Pause", "1"] {
            assert_eq!(portal_trigger(name), name);
        }
    }

    #[test]
    fn letters_and_the_remaining_renames() {
        assert_eq!(portal_trigger("A"), "a");
        assert_eq!(portal_trigger("Enter"), "Return");
        assert_eq!(portal_trigger("PageUp"), "Page_Up");
        assert_eq!(portal_trigger("LeftGui"), "Super_L");
        assert_eq!(portal_trigger("KP5"), "KP_5");
    }

    #[test]
    fn a_differently_cased_name_is_translated_like_the_canonical_spelling() {
        // `keymap::key_by_name` lower-cases what it is given, so `lock = "scrolllock"`
        // is valid configuration. A case-sensitive rename table would leave exactly
        // that value untranslated and hand the portal a name no keysym table has.
        assert_eq!(portal_trigger("scrolllock"), "Scroll_Lock");
        assert_eq!(portal_trigger("PAGEUP"), "Page_Up");
        assert_eq!(portal_trigger("leftGui"), "Super_L");
    }

    #[test]
    fn an_unknown_name_is_passed_through_rather_than_guessed_at() {
        // Better a trigger the compositor rejects -- which `run` now warns about --
        // than a silently different key.
        assert_eq!(portal_trigger("NoSuchKey"), "NoSuchKey");
    }
}
