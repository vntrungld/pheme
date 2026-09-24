//! Input capture on Wayland, through the `org.freedesktop.portal.InputCapture`
//! portal and libei.

pub mod geometry;
pub(crate) mod session;
pub mod shortcuts;
pub mod translate;

use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use pheme_core::{CaptureEvent, Rect};
use pheme_proto::ScreenInfo;
use tracing::{debug, warn};

use crate::linux_screens::wayland_screens;
use crate::{CaptureEdge, CaptureMode, Error, InputCapture, Result};
use session::Cmd;

/// How long a command waits for the session thread, matching the trait's contract
/// and the X11 backend's `MODE_CHANGE_TIMEOUT`.
const CMD_TIMEOUT: Duration = Duration::from_secs(1);

/// How long `stop()` waits for the session thread to confirm it has dropped its
/// `CaptureEvent` sender before giving up on joining it.
///
/// Closing a D-Bus session is not a mode change, and the two calls `shutdown`
/// makes (`Release`, then `Close`) have no timeout of their own — a slow or
/// hung compositor must not be able to turn `stop()`, and therefore `Drop`,
/// into an unbounded wait. A little more slack than `CMD_TIMEOUT` seems
/// warranted for a call that is allowed to touch the network stack twice.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Wraps a backend failure with the message aimed at a compositor that has no
/// InputCapture portal implementation at all -- the case a wlroots compositor
/// (Hyprland, Sway) hits every time.
///
/// That case does **not** surface from `PortalCapture::new()`: `new()` only
/// calls `wayland_screens()`, plain `wl_output`/`wl_registry` enumeration,
/// which every Wayland compositor implements correctly, portal or not. The
/// missing-portal failure happens inside `establish()`, on the session
/// thread, and is reported through `start()`'s `ready` channel -- so this
/// wrapping is applied at both call sites (`new()`'s error in
/// `detect_capture()`, and `start()`'s error here) rather than only the one
/// that looks like the "right" place, so the message actually reaches the
/// people it was written for.
pub(crate) fn describe_no_portal(e: Error) -> Error {
    match e {
        Error::Backend(m) | Error::Unsupported(m) => Error::Unsupported(format!(
            "Wayland capture needs a compositor that implements the InputCapture \
             portal (KDE, GNOME): {m}. wlroots compositors such as Hyprland and \
             Sway are not supported yet; this machine can still be used as a \
             client."
        )),
        other => other,
    }
}

pub struct PortalCapture {
    screens: Vec<ScreenInfo>,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: Option<async_channel::Receiver<Cmd>>,
    edges: Vec<CaptureEdge>,
    thread: Option<JoinHandle<()>>,
    /// Signalled by the session thread right after it drops its `CaptureEvent`
    /// sender, so `stop()` can bound its wait instead of joining blindly into
    /// `shutdown`'s untimed D-Bus calls. `None` before `start()` and after the
    /// signal has been consumed.
    stopped_rx: Option<std::sync::mpsc::Receiver<()>>,
}

impl PortalCapture {
    pub fn new() -> Result<PortalCapture> {
        let screens = wayland_screens()?;
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        Ok(PortalCapture {
            screens,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            edges: Vec::new(),
            thread: None,
            stopped_rx: None,
        })
    }

    /// Sends a command and waits for the session thread's answer, so the trait's
    /// synchronous contract holds.
    ///
    /// The acknowledgement travels on `std::sync::mpsc`, whose `recv_timeout` gives
    /// the 1 s bound the trait requires; `async_channel` 2.x has no timed receive.
    fn call(&self, make: impl FnOnce(std::sync::mpsc::Sender<Result<()>>) -> Cmd) -> Result<()> {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .send_blocking(make(ack_tx))
            .map_err(|_| Error::Backend("the portal session thread is gone".into()))?;
        match ack_rx.recv_timeout(CMD_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(Error::Backend("mode change timed out".into())),
        }
    }
}

impl InputCapture for PortalCapture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        let cmd_rx = self
            .cmd_rx
            .take()
            .ok_or_else(|| Error::Backend("already started".into()))?;
        let screen = Rect::bounds(&self.screens);
        let edges = self.edges.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pheme-portal".into())
            .spawn(move || {
                futures_lite::future::block_on(session::run(
                    tx, cmd_rx, screen, edges, ready_tx, stopped_tx,
                ))
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        self.thread = Some(thread);
        self.stopped_rx = Some(stopped_rx);
        // Wait for the session to exist before returning, so a `set_edges` issued
        // immediately after `start()` is not answered by a thread that has not yet
        // created its session — and so a refused permission surfaces here.
        //
        // A missing-portal failure (wlroots: Hyprland, Sway) is exactly the sort
        // of error that arrives here rather than from `new()`, so it gets the
        // same friendly wrapping `detect_capture()` applies to `new()`'s errors.
        match ready_rx.recv() {
            Ok(r) => r.map_err(describe_no_portal),
            Err(_) => Err(Error::Backend(
                "the portal session thread exited during startup".into(),
            )),
        }
    }

    /// The compositor is already capturing by the time the core asks for a grab, and
    /// releasing is `release`, not a mode. Both directions are acknowledged with
    /// nothing to do.
    fn set_mode(&mut self, _mode: CaptureMode) -> Result<()> {
        Ok(())
    }

    /// A captured pointer is hidden and parked by the compositor, and an uncaptured
    /// one cannot be moved by an application under Wayland. The core only warps
    /// during `abort_switch`, where doing nothing is correct.
    fn warp_cursor(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        self.call(|ack| Cmd::Release { x, y, ack })
    }

    fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        self.edges = edges.to_vec();
        if self.thread.is_none() {
            // Not started yet; `start` passes these through to the session.
            return Ok(());
        }
        self.call(|ack| Cmd::SetEdges(edges.to_vec(), ack))
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    /// Idempotent. Asks the session thread to stop, and joins it only once its
    /// `CaptureEvent` sender is confirmed dropped and it has actually finished --
    /// otherwise the thread is detached rather than joined.
    ///
    /// `shutdown`'s `Release` and `Close` calls have no timeout of their own, so
    /// blindly joining here could still turn this into an unbounded wait on a
    /// slow or hung compositor -- exactly what `DeviceThread::start` in
    /// `pheme-audio` already refuses to do for the same reason. The trait's
    /// contract only needs the sender dropped before `stop()` returns, which
    /// `run` guarantees happens before the untimed part even begins.
    fn stop(&mut self) {
        let _ = self.cmd_tx.send_blocking(Cmd::Stop);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let Some(stopped_rx) = self.stopped_rx.take() else {
            // No `start()` reached the point of creating this channel, so the
            // thread (if any) never ran far enough to need bounding.
            let _ = thread.join();
            return;
        };
        match stopped_rx.recv_timeout(STOP_TIMEOUT) {
            Ok(()) if thread.is_finished() => {
                let _ = thread.join();
            }
            Ok(()) => {
                // The sender is confirmed dropped -- the trait's contract is
                // met -- but the thread is still inside `shutdown`'s untimed
                // D-Bus calls. Let it finish on its own rather than block here.
                debug!("the portal session thread is still closing its D-Bus session; detaching it rather than waiting");
                drop(thread);
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The sender end of `stopped_rx` was dropped without ever
                // sending: `run` returned before reaching that point (most
                // likely a startup failure left this thread already finished).
                // Joining an already-finished thread is instant either way.
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                warn!(
                    "the portal session thread did not confirm its event sender \
                     was dropped within {STOP_TIMEOUT:?}; detaching it rather \
                     than blocking stop() on a possibly hung compositor call"
                );
                drop(thread);
            }
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
