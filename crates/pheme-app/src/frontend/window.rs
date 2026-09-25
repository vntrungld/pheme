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

use crate::config::{config_dir, AudioCfg, ClientCfg, Config, HotkeysCfg, Role, SideCfg};
use crate::ipc::{Command, LinkState};
use pheme_audio::devices::{list_devices, DeviceInfo, DeviceKind};

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

    // `cfg` was moved into `Supervisor::start` above; read it back from the
    // supervisor rather than cloning it earlier just for this.
    let config_form = ConfigForm::from_config(&supervisor.config().unwrap_or_default());

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
        config_form,
        config_errors: ConfigFormErrors::default(),
        config_warned: false,
        devices: Arc::new(Mutex::new(DeviceListState::Loading)),
    };
    // Fetched once, here, off the repaint thread -- never on a repaint or a
    // menu open. See the doc comment on `start_device_fetch`.
    start_device_fetch(&app);

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

/// The in-progress edits behind the configuration panel: what a person has
/// typed, not yet a [`Config`]. A listen address and a span bound are text
/// here, exactly as they are in a text field, until [`ConfigForm::build`]
/// either turns them into one or explains why it could not.
///
/// Kept separate from `Config` for the same reason [`StatusView`] is kept
/// separate from `CoreState`: a pure reduction ([`ConfigForm::from_config`])
/// and its inverse ([`ConfigForm::build`]), each testable without a window.
#[derive(Debug, Clone, PartialEq)]
struct ConfigForm {
    role: Role,
    name: String,
    listen: String,
    connect: String,
    /// Not exposed anywhere in the panel -- task 12 does not ask for a
    /// discovery toggle -- but carried through unedited so that saving from
    /// this form never silently flips it back to its default.
    discovery: bool,
    lock_hotkey: String,
    clients: Vec<ClientForm>,
    /// Empty means "the operating system default", exactly as an absent
    /// key does in the file (`AudioCfg`'s doc comment).
    playback_device: String,
    capture_device: String,
    mic_device: String,
}

impl ConfigForm {
    /// Reduces `cfg` into what the panel shows.
    fn from_config(cfg: &Config) -> Self {
        ConfigForm {
            role: cfg.role,
            name: cfg.name.clone(),
            listen: cfg.listen.to_string(),
            connect: cfg.connect.clone().unwrap_or_default(),
            discovery: cfg.discovery,
            lock_hotkey: cfg.hotkeys.lock.clone().unwrap_or_default(),
            clients: cfg.clients.iter().map(ClientForm::from_cfg).collect(),
            playback_device: cfg.audio.playback_device.clone().unwrap_or_default(),
            capture_device: cfg.audio.capture_device.clone().unwrap_or_default(),
            mic_device: cfg.audio.mic_device.clone().unwrap_or_default(),
        }
    }

    /// Builds the `Config` this form describes, or the reasons it cannot --
    /// one attached to the field that produced it, so the panel can show a
    /// rejection beside the field rather than in a dialog that takes the
    /// context away.
    ///
    /// Two passes. The first is plain syntax nothing but this form can
    /// check -- is `listen` an address at all, is a span bound a number --
    /// because the form holds text and `Config` does not. Only once every
    /// field parses does the second pass run, and it runs the *same*
    /// checks `Config` itself already performs when the command line loads
    /// one: [`Config::hotkeys`], [`Config::placements`], and, for a client,
    /// [`Config::connect_target`]. Nothing here re-implements what a valid
    /// hotkey name or a valid span looks like -- that would risk the panel
    /// and the command line disagreeing about what counts as valid, which
    /// is exactly the situation this method exists to prevent.
    fn build(&self) -> Result<Config, Box<ConfigFormErrors>> {
        let mut errors = ConfigFormErrors {
            clients: vec![None; self.clients.len()],
            ..ConfigFormErrors::default()
        };

        let listen: Option<SocketAddr> = match self.listen.trim().parse() {
            Ok(a) => Some(a),
            Err(e) => {
                errors.listen = Some(format!("not a valid address: {e}"));
                None
            }
        };

        let mut clients = Vec::with_capacity(self.clients.len());
        for (i, c) in self.clients.iter().enumerate() {
            match c.build() {
                Ok(built) => clients.push(built),
                Err(e) => errors.clients[i] = Some(e),
            }
        }

        if errors.has_any() {
            return Err(Box::new(errors));
        }
        let listen = listen.expect("no listen error was recorded above");

        let trimmed_or_none = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };

        let cfg = Config {
            role: self.role,
            name: self.name.clone(),
            listen,
            connect: trimmed_or_none(&self.connect),
            discovery: self.discovery,
            hotkeys: HotkeysCfg {
                lock: trimmed_or_none(&self.lock_hotkey),
            },
            audio: AudioCfg {
                playback_device: trimmed_or_none(&self.playback_device),
                capture_device: trimmed_or_none(&self.capture_device),
                mic_device: trimmed_or_none(&self.mic_device),
            },
            clients,
        };

        // From here on: `Config`'s own checks, not a second copy of them.
        if let Err(e) = cfg.hotkeys() {
            errors.hotkey_lock = Some(e.to_string());
        }
        if let Err(e) = cfg.placements() {
            attribute_placement_error(&mut errors, &cfg, &e.to_string());
        }
        if cfg.role == Role::Client {
            if let Err(e) = cfg.connect_target(None) {
                errors.connect = Some(e.to_string());
            }
        }

        if errors.has_any() {
            Err(Box::new(errors))
        } else {
            Ok(cfg)
        }
    }
}

/// One row of the client list being edited: [`ClientCfg`]'s fields, with the
/// span written out as the two text fields a person actually edits.
#[derive(Debug, Clone, PartialEq)]
struct ClientForm {
    name: String,
    side: SideCfg,
    span_start: String,
    span_end: String,
}

impl Default for ClientForm {
    fn default() -> Self {
        ClientForm {
            name: String::new(),
            side: SideCfg::Right,
            span_start: String::new(),
            span_end: String::new(),
        }
    }
}

impl ClientForm {
    fn from_cfg(c: &ClientCfg) -> Self {
        let (start, end) = c.span.map(|s| (s[0], s[1])).unzip();
        ClientForm {
            name: c.name.clone(),
            side: c.side,
            span_start: start.map(|v| v.to_string()).unwrap_or_default(),
            span_end: end.map(|v| v.to_string()).unwrap_or_default(),
        }
    }

    /// Parses this row's own text into a [`ClientCfg`], or says why it
    /// could not -- the syntax pass [`ConfigForm::build`] needs before it
    /// can even ask `Config::placements` about the semantics of a span.
    fn build(&self) -> Result<ClientCfg, String> {
        let start = self.span_start.trim();
        let end = self.span_end.trim();
        let span = match (start.is_empty(), end.is_empty()) {
            (true, true) => None,
            (false, false) => {
                let s: f32 = start
                    .parse()
                    .map_err(|_| format!("span start {start:?} is not a number"))?;
                let e: f32 = end
                    .parse()
                    .map_err(|_| format!("span end {end:?} is not a number"))?;
                Some([s, e])
            }
            _ => return Err("span needs both a start and an end, or neither".to_string()),
        };
        Ok(ClientCfg {
            name: self.name.trim().to_string(),
            side: self.side,
            span,
        })
    }
}

/// A rejected field's reason, one slot per field on [`ConfigForm`] --
/// `clients` is aligned by row index with `ConfigForm::clients`. `general`
/// is for the one case a reason cannot be attached to any single field (see
/// [`attribute_placement_error`]'s fallback); the panel shows it above the
/// Save button rather than dropping it.
#[derive(Debug, Clone, Default, PartialEq)]
struct ConfigFormErrors {
    general: Option<String>,
    name: Option<String>,
    listen: Option<String>,
    connect: Option<String>,
    hotkey_lock: Option<String>,
    clients: Vec<Option<String>>,
}

impl ConfigFormErrors {
    fn has_any(&self) -> bool {
        self.general.is_some()
            || self.name.is_some()
            || self.listen.is_some()
            || self.connect.is_some()
            || self.hotkey_lock.is_some()
            || self.clients.iter().any(Option::is_some)
    }
}

/// Attaches `message` -- `Config::placements`'s own error text -- to the
/// client row it names. `placements` reports the *first* invalid span it
/// finds and stops there (it is built on `Iterator::collect` into a
/// `Result`), so only one row is ever attributed per call; a second Save
/// click surfaces the next one once the first is fixed, the same way the
/// command line would only ever report one line at a time either.
///
/// The message always starts with `"client {name:?}: "` (see
/// `Config::placements`'s `bail!`), so matching that prefix against each
/// row's own name finds the row without re-deriving the rule the message
/// is about. Falls back to `errors.general` on the message shape ever
/// changing underneath this -- so a validation failure can never silently
/// vanish, even if it can no longer be pinned to one row.
fn attribute_placement_error(errors: &mut ConfigFormErrors, cfg: &Config, message: &str) {
    for (i, c) in cfg.clients.iter().enumerate() {
        let prefix = format!("client {:?}: ", c.name);
        if let Some(rest) = message.strip_prefix(&prefix) {
            errors.clients[i] = Some(rest.to_string());
            return;
        }
    }
    errors.general = Some(message.to_string());
}

fn side_label(side: SideCfg) -> &'static str {
    match side {
        SideCfg::Left => "Left",
        SideCfg::Right => "Right",
        SideCfg::Top => "Top",
        SideCfg::Bottom => "Bottom",
    }
}

/// The audio device menus' contents, fetched once off the repaint thread
/// and refreshed only when asked. Filled by [`start_device_fetch`].
#[derive(Debug, Clone)]
enum DeviceListState {
    Loading,
    Loaded(Vec<DeviceInfo>),
    Failed(String),
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
    /// The configuration panel's in-progress edits. Loaded once from
    /// `Supervisor::config()` in `run`, then owned here: the panel is a
    /// draft that survives a rejected Save, not something re-read from the
    /// supervisor on every frame.
    config_form: ConfigForm,
    /// A rejected field's reason from the last Save attempt, one slot per
    /// field. Cleared the moment a Save actually goes through.
    config_errors: ConfigFormErrors,
    /// Whether the "comments do not survive" warning has been shown yet
    /// this session. Starts `false`; the panel shows the warning until the
    /// first Save click, then stops -- it is a warning to say once, not a
    /// standing disclaimer.
    config_warned: bool,
    /// The audio device menus' contents. Filled once by `start_device_fetch`
    /// at startup and again only when "Refresh devices" is clicked -- never
    /// on a repaint, never on a menu open. See that function's doc comment
    /// for why.
    devices: Arc<Mutex<DeviceListState>>,
}

impl PhemeApp {
    /// Flips Lock/Unlock against whatever the core last reported. The only
    /// place either the tray's "Lock input" item or the window's own Lock
    /// button drives -- see Finding 3 (final review): Start/Stop and Lock
    /// used to exist only as tray menu items, which left GNOME without the
    /// AppIndicator extension (the README's own stated default) with no
    /// way to lock input from the GUI at all. Both callers act through
    /// this one method rather than each sending their own `Command`, so
    /// they cannot drift apart on what a click actually does.
    fn toggle_lock(&mut self) {
        if let CoreState::Running(status) = self.supervisor.state() {
            let cmd = if status.locked {
                Command::Unlock
            } else {
                Command::Lock
            };
            self.handle.block_on(self.supervisor.send(cmd));
        }
    }

    /// Starts or stops the supervised core, whichever the current state
    /// implies. The only place either the tray's Start/Stop item or the
    /// window's own Start/Stop button drives -- see [`toggle_lock`][Self::toggle_lock]'s
    /// doc comment for why both go through one method.
    fn start_stop(&mut self) {
        self.action_error = None;
        match self.supervisor.state() {
            CoreState::Running(_) => {
                self.handle.block_on(self.supervisor.shutdown());
            }
            // `restart` respawns from whatever `Supervisor` is already
            // holding; starting a child back up is not a configuration
            // change, so this never touches disk the way `apply_config`
            // does. A `NoConfig` restart is a harmless no-op, so there is
            // nothing to report there either.
            CoreState::Stopped(_) | CoreState::NoConfig => {
                if let Err(e) = self.handle.block_on(self.supervisor.restart()) {
                    // Surfaced the same way a link failure is: a silently
                    // discarded error here would leave a Start click with
                    // no visible effect and no reason anywhere.
                    self.action_error = Some(format!("could not start: {e:#}"));
                }
            }
        }
    }
}

impl eframe::App for PhemeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The tray's clicks and its own icon/menu updates ride on GTK's
        // event loop on Linux; nothing else in this process pumps it.
        pump_gtk();

        // Collected rather than handled inline: `tray.poll()` holds a
        // mutable borrow of `self.tray` for the loop, and `ToggleLock` and
        // `StartStop` now call `self.toggle_lock()` / `self.start_stop()`
        // -- the same methods the window's own buttons call, per Finding 3
        // -- which need the whole of `self`, not just the tray field. The
        // borrow has to end before those run.
        let mut tray_events = Vec::new();
        if let Some(tray) = &mut self.tray {
            while let Some(event) = tray.poll() {
                tray_events.push(event);
            }
        }
        for event in tray_events {
            match event {
                TrayEvent::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayEvent::ToggleLock => self.toggle_lock(),
                TrayEvent::StartStop => self.start_stop(),
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
                draw_actions(ui, &state, self);
                draw_pairing(ui, role, &state, self);
                draw_config(ui, self);
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

/// Start/Stop and Lock, next to the status panel -- drawn every frame,
/// regardless of whether a tray exists.
///
/// Finding 3 (final review): these two actions used to live only in the
/// tray's menu, which left a person on GNOME without the AppIndicator
/// extension (the README's own stated default there) with no way to start
/// a stopped core, and no way to lock input from the GUI at all. Both
/// buttons call [`PhemeApp::start_stop`] and [`PhemeApp::toggle_lock`] --
/// the exact same methods `TrayEvent::StartStop` and
/// `TrayEvent::ToggleLock` call in `PhemeApp::update` -- rather than
/// reimplementing either action here, so the window and the tray can never
/// disagree about what a click does.
fn draw_actions(ui: &mut egui::Ui, core_state: &CoreState, app: &mut PhemeApp) {
    let running = matches!(core_state, CoreState::Running(_));
    let locked = matches!(core_state, CoreState::Running(status) if status.locked);

    ui.horizontal(|ui| {
        if ui.button(if running { "Stop" } else { "Start" }).clicked() {
            app.start_stop();
        }
        let lock_label = if locked { "Unlock input" } else { "Lock input" };
        // Disabled while nothing is running, same as the tray's own
        // checkmark item (`Tray::set_state` calls `lock.set_enabled(running)`):
        // there is nothing for a lock command to reach.
        if ui
            .add_enabled(running, egui::Button::new(lock_label))
            .clicked()
        {
            app.toggle_lock();
        }
    });
    ui.separator();
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

/// What the server pairing panel should show for the code its current
/// generation has printed, if any. Three states, and only the first shows
/// the code at all:
///
/// - [`Live`][Self::Live]: the code is still good -- its generation hasn't
///   exited, and hasn't (yet) connected over `--ipc` to say it is serving
///   normally.
/// - [`Completed`][Self::Completed]: that generation connected over `--ipc`
///   (`CoreState::Running`) -- pairing succeeded, and the code is spent.
/// - [`Failed`][Self::Failed]: that generation's own child exited before
///   either of the above -- a wrong code, a timeout, or anything else
///   `pheme server --pair` can fail with.
///
/// Kept separate from `draw_pairing_server` itself, on the same reasoning
/// as [`StatusView`]: which of these three a code is in is a pure
/// reduction of a few small pieces of state, worth testing without a
/// window (see the finding that added this: `Supervisor::state()` alone
/// cannot tell "the generation that printed this code is over" from "an
/// *older* generation's `Stopped` just hasn't been overwritten yet",
/// because it is shared across every generation a `Supervisor` ever
/// spawns).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PairingCodeStatus {
    /// No code has been printed (or asked for) yet.
    None,
    Live(String),
    Completed(String),
    Failed {
        code: String,
        reason: String,
    },
}

impl PairingCodeStatus {
    /// Reduces what the current generation has reported into one of the
    /// three states above.
    ///
    /// `exit_reason` is `Supervisor::current_exit_reason()`: `Some` only
    /// once *this* generation's own child has exited, which is what
    /// distinguishes `Failed` from `Live` even while the shared
    /// `core_state` still reads `Stopped` from whatever generation came
    /// before this one.
    fn from_parts(
        code: Option<String>,
        exit_reason: Option<String>,
        core_state: &CoreState,
    ) -> Self {
        let Some(code) = code else {
            return Self::None;
        };
        if let Some(reason) = exit_reason {
            return Self::Failed { code, reason };
        }
        match core_state {
            CoreState::Running(_) => Self::Completed(code),
            _ => Self::Live(code),
        }
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
    let status = PairingCodeStatus::from_parts(
        app.supervisor.pairing_code(),
        app.supervisor.current_exit_reason(),
        core_state,
    );
    match status {
        PairingCodeStatus::None => {
            ui.label("Click Start pairing to get a code, then enter it on the client.");
        }
        PairingCodeStatus::Live(code) => {
            ui.label(format!("Pairing code: {code}"));
            ui.label(
                "Enter this on the client's pairing panel (or run `pheme \
                 pair <this host> <code>`). Valid for 120 seconds, for one \
                 client.",
            );
        }
        PairingCodeStatus::Completed(code) => {
            // The generation that printed this code has since connected
            // over `--ipc` and is serving normally -- pairing succeeded,
            // so the code on screen is stale, not live.
            ui.label(format!(
                "Last pairing code was {code}, now inactive: pairing \
                 succeeded. Click Start pairing again for a new one."
            ));
        }
        PairingCodeStatus::Failed { code, reason } => {
            // Same visual pattern `action_error` already uses: red, plain
            // text. A dead code shown as though it were live is worse than
            // showing nothing -- the whole reason this state exists.
            ui.colored_label(
                egui::Color32::RED,
                format!("Pairing code {code} is no longer valid: {reason}"),
            );
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

/// Starts a background audio-device enumeration, and marks the device menus
/// as loading. Split out from `draw_config` for the same reason
/// `start_discovery` is: this call itself must return at once, with
/// `list_devices` running on `handle` rather than blocking the caller, and
/// splitting it out lets that be exercised directly in a test.
///
/// `list_devices` already bounds its own wait for PipeWire to one second and
/// abandons the listening thread, detached, on timeout (see
/// `pheme_audio::devices::list_devices`'s own doc comment) -- what this
/// function guards against is calling it *at all* on every repaint or every
/// menu open, which would pile up one abandoned thread per attempt against a
/// wedged sound server. It runs exactly once, here, at startup (`run`), and
/// again only when the panel's "Refresh devices" button is clicked.
/// `spawn_blocking` rather than `spawn`: `list_devices` is a synchronous,
/// blocking call in its own right (it joins or abandons its own thread
/// before returning), so it belongs on the blocking pool, not an async
/// worker.
fn start_device_fetch(app: &PhemeApp) {
    *app.devices.lock().expect("devices mutex poisoned") = DeviceListState::Loading;
    let devices = app.devices.clone();
    app.handle.spawn_blocking(move || {
        let result = list_devices();
        *devices.lock().expect("devices mutex poisoned") = match result {
            Ok(list) => DeviceListState::Loaded(list),
            Err(e) => DeviceListState::Failed(e.to_string()),
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

/// Draws the configuration panel: role, name, the address field the role
/// implies, the client list, the lock hotkey, and the three device menus --
/// everything `config.toml` holds, so a person never has to open a text
/// editor to reach it. Always drawn, including with no configuration yet
/// (`role` in `draw_pairing`'s sense may be `None` then): this panel is
/// exactly how that first configuration gets created.
fn draw_config(ui: &mut egui::Ui, app: &mut PhemeApp) {
    ui.separator();
    ui.heading("Configuration");

    if let Some(msg) = &app.config_errors.general {
        ui.colored_label(egui::Color32::RED, msg);
    }

    ui.horizontal(|ui| {
        ui.label("Role:");
        ui.radio_value(&mut app.config_form.role, Role::Server, "Server");
        ui.radio_value(&mut app.config_form.role, Role::Client, "Client");
    });

    ui.horizontal(|ui| {
        ui.label("Name:");
        ui.text_edit_singleline(&mut app.config_form.name);
    });
    if let Some(err) = &app.config_errors.name {
        ui.colored_label(egui::Color32::RED, err);
    }

    match app.config_form.role {
        Role::Server => {
            ui.horizontal(|ui| {
                ui.label("Listen address:");
                ui.text_edit_singleline(&mut app.config_form.listen);
            });
            if let Some(err) = &app.config_errors.listen {
                ui.colored_label(egui::Color32::RED, err);
            }
        }
        Role::Client => {
            ui.horizontal(|ui| {
                ui.label("Server address:");
                ui.text_edit_singleline(&mut app.config_form.connect);
            });
            if let Some(err) = &app.config_errors.connect {
                ui.colored_label(egui::Color32::RED, err);
            }
        }
    }

    ui.horizontal(|ui| {
        ui.label("Lock hotkey:");
        ui.text_edit_singleline(&mut app.config_form.lock_hotkey);
    });
    ui.label("Empty disables the lock hotkey.");
    if let Some(err) = &app.config_errors.hotkey_lock {
        ui.colored_label(egui::Color32::RED, err);
    }

    ui.separator();
    ui.label("Clients");
    let mut remove: Option<usize> = None;
    for (i, client) in app.config_form.clients.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.add(egui::TextEdit::singleline(&mut client.name).desired_width(100.0));
                ui.label("Side:");
                egui::ComboBox::from_id_salt("client_side")
                    .selected_text(side_label(client.side))
                    .show_ui(ui, |ui| {
                        for side in [SideCfg::Left, SideCfg::Right, SideCfg::Top, SideCfg::Bottom] {
                            ui.selectable_value(&mut client.side, side, side_label(side));
                        }
                    });
                ui.label("Span:");
                ui.add(egui::TextEdit::singleline(&mut client.span_start).desired_width(48.0));
                ui.label("to");
                ui.add(egui::TextEdit::singleline(&mut client.span_end).desired_width(48.0));
                if ui.button("Remove").clicked() {
                    remove = Some(i);
                }
            });
            if let Some(err) = app.config_errors.clients.get(i).and_then(|e| e.as_ref()) {
                ui.colored_label(egui::Color32::RED, err);
            }
        });
    }
    if let Some(i) = remove {
        app.config_form.clients.remove(i);
    }
    if ui.button("Add client").clicked() {
        app.config_form.clients.push(ClientForm::default());
    }

    ui.separator();
    ui.label("Audio devices (empty means the operating system default)");
    let device_state = app.devices.lock().expect("devices mutex poisoned").clone();
    let loading = matches!(device_state, DeviceListState::Loading);
    let list = match &device_state {
        DeviceListState::Loaded(v) => v.as_slice(),
        DeviceListState::Loading | DeviceListState::Failed(_) => &[],
    };
    if ui
        .add_enabled(!loading, egui::Button::new("Refresh devices"))
        .clicked()
    {
        start_device_fetch(app);
    }
    match &device_state {
        DeviceListState::Loading => {
            ui.label("Loading audio devices...");
        }
        DeviceListState::Failed(e) => {
            ui.colored_label(
                egui::Color32::RED,
                format!("Could not list audio devices: {e}"),
            );
        }
        DeviceListState::Loaded(_) => {}
    }
    device_combo(
        ui,
        "Playback device (server, plays received audio):",
        &mut app.config_form.playback_device,
        list,
        DeviceKind::Playback,
    );
    device_combo(
        ui,
        "Capture device (client, Windows loopback source):",
        &mut app.config_form.capture_device,
        list,
        DeviceKind::Playback,
    );
    device_combo(
        ui,
        "Microphone device (server):",
        &mut app.config_form.mic_device,
        list,
        DeviceKind::Capture,
    );

    ui.separator();
    if !app.config_warned {
        ui.colored_label(
            egui::Color32::YELLOW,
            "Saving rewrites config.toml from scratch. Comments in a \
             hand-edited file will not survive it -- a TOML writer has \
             none to preserve.",
        );
    }
    if ui.button("Save").clicked() {
        app.config_warned = true;
        match app.config_form.build() {
            Ok(cfg) => {
                app.config_errors = ConfigFormErrors::default();
                app.action_error = None;
                let path = Config::default_path();
                // The only path that writes the file and restarts the
                // child -- `restart` is for starting back up without a
                // configuration change, which this is not.
                if let Err(e) = app.handle.block_on(app.supervisor.apply_config(cfg, &path)) {
                    app.action_error = Some(format!("could not save the configuration: {e:#}"));
                }
            }
            Err(errors) => {
                app.config_errors = *errors;
            }
        }
    }
}

/// One audio device menu: an empty selection (the operating system default)
/// plus every known device of `kind`, in whatever order `list` already has
/// them (`list_devices` sorts playback-first, then alphabetical).
fn device_combo(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut String,
    list: &[DeviceInfo],
    kind: DeviceKind,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let shown = if selected.is_empty() {
            "(operating system default)".to_string()
        } else {
            selected.clone()
        };
        egui::ComboBox::from_id_salt(label)
            .selected_text(shown)
            .show_ui(ui, |ui| {
                ui.selectable_value(selected, String::new(), "(operating system default)");
                for d in list.iter().filter(|d| d.kind == kind) {
                    ui.selectable_value(selected, d.name.clone(), &d.name);
                }
            });
    });
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

    // --- `PairingCodeStatus::from_parts` ------------------------------
    //
    // The three states a pairing code can be in, pinned directly against
    // the pure reduction -- no window, no Supervisor, no process.

    #[test]
    fn no_code_yet_is_none_regardless_of_core_state() {
        assert_eq!(
            PairingCodeStatus::from_parts(None, None, &CoreState::NoConfig),
            PairingCodeStatus::None
        );
        assert_eq!(
            PairingCodeStatus::from_parts(None, None, &CoreState::Stopped("x".into())),
            PairingCodeStatus::None
        );
    }

    #[test]
    fn a_code_with_its_generation_still_alive_is_live() {
        // The case that matters most while pairing has just started: the
        // *previous* generation's `Stopped` can still be sitting in the
        // shared `core_state` (`stop_current` inside `restart_pairing` ran
        // before the new generation reported anything), and that must not
        // be mistaken for the new code having already died.
        assert_eq!(
            PairingCodeStatus::from_parts(
                Some("123456".into()),
                None,
                &CoreState::Stopped("previous generation exited normally".into()),
            ),
            PairingCodeStatus::Live("123456".into())
        );
        assert_eq!(
            PairingCodeStatus::from_parts(Some("123456".into()), None, &CoreState::NoConfig),
            PairingCodeStatus::Live("123456".into())
        );
    }

    #[test]
    fn a_code_whose_generation_connected_over_ipc_is_completed() {
        // The generation that printed this code went on to connect over
        // `--ipc` and report a real `Status` -- pairing succeeded.
        let status = Status {
            state: LinkState::Listening,
            ..server_status_with_peer("laptop-win")
        };
        assert_eq!(
            PairingCodeStatus::from_parts(Some("123456".into()), None, &CoreState::Running(status)),
            PairingCodeStatus::Completed("123456".into())
        );
    }

    #[test]
    fn a_code_whose_generation_exited_is_failed_not_live() {
        // The finding this test exists for: a pairing attempt that failed
        // or timed out exits non-zero, which the shared `core_state`
        // reports as `Stopped` -- indistinguishable, by state alone, from
        // the previous generation's own `Stopped` still sitting there
        // while the new one is genuinely still alive and waiting. Without
        // `exit_reason`, this fell into the same bucket as
        // `a_code_with_its_generation_still_alive_is_live` above and kept
        // presenting a dead code as though it were still good.
        assert_eq!(
            PairingCodeStatus::from_parts(
                Some("123456".into()),
                Some("exited with status 1: pairing failed: wrong code".into()),
                &CoreState::Stopped("exited with status 1: pairing failed: wrong code".into()),
            ),
            PairingCodeStatus::Failed {
                code: "123456".into(),
                reason: "exited with status 1: pairing failed: wrong code".into(),
            }
        );
        // The failure reads the same even if `core_state` has not caught
        // up yet -- `exit_reason` alone decides this, precisely because it
        // is the one piece of state tied to *this* generation rather than
        // shared with every other one a `Supervisor` has ever spawned.
        assert_eq!(
            PairingCodeStatus::from_parts(
                Some("123456".into()),
                Some("timed out".into()),
                &CoreState::NoConfig,
            ),
            PairingCodeStatus::Failed {
                code: "123456".into(),
                reason: "timed out".into(),
            }
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
            config_form: ConfigForm::from_config(&Config::default()),
            config_errors: ConfigFormErrors::default(),
            config_warned: false,
            devices: Arc::new(Mutex::new(DeviceListState::Loading)),
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

    // --- `ConfigForm` ---------------------------------------------------
    //
    // The configuration panel's own validation and conversion, reachable
    // without a window -- in the same spirit as `StatusView` and
    // `PairingCodeStatus::from_parts` above.

    #[test]
    fn a_form_round_trips_to_an_equal_config() {
        let mut cfg = Config {
            role: Role::Server,
            name: "desk-linux".into(),
            connect: Some("laptop-win".into()),
            ..Config::default()
        };
        cfg.clients.push(ClientCfg {
            name: "laptop-win".into(),
            side: SideCfg::Right,
            span: Some([0.25, 0.75]),
        });
        cfg.clients.push(ClientCfg {
            name: "tv".into(),
            side: SideCfg::Top,
            span: None,
        });
        cfg.audio.playback_device = Some("Speakers (Realtek)".into());

        let form = ConfigForm::from_config(&cfg);
        assert_eq!(
            form.build()
                .expect("a form built from a valid config must build back"),
            cfg
        );
    }

    #[test]
    fn a_default_config_round_trips_too() {
        let cfg = Config::default();
        let form = ConfigForm::from_config(&cfg);
        assert_eq!(form.build().unwrap(), cfg);
    }

    #[test]
    fn an_invalid_span_is_rejected_beside_its_row_not_written() {
        let mut form = ConfigForm::from_config(&Config::default());
        form.clients.push(ClientForm {
            name: "backwards".into(),
            side: SideCfg::Left,
            span_start: "0.9".into(),
            span_end: "0.1".into(),
        });
        let errors = form.build().expect_err("a reversed span must be rejected");
        assert!(
            errors.clients[0]
                .as_deref()
                .is_some_and(|e| e.contains("span")),
            "no reason attached to the offending row: {errors:?}"
        );
        // Nothing else on the struct should have a stray reason: this is
        // the one thing wrong with the form.
        assert!(errors.listen.is_none());
        assert!(errors.hotkey_lock.is_none());
    }

    #[test]
    fn a_span_bound_that_is_not_a_number_is_rejected_before_any_semantic_check() {
        let mut form = ConfigForm::from_config(&Config::default());
        form.clients.push(ClientForm {
            name: "typo".into(),
            side: SideCfg::Left,
            span_start: "half".into(),
            span_end: "1".into(),
        });
        let errors = form.build().expect_err("non-numeric span must be rejected");
        assert!(errors.clients[0]
            .as_deref()
            .is_some_and(|e| e.contains("not a number")));
    }

    #[test]
    fn an_unknown_hotkey_name_is_rejected_with_configs_own_reason() {
        let mut form = ConfigForm::from_config(&Config::default());
        form.lock_hotkey = "NoSuchKey".into();
        let errors = form
            .build()
            .expect_err("an unknown key name must be rejected");
        assert!(errors.hotkey_lock.is_some());
    }

    #[test]
    fn a_client_role_with_no_server_address_is_rejected() {
        let mut form = ConfigForm::from_config(&Config::default());
        form.role = Role::Client;
        form.connect = "   ".into();
        let errors = form
            .build()
            .expect_err("a client with nothing to connect to must be rejected");
        assert!(errors.connect.is_some());
    }

    #[test]
    fn an_invalid_listen_address_is_rejected_beside_the_field() {
        let mut form = ConfigForm::from_config(&Config::default());
        form.listen = "not an address".into();
        let errors = form.build().expect_err("a bad address must be rejected");
        assert!(errors.listen.is_some());
    }

    #[test]
    fn empty_device_selections_mean_the_operating_system_default() {
        let form = ConfigForm::from_config(&Config::default());
        assert_eq!(form.playback_device, "");
        assert_eq!(form.capture_device, "");
        assert_eq!(form.mic_device, "");

        let cfg = form.build().unwrap();
        assert_eq!(cfg.audio.playback_device, None);
        assert_eq!(cfg.audio.capture_device, None);
        assert_eq!(cfg.audio.mic_device, None);

        // And the other direction: a config with a device set shows it,
        // never blank.
        let mut with_device = Config::default();
        with_device.audio.mic_device = Some("Blue Yeti".into());
        let form = ConfigForm::from_config(&with_device);
        assert_eq!(form.mic_device, "Blue Yeti");
    }

    #[tokio::test]
    async fn start_device_fetch_returns_before_list_devices_finishes() {
        // The rule `start_discovery`'s own test pins for `browse`, pinned
        // here for `list_devices`: a menu must never freeze the repaint
        // thread waiting on it, however long a wedged PipeWire takes to
        // answer (up to its own one-second bound, plus scheduling).
        let app = test_app().await;
        let started = std::time::Instant::now();
        start_device_fetch(&app);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "start_device_fetch blocked its caller for {:?}",
            started.elapsed()
        );
        assert!(matches!(
            *app.devices.lock().unwrap(),
            DeviceListState::Loading
        ));
    }
}
