//! The system tray icon and its menu.
//!
//! Everything here is mechanical translation: [`Tray::set_state`] mirrors
//! [`CoreState`] onto the icon and the menu, and [`Tray::poll`] turns a
//! click on one of the menu items into a [`TrayEvent`] for the caller to
//! act on. The tray never invents state of its own -- the lock checkmark in
//! particular always shows `Status.locked` as last reported by the core,
//! never a guess made the moment `ToggleLock` was sent.

use std::io::Cursor;

use tracing::warn;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::ipc::LinkState;

use super::CoreState;

const ICON_CONNECTED_PNG: &[u8] = include_bytes!("../../assets/tray-connected.png");
const ICON_DISCONNECTED_PNG: &[u8] = include_bytes!("../../assets/tray-disconnected.png");

const ID_OPEN: &str = "open";
const ID_LOCK: &str = "lock";
const ID_START_STOP: &str = "start_stop";
const ID_QUIT: &str = "quit";

/// What the tray's menu asked the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Show the configuration window.
    Open,
    /// Flip the lock, subject to the core actually doing it -- the
    /// checkmark does not move until the core's next `Status` says so.
    ToggleLock,
    /// Stop the running core, or start one, depending on which the menu
    /// was showing when it was clicked.
    StartStop,
    /// Exit the whole application, tray included.
    Quit,
}

/// The system tray icon and its menu.
///
/// Built from two icons embedded with `include_bytes!` -- there is nothing
/// to install alongside the binary for them -- and a menu whose lock
/// checkmark and Start/Stop label [`Tray::set_state`] keeps in step with
/// [`CoreState`].
pub struct Tray {
    icon: TrayIcon,
    icon_connected: Icon,
    icon_disconnected: Icon,
    lock: CheckMenuItem,
    start_stop: MenuItem,
    /// The state last actually applied to the icon and the menu, so a
    /// caller that calls `set_state` on every frame does not rewrite the
    /// icon file (Linux writes it to disk) and the menu text that often.
    applied: Option<Applied>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Applied {
    connected: bool,
    locked: bool,
    running: bool,
}

impl Tray {
    /// Builds the tray icon, or returns `None` if this platform has nowhere
    /// to put it.
    ///
    /// On Linux, that is the ordinary case for GNOME: it does not ship the
    /// AppIndicator shell extension, so nothing is listening for the
    /// StatusNotifierItem the icon would register as. `tray-icon`'s own
    /// `build()` does not notice this -- the AppIndicator registration
    /// succeeds whether or not a host is watching for it, so a build that
    /// only checked `Result` would return `Some` on GNOME and then silently
    /// show nothing. This checks for a host on the session bus first, so a
    /// missing one is logged once, here, and treated as the same "no tray"
    /// outcome as a build failure: the caller opens the window instead.
    pub fn new() -> Option<Tray> {
        if !tray_host_available() {
            warn!(
                "no system tray is available (GNOME without the AppIndicator \
                 extension is the usual reason on Linux); continuing without one"
            );
            return None;
        }
        match Self::build() {
            Ok(tray) => Some(tray),
            Err(err) => {
                warn!("could not create the tray icon, continuing without one: {err:#}");
                None
            }
        }
    }

    fn build() -> anyhow::Result<Tray> {
        let icon_connected = decode_icon(ICON_CONNECTED_PNG);
        let icon_disconnected = decode_icon(ICON_DISCONNECTED_PNG);

        let open = MenuItem::with_id(ID_OPEN, "Open", true, None);
        let lock = CheckMenuItem::with_id(ID_LOCK, "Lock input", false, false, None);
        let start_stop = MenuItem::with_id(ID_START_STOP, "Start", false, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let menu = Menu::with_items(&[&open, &lock, &start_stop, &quit])?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon_disconnected.clone())
            .with_tooltip("Pheme")
            .build()?;

        Ok(Tray {
            icon,
            icon_connected,
            icon_disconnected,
            lock,
            start_stop,
            applied: None,
        })
    }

    /// Mirrors the tray's icon and menu to the core's own state.
    ///
    /// The icon shows connected only once the core reports the link itself
    /// as `LinkState::Connected` -- a core that is merely running but still
    /// listening or connecting is shown disconnected, since that is what it
    /// is from the other end's point of view. The lock checkmark and the
    /// Start/Stop label follow `CoreState` the same way: never set from the
    /// event a menu click produced, only from the state that came back.
    pub fn set_state(&mut self, s: &CoreState) {
        let (running, locked) = match s {
            CoreState::Running(status) => (true, status.locked),
            CoreState::NoConfig | CoreState::Stopped(_) => (false, false),
        };
        let connected =
            matches!(s, CoreState::Running(status) if status.state == LinkState::Connected);

        let next = Applied {
            connected,
            locked,
            running,
        };
        if self.applied == Some(next) {
            return;
        }

        if self.applied.map(|a| a.connected) != Some(connected) {
            let icon = if connected {
                self.icon_connected.clone()
            } else {
                self.icon_disconnected.clone()
            };
            if let Err(err) = self.icon.set_icon(Some(icon)) {
                warn!("could not update the tray icon: {err}");
            }
        }
        self.lock.set_checked(locked);
        self.lock.set_enabled(running);
        self.start_stop
            .set_text(if running { "Stop" } else { "Start" });
        self.start_stop.set_enabled(true);

        self.applied = Some(next);
    }

    /// The next menu event, if one arrived since the last call. Never
    /// blocks: called from a poll loop, not awaited.
    pub fn poll(&mut self) -> Option<TrayEvent> {
        let event = MenuEvent::receiver().try_recv().ok()?;
        event_for_id(event.id.as_ref())
    }
}

/// Routes a clicked menu item's id to the event it means, or `None` for an
/// id nothing on this menu produces -- defensively, since `MenuEvent`'s
/// channel is process-global and shared with whatever else in the binary
/// might use `muda` menus.
fn event_for_id(id: &str) -> Option<TrayEvent> {
    match id {
        ID_OPEN => Some(TrayEvent::Open),
        ID_LOCK => Some(TrayEvent::ToggleLock),
        ID_START_STOP => Some(TrayEvent::StartStop),
        ID_QUIT => Some(TrayEvent::Quit),
        _ => None,
    }
}

/// Decodes one of the two embedded PNGs into the raw RGBA `tray-icon`
/// wants. Both are generated at 32x32, 8-bit RGBA, and checked in under
/// `assets/`; a decode failure here is a bug in that asset, not a runtime
/// condition, so it panics with a specific reason rather than degrading.
fn decode_icon(png_bytes: &[u8]) -> Icon {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder
        .read_info()
        .expect("embedded tray icon is a valid PNG");
    let mut buf = vec![
        0u8;
        reader
            .output_buffer_size()
            .expect("embedded tray icon has a known frame size")
    ];
    let info = reader
        .next_frame(&mut buf)
        .expect("embedded tray icon decodes to its declared size");
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "embedded tray icon must be RGBA"
    );
    assert_eq!(
        info.bit_depth,
        png::BitDepth::Eight,
        "embedded tray icon must be 8-bit"
    );
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).expect("embedded tray icon has valid dimensions")
}

/// Whether something is listening for a tray icon to register itself.
///
/// Windows and macOS both build a tray host into the shell, so there is
/// nothing to ask; whatever `tray-icon`'s own `build()` reports is the
/// whole story there. Linux (and the BSDs `tray-icon`'s "gtk" feature also
/// targets) go through the freedesktop StatusNotifierItem protocol, whose
/// host registers on the session bus as `org.kde.StatusNotifierWatcher`
/// regardless of which desktop implements it -- GNOME's AppIndicator
/// extension included. If nothing owns that name, no icon will ever be
/// rendered no matter what `tray-icon` returns, which is exactly the
/// GNOME-without-the-extension case this exists to catch.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn tray_host_available() -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return false;
    };
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &("org.kde.StatusNotifierWatcher",),
    )
    .and_then(|reply| reply.body().deserialize::<bool>())
    .unwrap_or(false)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn tray_host_available() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Status;

    fn status(state: LinkState, locked: bool) -> Status {
        Status {
            role: crate::config::Role::Server,
            state,
            peer: None,
            rtt_us: 0,
            locked,
            events: 0,
            lost: 0,
            audio_depth_ms: 0,
            audio_lost: 0,
            mic_depth_ms: 0,
            mic_lost: 0,
        }
    }

    #[test]
    fn the_two_embedded_icons_decode() {
        // Not a `Tray` test -- it does not need a display or a tray host --
        // but it is the one thing that can catch a corrupted or re-encoded
        // asset before `Tray::new` does, deep inside a `.expect`.
        let connected = decode_icon(ICON_CONNECTED_PNG);
        let disconnected = decode_icon(ICON_DISCONNECTED_PNG);
        // `Icon` exposes no accessors to compare against, so decoding
        // without panicking is the whole assertion.
        drop(connected);
        drop(disconnected);
    }

    #[test]
    fn every_menu_item_routes_to_its_event() {
        assert_eq!(event_for_id(ID_OPEN), Some(TrayEvent::Open));
        assert_eq!(event_for_id(ID_LOCK), Some(TrayEvent::ToggleLock));
        assert_eq!(event_for_id(ID_START_STOP), Some(TrayEvent::StartStop));
        assert_eq!(event_for_id(ID_QUIT), Some(TrayEvent::Quit));
    }

    #[test]
    fn an_id_nothing_on_this_menu_produces_is_ignored() {
        // Defensive: `MenuEvent`'s channel is process-global, so `poll`
        // must not panic or misroute on an id from elsewhere in the binary.
        assert_eq!(event_for_id("not-ours"), None);
    }

    #[test]
    fn set_state_reduction_matches_core_state() {
        // `Tray::set_state` needs a built `Tray`, which needs a tray host
        // that CI does not have. The reduction from `CoreState` to
        // (connected, locked, running) is the part worth pinning without
        // one, since it is the part the brief calls out as easy to get
        // wrong: connected tracks the link, not merely "a child exists".
        fn reduce(s: &CoreState) -> (bool, bool, bool) {
            let (running, locked) = match s {
                CoreState::Running(status) => (true, status.locked),
                CoreState::NoConfig | CoreState::Stopped(_) => (false, false),
            };
            let connected =
                matches!(s, CoreState::Running(status) if status.state == LinkState::Connected);
            (connected, locked, running)
        }

        assert_eq!(reduce(&CoreState::NoConfig), (false, false, false));
        assert_eq!(
            reduce(&CoreState::Stopped("exited".into())),
            (false, false, false)
        );
        assert_eq!(
            reduce(&CoreState::Running(status(LinkState::Connecting, false))),
            (false, false, true),
            "running but not yet linked must not show connected"
        );
        assert_eq!(
            reduce(&CoreState::Running(status(LinkState::Connected, true))),
            (true, true, true)
        );
        assert_eq!(
            reduce(&CoreState::Running(status(
                LinkState::Failed("boom".into()),
                true
            ))),
            (false, true, true),
            "a failed link is not connected even while the core still runs and stays locked"
        );
    }

    /// Not run by `cargo test`: it needs a live desktop session (a tray
    /// host on the session bus, and a GTK main loop on Linux to drive the
    /// icon and deliver menu clicks) that CI does not have. Run it by hand:
    ///
    /// ```text
    /// cargo test -p pheme-app --lib frontend::tray::tests::manual_smoke -- --ignored --nocapture
    /// ```
    ///
    /// It prints what `Tray::new` decided, then, if it got one, keeps the
    /// icon up for 30 seconds while printing every menu event so a person
    /// can click through Open, Lock input, Stop/Start and Quit and see
    /// them arrive.
    #[test]
    #[ignore = "manual: needs a live desktop session"]
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    fn manual_smoke() {
        // `tray-icon`'s gtk backend needs a running gtk event loop on the
        // same thread it was created on -- gtk-rs enforces this and panics
        // otherwise -- to register the icon and to deliver menu clicks.
        // Nothing in `Tray` does this for the caller, since that is the
        // window's job from Task 10 on; here, everything (init, `Tray::new`,
        // and pumping the loop) stays on this one thread instead.
        gtk::init().expect("gtk::init for the manual smoke test");

        let Some(mut tray) = Tray::new() else {
            println!("Tray::new() returned None -- no tray host on this session bus");
            return;
        };
        println!("tray created; watch the tray area for the icon");
        tray.set_state(&CoreState::Running(status(LinkState::Connecting, false)));
        println!("state: Running/Connecting (disconnected icon, Stop, unchecked lock disabled)");

        // Simulates what Task 10's window will do in response to each
        // event, so a person clicking through the menu can watch the
        // checkmark and the label actually follow rather than guess.
        let mut locked = false;
        let mut running = true;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            if let Some(event) = tray.poll() {
                println!("event: {event:?}");
                match event {
                    TrayEvent::Quit => break,
                    TrayEvent::ToggleLock => {
                        locked = !locked;
                        println!("core would now be locked = {locked}");
                    }
                    TrayEvent::StartStop => {
                        running = !running;
                        println!("core would now be running = {running}");
                    }
                    TrayEvent::Open => println!("core would now show the window"),
                }
                let state = if running {
                    CoreState::Running(status(LinkState::Connected, locked))
                } else {
                    CoreState::Stopped("stopped from the tray".into())
                };
                tray.set_state(&state);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        println!("manual_smoke done");
    }
}
