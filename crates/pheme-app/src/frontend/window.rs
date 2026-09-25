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

use std::time::Duration;

use anyhow::Context as _;
use eframe::egui;

use crate::config::{Config, Role};
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
        .block_on(Supervisor::start(exe, cfg.clone()))
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
        cfg,
        view: StatusView::default(),
        quitting: false,
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
    /// The configuration last started or applied, kept so the tray's
    /// Start/Stop item can restart the same role without a config panel's
    /// help. `None` until one exists on disk.
    cfg: Option<Config>,
    view: StatusView,
    /// Set once something has decided the application should actually
    /// exit -- the tray's Quit item, or the window's own close button when
    /// there is no tray to reach the application by otherwise. Guards the
    /// close-request handling below from re-cancelling a close it asked
    /// for itself.
    quitting: bool,
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
                    TrayEvent::StartStop => match self.supervisor.state() {
                        CoreState::Running(_) => {
                            self.handle.block_on(self.supervisor.shutdown());
                        }
                        CoreState::Stopped(_) => {
                            if let Some(cfg) = self.cfg.clone() {
                                let path = Config::default_path();
                                let _ = self
                                    .handle
                                    .block_on(self.supervisor.apply_config(cfg, &path));
                            }
                        }
                        CoreState::NoConfig => {}
                    },
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
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.quitting = true;
            }
        }

        let state = self.supervisor.state();
        self.view.apply(&state);
        if let Some(tray) = &mut self.tray {
            tray.set_state(&state);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            draw_status(ui, &self.view);
        });

        // The core pushes a `Status` at most once a second; this just needs
        // to keep noticing it without spinning the CPU polling faster.
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
}
