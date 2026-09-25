//! The front-end's window: the status panel, plus the glue that starts the
//! supervised core, drives the tray, and decides what closing the window
//! means.
//!
//! [`run`] is the whole front-end. It owns the [`Supervisor`] and the
//! `Option<Tray>` the previous task built, and it is the one place that
//! decides what closing the window does -- which depends on whether a tray
//! exists at all (see the module-level comment on [`PhemeApp::update`]).
//!
//! [`StatusView`] is kept separate from anything that draws, on purpose: it
//! is a pure reduction of [`CoreState`] into what the panel shows, so the
//! rule that a restart must not leave the previous generation's peer or RTT
//! on screen can be tested without a window at all.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use eframe::egui;

use crate::config::{config_dir, Config, Role};
use crate::ipc::{Command, LinkState};

use super::{CoreState, Supervisor, Tray, TrayEvent};

/// Runs the whole front-end: starts the supervised core, builds the tray if
/// one is available, and opens the window. Returns once the application has
/// actually been told to quit -- not when the window is merely hidden.
pub fn run() -> anyhow::Result<()> {
    // The front-end's own tokio runtime. `main` does not hand one down: on
    // the GUI path it never builds one of its own, so there is no risk of
    // nesting `block_on` inside an already-running runtime here.
    let rt = tokio::runtime::Runtime::new().context("building the front-end's tokio runtime")?;

    let path = Config::default_path();
    let cfg = if path.exists() {
        Some(Config::load(Some(&path)).context("loading the configuration")?)
    } else {
        // No file on disk: the first run for every new user, every time.
        // `Supervisor::start` turns this into `CoreState::NoConfig` rather
        // than a guessed-at default role it was never asked to run.
        None
    };

    let exe = std::env::current_exe().context("locating the running executable")?;
    let supervisor = rt
        .block_on(Supervisor::start(exe, cfg))
        .context("starting the supervised core")?;

    // `Tray`'s own Linux backend is GTK-based and panics unless something
    // has called `gtk::init()` first -- deliberately not `Tray`'s job (see
    // its Cargo.toml comment); this is what task 10 owns instead. Harmless
    // to call even if `Tray::new()` ends up returning `None`.
    init_gtk_if_needed().context("initializing GTK for the tray icon")?;
    let tray = Tray::new();
    let has_tray = tray.is_some();

    let app = PhemeApp {
        handle: rt.handle().clone(),
        supervisor,
        tray,
        has_tray,
        hiding_works: hiding_works(),
        view: StatusView::default(),
        quitting: false,
        close_ineffective: false,
        action_error: None,
        discovery: Arc::new(Mutex::new(DiscoveryState::Idle)),
        selected_server: None,
        pair_code: String::new(),
        pairing: Arc::new(Mutex::new(PairingState::Idle)),
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("Pheme")
        .with_inner_size([420.0, 420.0])
        .with_min_inner_size([360.0, 320.0]);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native("Pheme", options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("running the front-end window: {e}"))

    // `rt` is dropped here, once the window has actually closed (`on_exit`
    // already stopped the supervised core by then).
}

/// Initializes GTK on the platforms whose tray backend needs it running.
/// A no-op everywhere else, where `gtk` is not even a dependency.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn init_gtk_if_needed() -> anyhow::Result<()> {
    gtk::init().map_err(|e| anyhow::anyhow!("gtk::init: {e}"))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn init_gtk_if_needed() -> anyhow::Result<()> {
    Ok(())
}

/// Drains whatever GTK's main loop has pending -- the tray icon's own
/// clicks and menu updates ride on it on Linux -- without blocking. A
/// no-op everywhere else, matching [`init_gtk_if_needed`].
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn pump_gtk() {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn pump_gtk() {}

/// Whether hiding the window (`ViewportCommand::Visible(false)`) can
/// actually take effect on this session, decided once at startup.
///
/// winit's Wayland backend never reads back the visibility it is asked
/// for -- `set_visible` is a documented no-op there ("Not possible on
/// Wayland"), and nothing else in its Linux tree reads it either except
/// the X11 backend, which does. A Wayland session is therefore the one
/// case this crate can name concretely; anywhere else (X11, Windows,
/// macOS) hiding is assumed to work, matching what winit actually
/// implements for those backends today.
fn hiding_works() -> bool {
    !pheme_input::is_wayland_session()
}

/// The pure reduction of [`CoreState`] into what the status panel draws.
///
/// A separate type so the reset-on-restart rule -- nothing from a run
/// before a restart may still be on screen once the role underneath it has
/// changed -- can be tested without a window (Review Focus 5).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatusView {
    pub role: Option<Role>,
    pub state: Option<LinkState>,
    pub peer: Option<String>,
    pub rtt_us: u64,
    pub locked: bool,
    pub events: u64,
    pub lost: u64,
    pub audio_depth_ms: u32,
    pub audio_lost: u64,
    pub mic_depth_ms: u32,
    pub mic_lost: u64,
    /// Set only while [`CoreState::Stopped`], and gone again the moment a
    /// fresh `Running` report arrives.
    pub stopped_reason: Option<String>,
}

impl StatusView {
    /// Reduces `s` into what the panel draws.
    ///
    /// Starts from a blank [`StatusView`] every time rather than updating
    /// fields in place: a child that stopped -- because its role changed
    /// underneath it, or for any other reason -- has nothing left to say
    /// about the peer or the RTT the generation before it reported, and a
    /// field this function did not touch is a field a stale value could
    /// hide in.
    pub fn apply(&mut self, s: &CoreState) {
        *self = StatusView::default();
        match s {
            CoreState::NoConfig => {}
            CoreState::Stopped(reason) => {
                self.stopped_reason = Some(reason.clone());
            }
            CoreState::Running(status) => {
                self.role = Some(status.role);
                self.state = Some(status.state.clone());
                self.peer = status.peer.clone();
                self.rtt_us = status.rtt_us;
                self.locked = status.locked;
                self.events = status.events;
                self.lost = status.lost;
                self.audio_depth_ms = status.audio_depth_ms;
                self.audio_lost = status.audio_lost;
                self.mic_depth_ms = status.mic_depth_ms;
                self.mic_lost = status.mic_lost;
            }
        }
    }
}

/// The eframe application: the window plus everything `run` handed it.
struct PhemeApp {
    /// A handle to `run`'s tokio runtime, so this can drive `Supervisor`'s
    /// async methods from the plain callback-driven thread eframe calls
    /// `update` on.
    handle: tokio::runtime::Handle,
    supervisor: Supervisor,
    tray: Option<Tray>,
    /// Cached from `tray.is_some()` at startup: `Tray` does not outlive
    /// `run`'s own construction of it, but this needs checking every frame.
    has_tray: bool,
    /// Decided once at startup by [`hiding_works`]: whether
    /// `ViewportCommand::Visible(false)` can actually hide the window on
    /// this session.
    hiding_works: bool,
    view: StatusView,
    /// Set once something has decided the application should actually
    /// exit -- the tray's Quit item, or the window's own close button when
    /// there is no tray to reach the application by otherwise. Guards the
    /// close-request handling below from re-cancelling a close it asked
    /// for itself.
    quitting: bool,
    /// Set the moment a close request was cancelled but could not actually
    /// hide the window (`!hiding_works`): the panel says so, so a click
    /// that visibly did nothing is not mistaken for a hung application.
    close_ineffective: bool,
    /// The reason the tray's Start/Stop item last failed to do what it
    /// asked, if it did. Cleared at the start of every new attempt.
    action_error: Option<String>,
    /// What the client pairing panel's "Discover servers" button last asked
    /// for, or found. `browse` takes about three seconds and must never run
    /// on the repaint thread, so the button spawns it on `handle` and this
    /// cell is how the result comes back; drawn every frame, never awaited.
    discovery: Arc<Mutex<DiscoveryState>>,
    /// The server picked from the discovery list, by address. Its identity
    /// beyond "the machine that answered at this address" is unproven until
    /// pairing actually succeeds -- see the fingerprint note in
    /// `draw_pairing_client`.
    selected_server: Option<SocketAddr>,
    /// The pairing code as typed into the client panel.
    pair_code: String,
    /// What the client pairing panel's "Pair" button last did. `client_pair`
    /// talks to another machine, so like discovery this is filled by a
    /// background task on `handle` rather than awaited here.
    pairing: Arc<Mutex<PairingState>>,
}

impl eframe::App for PhemeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The tray's clicks and its own icon/menu updates ride on GTK's
        // event loop on Linux; nothing else in this process pumps it.
        pump_gtk();

        if let Some(tray) = &mut self.tray {
            while let Some(event) = tray.poll() {
                match event {
                    TrayEvent::Open => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    TrayEvent::Quit => {
                        self.quitting = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    TrayEvent::ToggleLock => {
                        if let CoreState::Running(status) = self.supervisor.state() {
                            let cmd = if status.locked {
                                Command::Unlock
                            } else {
                                Command::Lock
                            };
                            self.handle.block_on(self.supervisor.send(cmd));
                        }
                    }
                    TrayEvent::StartStop => {
                        self.action_error = None;
                        match self.supervisor.state() {
                            CoreState::Running(_) => {
                                self.handle.block_on(self.supervisor.shutdown());
                            }
                            // `restart` respawns from whatever `Supervisor` is
                            // already holding; starting a child back up is not
                            // a configuration change, so this never touches
                            // disk the way `apply_config` does. A `NoConfig`
                            // restart is a harmless no-op, so there is
                            // nothing to report there either.
                            CoreState::Stopped(_) | CoreState::NoConfig => {
                                if let Err(e) = self.handle.block_on(self.supervisor.restart()) {
                                    // Surfaced the same way a link failure is:
                                    // a silently discarded error here would
                                    // leave a Start click with no visible
                                    // effect and no reason anywhere.
                                    self.action_error = Some(format!("could not start: {e:#}"));
                                }
                            }
                        }
                    }
                }
            }
        }

        // §6: the window is a view onto a running core, not the
        // application. Closing it must not stop sharing -- only Quit does
        // that -- unless there is no tray left to reopen it from, in which
        // case closing it is the only way to reach Quit at all, and must
        // act like it.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            if self.has_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if self.hiding_works {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    // The close is still cancelled -- the core keeps
                    // running either way -- but nothing here can make the
                    // window disappear, and a click that visibly does
                    // nothing reads as a hung application. Say so instead.
                    self.close_ineffective = true;
                }
            } else {
                self.quitting = true;
            }
        }

        let state = self.supervisor.state();
        // Read from the held configuration, not `view.role`: that is `None`
        // until the first `Status` arrives, which -- while pairing is in
        // progress -- can be a while (see `spawn_generation`'s comment on
        // why the code has to travel over stdout instead).
        let role = self.supervisor.config().map(|c| c.role);
        self.view.apply(&state);
        if let Some(tray) = &mut self.tray {
            tray.set_state(&state);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // The status grid plus the pairing panel below it can run past
            // the window's fixed size (a client with the discovery list
            // full, say); a scroll area keeps every control reachable
            // instead of letting the bottom of the panel run off the
            // window with no way back to it.
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(err) = &self.action_error {
                    ui.colored_label(egui::Color32::RED, err);
                    ui.separator();
                }
                if self.close_ineffective {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Pheme is still running. Use Quit in the tray to exit.",
                    );
                    ui.separator();
                }
                draw_status(ui, &self.view);
                draw_pairing(ui, role, &state, self);
            });
        });

        // The core pushes a `Status` at most once a second; this just needs
        // to keep noticing it without spinning the CPU polling faster.
        //
        // Called unconditionally, whether or not the window is currently
        // visible: this is also what keeps `update` -- and so `pump_gtk`,
        // above -- being called at all while the window is hidden on a
        // platform where hiding actually works (X11). Without it, a hidden
        // window would stop pumping GTK, the tray menu would go dead, and
        // the tray -- the only way back to a hidden window -- would be
        // unresponsive. `eframe`'s repaint scheduling is a per-window
        // timer independent of visibility (`WindowId` -> next repaint
        // `Instant`, driven by `ControlFlow::WaitUntil`), and winit's own
        // X11 `request_redraw` is an unconditional channel send with no
        // visibility check (`platform_impl/linux/x11/window.rs`) -- so this
        // timer keeps firing, and `update` keeps being called, regardless
        // of whether the window is mapped. Deliberately not moved onto a
        // separate thread: GTK's main context has thread affinity to
        // wherever `gtk::init()` ran, and `Tray`'s widgets are also touched
        // from this same thread below, so pumping it from anywhere else
        // would violate GTK's single-thread rule instead of fixing this.
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.handle.block_on(self.supervisor.shutdown());
    }
}

/// Draws the status panel for `view`.
fn draw_status(ui: &mut egui::Ui, view: &StatusView) {
    let Some(role) = view.role else {
        if let Some(reason) = &view.stopped_reason {
            ui.heading("Stopped");
            ui.label(reason);
        } else {
            ui.heading("No configuration yet");
            ui.label("Set one up to start sharing.");
        }
        return;
    };

    let role_name = match role {
        Role::Server => "Server",
        Role::Client => "Client",
    };
    ui.heading(format!("Running as {role_name}"));

    egui::Grid::new("status_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Link");
            match view.state.as_ref() {
                Some(LinkState::Failed(why)) => {
                    ui.colored_label(egui::Color32::RED, format!("Failed: {why}"));
                }
                Some(state) => {
                    ui.label(link_state_name(state));
                }
                None => {
                    ui.label("-");
                }
            }
            ui.end_row();

            ui.label("Peer");
            ui.label(view.peer.as_deref().unwrap_or("(none yet)"));
            ui.end_row();

            ui.label("RTT");
            ui.label(format!("{:.1} ms", view.rtt_us as f64 / 1000.0));
            ui.end_row();

            ui.label("Locked");
            ui.label(if view.locked { "yes" } else { "no" });
            ui.end_row();

            ui.label("Events/s");
            ui.label(view.events.to_string());
            ui.end_row();

            // `lost` is counted only by the client, which numbers the gaps
            // in the server's own sequence -- a server never has anything
            // to say about it, so it gets a label rather than a nought
            // that would read as "nothing was lost". Doc comment on
            // `Status`.
            ui.label("Lost/s");
            match role {
                Role::Client => ui.label(view.lost.to_string()),
                Role::Server => ui.label("not measured here (the client counts this)"),
            };
            ui.end_row();

            // `audio_*` is the stream the server plays and `mic_*` the
            // stream the client plays; each role fills only the pair it
            // owns, so the other pair is labelled rather than shown as a
            // misleading zero.
            ui.label("Audio depth");
            match role {
                Role::Server => ui.label(format!("{} ms", view.audio_depth_ms)),
                Role::Client => ui.label("not measured here (the server plays audio)"),
            };
            ui.end_row();

            ui.label("Audio lost/s");
            match role {
                Role::Server => ui.label(view.audio_lost.to_string()),
                Role::Client => ui.label("not measured here (the server plays audio)"),
            };
            ui.end_row();

            ui.label("Mic depth");
            match role {
                Role::Client => ui.label(format!("{} ms", view.mic_depth_ms)),
                Role::Server => ui.label("not measured here (the client plays the mic)"),
            };
            ui.end_row();

            ui.label("Mic lost/s");
            match role {
                Role::Client => ui.label(view.mic_lost.to_string()),
                Role::Server => ui.label("not measured here (the client plays the mic)"),
            };
            ui.end_row();
        });
}

fn link_state_name(state: &LinkState) -> &'static str {
    match state {
        LinkState::Starting => "Starting",
        LinkState::Listening => "Listening",
        LinkState::Connecting => "Connecting",
        LinkState::Connected => "Connected",
        LinkState::Failed(_) => "Failed",
    }
}

/// What the client pairing panel's "Discover servers" button last asked for,
/// or found. `pheme_net::discovery::browse` takes about three seconds;
/// written from the background task `draw_pairing_client` spawns, and read
/// back every frame -- never awaited on the repaint thread.
#[derive(Debug, Clone)]
enum DiscoveryState {
    Idle,
    Browsing,
    Found(Vec<pheme_net::discovery::Found>),
    Failed(String),
}

/// What the client pairing panel's "Pair" button last did. Filled by a
/// background task the same way as [`DiscoveryState`]: `client_pair` also
/// talks to another machine and must not block the repaint thread either.
#[derive(Debug, Clone)]
enum PairingState {
    Idle,
    Pairing,
    Done(String),
    Failed(String),
}

/// Draws the pairing panel for `role`, or nothing at all if there is no
/// configuration yet (`role` is `None`) -- there is nothing to pair a role
/// that has not been chosen. A server gets the button that restarts the
/// core with `--pair` and the code it then prints; a client gets the mDNS
/// list, a code field and the button that runs `client_pair` in this
/// process.
fn draw_pairing(ui: &mut egui::Ui, role: Option<Role>, core_state: &CoreState, app: &mut PhemeApp) {
    let Some(role) = role else {
        return;
    };
    ui.separator();
    ui.heading("Pairing");
    match role {
        Role::Server => draw_pairing_server(ui, core_state, app),
        Role::Client => draw_pairing_client(ui, app),
    }
}

/// The server half of the pairing panel: a button that restarts the
/// supervised core with `--pair`, and the code it prints once it has.
fn draw_pairing_server(ui: &mut egui::Ui, core_state: &CoreState, app: &mut PhemeApp) {
    if ui.button("Start pairing").clicked() {
        app.action_error = None;
        // Blocks only on the child spawning, not on the pairing exchange
        // itself, which runs to completion inside that child process --
        // the same pattern `TrayEvent::StartStop` already uses for `restart`.
        if let Err(e) = app.handle.block_on(app.supervisor.restart_pairing()) {
            app.action_error = Some(format!("could not start pairing: {e:#}"));
        }
    }
    match (app.supervisor.pairing_code(), core_state) {
        (Some(code), CoreState::Running(_)) => {
            // The generation that printed this code has since connected
            // over `--ipc` and is serving normally -- pairing ended one way
            // or another, so the code on screen is stale, not live.
            ui.label(format!(
                "Last pairing code was {code}, now inactive. Click Start \
                 pairing again for a new one."
            ));
        }
        (Some(code), _) => {
            ui.label(format!("Pairing code: {code}"));
            ui.label(
                "Enter this on the client's pairing panel (or run `pheme \
                 pair <this host> <code>`). Valid for 120 seconds, for one \
                 client.",
            );
        }
        (None, _) => {
            ui.label("Click Start pairing to get a code, then enter it on the client.");
        }
    }
}

/// The client half of the pairing panel: the mDNS discovery list, a code
/// field, and the button that runs `client_pair` in this process.
fn draw_pairing_client(ui: &mut egui::Ui, app: &mut PhemeApp) {
    let browsing = matches!(
        *app.discovery.lock().expect("discovery mutex poisoned"),
        DiscoveryState::Browsing
    );
    if ui
        .add_enabled(!browsing, egui::Button::new("Discover servers"))
        .clicked()
    {
        start_discovery(app);
    }

    let snapshot = app
        .discovery
        .lock()
        .expect("discovery mutex poisoned")
        .clone();
    match &snapshot {
        DiscoveryState::Idle => {
            ui.label("Click Discover servers to look for one on this network.");
        }
        DiscoveryState::Browsing => {
            ui.label("Looking for servers (about 3 seconds)...");
        }
        DiscoveryState::Failed(e) => {
            ui.colored_label(egui::Color32::RED, format!("Discovery failed: {e}"));
        }
        DiscoveryState::Found(found) if found.is_empty() => {
            ui.label(
                "No servers found. Make sure one is running, with discovery \
                 on, on the same network.",
            );
        }
        DiscoveryState::Found(found) => {
            // The whole reason this line exists: a fingerprint published
            // over mDNS is not proof of anything -- anyone on the network
            // can advertise one. It is shown only so it can be compared by
            // eye with what the server itself displays; trust comes from
            // the pairing code below, never from this list.
            ui.label(
                "Fingerprints are advisory: compare the one below with what \
                 the server shows, but trust comes from the pairing code, \
                 never from this list.",
            );
            for f in found {
                let selected = app.selected_server == Some(f.addr);
                let label = format!(
                    "{}   {}   fingerprint: {}",
                    f.name,
                    f.addr,
                    f.fingerprint.as_deref().unwrap_or("(none advertised)")
                );
                if ui.selectable_label(selected, label).clicked() {
                    app.selected_server = Some(f.addr);
                }
            }
        }
    }

    ui.horizontal(|ui| {
        ui.label("Code:");
        ui.text_edit_singleline(&mut app.pair_code);
    });

    let pairing_snapshot = app.pairing.lock().expect("pairing mutex poisoned").clone();
    let busy_pairing = matches!(pairing_snapshot, PairingState::Pairing);
    let can_pair =
        app.selected_server.is_some() && !app.pair_code.trim().is_empty() && !busy_pairing;
    if ui
        .add_enabled(can_pair, egui::Button::new("Pair"))
        .clicked()
    {
        if let (Some(addr), Some(cfg)) = (app.selected_server, app.supervisor.config()) {
            let code = app.pair_code.clone();
            start_pairing(app, addr, cfg.name, code);
        }
    }

    match pairing_snapshot {
        PairingState::Idle => {}
        PairingState::Pairing => {
            ui.label("Pairing...");
        }
        PairingState::Done(name) => {
            ui.colored_label(egui::Color32::GREEN, format!("Paired with {name}."));
        }
        PairingState::Failed(e) => {
            ui.colored_label(egui::Color32::RED, format!("Pairing failed: {e}"));
        }
    }
}

/// Starts a background discovery browse, and marks the panel as looking.
/// Split out from `draw_pairing_client` so the one rule that matters here --
/// this call itself must return at once, with `browse` running on `handle`
/// rather than blocking the caller -- can be exercised directly in a test,
/// without needing to simulate a click through `egui`.
fn start_discovery(app: &PhemeApp) {
    *app.discovery.lock().expect("discovery mutex poisoned") = DiscoveryState::Browsing;
    let discovery = app.discovery.clone();
    // Never run on the repaint thread: `browse` takes about three seconds,
    // and freezing the window for that long is exactly what the
    // two-process design exists to avoid. `handle` is the front-end's own
    // tokio runtime, driven independently of `update`.
    app.handle.spawn(async move {
        let result = pheme_net::discovery::browse(Duration::from_secs(3)).await;
        *discovery.lock().expect("discovery mutex poisoned") = match result {
            Ok(found) => DiscoveryState::Found(found),
            Err(e) => DiscoveryState::Failed(e.to_string()),
        };
    });
}

/// Starts a background pairing attempt against `addr`, as the client
/// identified by `name`. Split out from `draw_pairing_client` for the same
/// reason as [`start_discovery`]: `client_pair` talks to another machine and
/// must not block the caller either.
fn start_pairing(app: &PhemeApp, addr: SocketAddr, name: String, code: String) {
    *app.pairing.lock().expect("pairing mutex poisoned") = PairingState::Pairing;
    let pairing = app.pairing.clone();
    app.handle.spawn(async move {
        let result = pair_with(&name, addr, &code).await;
        *pairing.lock().expect("pairing mutex poisoned") = match result {
            Ok(server_name) => PairingState::Done(server_name),
            Err(e) => PairingState::Failed(format!("{e:#}")),
        };
    });
}

/// Pairs with the server at `addr` using `code`, as the client identified by
/// `name`. The same three steps `pheme_app::client::pair` runs for the
/// `pheme pair` subcommand, run here in the front-end process instead: it
/// already links `pheme-net`, so nothing shells out to a second invocation
/// of the binary. Always called from a spawned task (see [`start_pairing`]),
/// never awaited directly from `update`.
async fn pair_with(name: &str, addr: SocketAddr, code: &str) -> anyhow::Result<String> {
    let dir = config_dir();
    let identity = pheme_net::Identity::load_or_create(&dir, name)?;
    let trust = pheme_net::TrustStore::load(&dir)?.shared();
    let endpoint = pheme_net::Endpoint::pairing_client(&identity, trust.clone())?;
    let server_name =
        pheme_net::pairing::client_pair(&endpoint, addr, code.trim(), &identity, trust).await?;
    Ok(server_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Status;

    fn server_status_with_peer(peer: &str) -> Status {
        Status {
            role: Role::Server,
            state: LinkState::Connected,
            peer: Some(peer.to_string()),
            rtt_us: 4_200,
            locked: false,
            events: 91,
            lost: 0,
            audio_depth_ms: 22,
            audio_lost: 3,
            mic_depth_ms: 0,
            mic_lost: 0,
        }
    }

    #[test]
    fn a_restart_does_not_show_the_previous_role_s_peer() {
        // The role changed and the child restarted. Nothing from the run
        // before may still be on screen.
        let mut view = StatusView::default();
        view.apply(&CoreState::Running(server_status_with_peer("laptop-win")));
        assert_eq!(view.peer.as_deref(), Some("laptop-win"));
        view.apply(&CoreState::Stopped("restarting".into()));
        assert_eq!(view.peer, None, "a stale peer survived the restart");
        assert_eq!(view.rtt_us, 0);
    }

    #[test]
    fn no_config_clears_a_previous_run_too() {
        let mut view = StatusView::default();
        view.apply(&CoreState::Running(server_status_with_peer("laptop-win")));
        view.apply(&CoreState::NoConfig);
        assert_eq!(view.peer, None);
        assert_eq!(view.rtt_us, 0);
        assert_eq!(view.role, None);
    }

    #[test]
    fn a_failed_link_carries_its_reason_into_the_view() {
        // `LinkState::Failed(why)` is the whole reason this panel earns its
        // place -- until now `why` existed only in the log.
        let mut view = StatusView::default();
        let status = Status {
            state: LinkState::Failed("address already in use".into()),
            ..server_status_with_peer("laptop-win")
        };
        view.apply(&CoreState::Running(status));
        assert_eq!(
            view.state,
            Some(LinkState::Failed("address already in use".into()))
        );
    }

    /// A `PhemeApp` with a `Supervisor` that has nothing configured, so
    /// `Supervisor::start` spawns no child at all -- `exe` is therefore
    /// never actually run, and can be any path. Cheap enough to build in
    /// every test below, and it keeps each one from having to repeat every
    /// field this struct has gained since task 10.
    async fn test_app() -> PhemeApp {
        let supervisor = Supervisor::start(std::path::PathBuf::from("/nonexistent-pheme"), None)
            .await
            .expect("a Supervisor with no configuration spawns nothing");
        PhemeApp {
            handle: tokio::runtime::Handle::current(),
            supervisor,
            tray: None,
            has_tray: false,
            hiding_works: true,
            view: StatusView::default(),
            quitting: false,
            close_ineffective: false,
            action_error: None,
            discovery: Arc::new(Mutex::new(DiscoveryState::Idle)),
            selected_server: None,
            pair_code: String::new(),
            pairing: Arc::new(Mutex::new(PairingState::Idle)),
        }
    }

    #[tokio::test]
    async fn start_discovery_returns_before_browse_finishes() {
        // Review Focus 1 (task 11 brief): `browse` takes about three
        // seconds, and this call must never make the caller wait for it --
        // the caller here stands in for the repaint thread `draw_pairing`
        // is called from every frame.
        let app = test_app().await;
        let started = std::time::Instant::now();
        start_discovery(&app);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "start_discovery blocked its caller for {:?}; browse must run on \
             a task, not whatever thread calls this",
            started.elapsed()
        );
        assert!(matches!(
            *app.discovery.lock().unwrap(),
            DiscoveryState::Browsing
        ));
    }

    /// Real mDNS multicast, and about three seconds long: not something CI
    /// should pay for on every push. Run by hand:
    /// `cargo test -p pheme-app --lib frontend::window::tests::discovery_finds_a_real_advertised_server -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn discovery_finds_a_real_advertised_server() {
        let _advert = pheme_net::discovery::advertise(
            "pheme-window-test-server",
            24813,
            "window-test-fingerprint",
        )
        .expect("advertise");

        let app = test_app().await;
        start_discovery(&app);

        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let found = loop {
            {
                let d = app.discovery.lock().unwrap();
                match &*d {
                    DiscoveryState::Found(found) => break found.clone(),
                    DiscoveryState::Failed(e) => panic!("discovery failed: {e}"),
                    _ => {}
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "discovery did not finish in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        let server = found
            .iter()
            .find(|f| f.name == "pheme-window-test-server")
            .expect("our own advertised server was not among those discovered");
        assert_eq!(server.addr.port(), 24813);
        assert_eq!(
            server.fingerprint.as_deref(),
            Some("window-test-fingerprint")
        );
    }
}
