//! Input capture on Wayland, through the `org.freedesktop.portal.InputCapture`
//! portal and libei.

pub mod geometry;
pub(crate) mod session;
pub mod translate;

use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use pheme_core::{CaptureEvent, Rect};
use pheme_proto::ScreenInfo;

use crate::linux_screens::wayland_screens;
use crate::{CaptureEdge, CaptureMode, Error, InputCapture, Result};
use session::Cmd;

/// How long a command waits for the session thread, matching the trait's contract
/// and the X11 backend's `MODE_CHANGE_TIMEOUT`.
const CMD_TIMEOUT: Duration = Duration::from_secs(1);

pub struct PortalCapture {
    screens: Vec<ScreenInfo>,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: Option<async_channel::Receiver<Cmd>>,
    edges: Vec<CaptureEdge>,
    thread: Option<JoinHandle<()>>,
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
        let thread = std::thread::Builder::new()
            .name("pheme-portal".into())
            .spawn(move || {
                futures_lite::future::block_on(session::run(tx, cmd_rx, screen, edges, ready_tx))
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        self.thread = Some(thread);
        // Wait for the session to exist before returning, so a `set_edges` issued
        // immediately after `start()` is not answered by a thread that has not yet
        // created its session — and so a refused permission surfaces here.
        ready_rx
            .recv()
            .map_err(|_| Error::Backend("the portal session thread exited during startup".into()))?
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

    fn stop(&mut self) {
        let _ = self.cmd_tx.send_blocking(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
