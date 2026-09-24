//! X11 capture backend: XInput2 raw events, XIGrabDevice, XFixes cursor hiding.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use pheme_core::CaptureEvent;
use pheme_proto::{Button, ScreenInfo};
use tracing::{debug, error, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, DeviceType, XIEventMask};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, PropMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

use crate::keymap::evdev_to_hid;
use crate::linux_screens::x11_screens;
use crate::{CaptureMode, Error, InputCapture, Result};

/// How long `set_mode` waits for the event thread to apply (or fail) a mode change.
const MODE_CHANGE_TIMEOUT: Duration = Duration::from_secs(1);

enum Cmd {
    /// Apply a mode; the event thread reports the actual grab/ungrab result on the sender.
    SetMode(CaptureMode, mpsc::Sender<Result<()>>),
    Stop,
}

pub struct X11Capture {
    /// Connection B: used from the caller's thread (warp, wake-ups).
    ctl: RustConnection,
    root: u32,
    wake_window: u32,
    screens: Vec<ScreenInfo>,
    cmd_tx: Sender<Cmd>,
    cmd_rx: Option<Receiver<Cmd>>,
    thread: Option<JoinHandle<()>>,
}

fn be(e: impl std::fmt::Display) -> Error {
    Error::Backend(format!("x11: {e}"))
}

impl X11Capture {
    pub fn new() -> Result<X11Capture> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return Err(Error::Unsupported(
                "Wayland capture is not supported yet (sub-project 4); use an X11 session or run this machine as a client".into(),
            ));
        }
        let (ctl, screen_num) = x11rb::connect(None).map_err(be)?;
        let root = ctl.setup().roots[screen_num].root;
        let screens = x11_screens(&ctl, root)?;
        let wake_window = ctl.generate_id().map_err(be)?;
        ctl.create_window(
            COPY_DEPTH_FROM_PARENT,
            wake_window,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .map_err(be)?;
        ctl.flush().map_err(be)?;
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        Ok(X11Capture {
            ctl,
            root,
            wake_window,
            screens,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            thread: None,
        })
    }

    /// Wakes the event thread by touching a property on the hidden window.
    fn wake(&self) -> Result<()> {
        self.ctl
            .change_property8(
                PropMode::REPLACE,
                self.wake_window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"w",
            )
            .map_err(be)?;
        self.ctl.flush().map_err(be)
    }
}

impl Drop for X11Capture {
    /// A capture dropped while grabbed still releases the grab and shows the cursor;
    /// `stop()` is idempotent so this is safe after an explicit `stop()`.
    fn drop(&mut self) {
        self.stop();
    }
}

impl InputCapture for X11Capture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        let cmd_rx = self
            .cmd_rx
            .take()
            .ok_or_else(|| Error::Backend("already started".into()))?;
        let wake_window = self.wake_window;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pheme-x11".into())
            .spawn(move || {
                if let Err(e) = event_loop(tx, cmd_rx, wake_window, ready_tx) {
                    error!("x11 event loop ended: {e}");
                }
            })
            .map_err(be)?;
        self.thread = Some(thread);
        // Wait for the event thread to select raw events and the wake-window
        // property mask, so a command sent right after `start()` returns is
        // never stranded (the thread wasn't watching for it yet).
        ready_rx.recv().map_err(|_| {
            Error::Backend("x11 event thread exited before signaling readiness".into())
        })?
    }

    /// Synchronous (see the trait contract): waits for the event thread to acquire or
    /// release the grab and returns its actual result.
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::SetMode(mode, ack_tx))
            .map_err(|_| Error::Backend("x11 event thread is gone".into()))?;
        self.wake()?;
        match ack_rx.recv_timeout(MODE_CHANGE_TIMEOUT) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(Error::Backend("mode change timed out".into()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Backend(
                "x11 event thread exited before applying the mode change".into(),
            )),
        }
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.ctl
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)
            .map_err(be)?;
        self.ctl.flush().map_err(be)
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    /// Ungrab, then warp. Both are attempted even if the first fails: the previous
    /// `Ungrab` + `WarpCursor` pair kept going after a failed ungrab, and dropping the
    /// warp would leave the pointer wherever the grab had parked it.
    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        let ungrab = self.set_mode(CaptureMode::Observe);
        let warp = self.warp_cursor(x, y);
        ungrab.and(warp)
    }

    fn stop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        match self.wake() {
            Ok(()) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
            }
            Err(e) => {
                // The event thread will never see the Stop command without the
                // wake-up; joining would hang forever. Detach it instead.
                error!("x11 stop: wake failed, detaching event thread: {e}");
                self.thread.take();
            }
        }
    }
}

struct EventLoop {
    conn: RustConnection,
    root: u32,
    pointer: u16,
    keyboard: u16,
    center: (i16, i16),
    mode: CaptureMode,
    /// Fractional motion left over after truncating to whole-pixel deltas
    /// (libinput reports raw deltas normalised to 1000 DPI, which are
    /// often sub-pixel for slow movement).
    rem_x: f64,
    rem_y: f64,
    tx: Sender<CaptureEvent>,
}

fn fp3232(v: xinput::Fp3232) -> f64 {
    v.integral as f64 + v.frac as f64 / 4_294_967_296.0
}

/// Extracts (x, y) deltas from a raw motion event's valuator mask/values.
fn raw_xy(mask: &[u32], values: &[xinput::Fp3232]) -> (f64, f64) {
    let bit = |axis: usize| {
        mask.get(axis / 32)
            .map(|m| m & (1 << (axis % 32)) != 0)
            .unwrap_or(false)
    };
    let mut idx = 0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    if bit(0) {
        dx = values.get(idx).map(|v| fp3232(*v)).unwrap_or(0.0);
        idx += 1;
    }
    if bit(1) {
        dy = values.get(idx).map(|v| fp3232(*v)).unwrap_or(0.0);
    }
    (dx, dy)
}

/// The raw XI2 event mask this backend watches, as a single OR'd value.
fn raw_event_mask() -> XIEventMask {
    XIEventMask::RAW_MOTION
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE
        | XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE
}

/// `XIGrabDevice`'s mask argument is a raw `&[u32]`, not `&[XIEventMask]`.
fn raw_masks() -> Vec<u32> {
    vec![raw_event_mask().into()]
}

impl EventLoop {
    fn new(tx: Sender<CaptureEvent>, wake_window: u32) -> Result<EventLoop> {
        let (conn, screen_num) = x11rb::connect(None).map_err(be)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let center = (
            (screen.width_in_pixels / 2) as i16,
            (screen.height_in_pixels / 2) as i16,
        );
        conn.xinput_xi_query_version(2, 2)
            .map_err(be)?
            .reply()
            .map_err(be)?;
        conn.xfixes_query_version(5, 0)
            .map_err(be)?
            .reply()
            .map_err(be)?;
        let devices = conn
            .xinput_xi_query_device(u16::from(xinput::Device::ALL_MASTER))
            .map_err(be)?
            .reply()
            .map_err(be)?;
        let mut pointer = None;
        let mut keyboard = None;
        for d in devices.infos {
            match d.type_ {
                DeviceType::MASTER_POINTER if pointer.is_none() => pointer = Some(d.deviceid),
                DeviceType::MASTER_KEYBOARD if keyboard.is_none() => keyboard = Some(d.deviceid),
                _ => {}
            }
        }
        let pointer = pointer.ok_or_else(|| be("no master pointer"))?;
        let keyboard = keyboard.ok_or_else(|| be("no master keyboard"))?;
        // `.check()` forces a round-trip, so by the time `EventLoop::new`
        // returns we know the server has registered both selections; the
        // caller uses that as the readiness signal before it lets commands
        // through (see `start()`).
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: u16::from(xinput::Device::ALL_MASTER),
                mask: vec![raw_event_mask()],
            }],
        )
        .map_err(be)?
        .check()
        .map_err(be)?;
        conn.change_window_attributes(
            wake_window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(be)?
        .check()
        .map_err(be)?;
        conn.flush().map_err(be)?;
        info!(pointer, keyboard, "x11 capture ready");
        Ok(EventLoop {
            conn,
            root,
            pointer,
            keyboard,
            center,
            mode: CaptureMode::Observe,
            rem_x: 0.0,
            rem_y: 0.0,
            tx,
        })
    }

    fn grab(&mut self) -> Result<()> {
        for attempt in 0..3 {
            let masks = raw_masks();
            let ok_p = self
                .conn
                .xinput_xi_grab_device(
                    self.root,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    self.pointer,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    false.into(),
                    &masks,
                )
                .map_err(be)?
                .reply()
                .map_err(be)?
                .status;
            let ok_k = self
                .conn
                .xinput_xi_grab_device(
                    self.root,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    self.keyboard,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    false.into(),
                    &masks,
                )
                .map_err(be)?
                .reply()
                .map_err(be)?
                .status;
            if ok_p == GrabStatus::SUCCESS && ok_k == GrabStatus::SUCCESS {
                // Hide + centre; if either fails, release the grabs again so an `Err`
                // leaves the backend exactly as it was (still Observe, nothing held).
                if let Err(e) = self
                    .conn
                    .xfixes_hide_cursor(self.root)
                    .map_err(be)
                    .and_then(|_| self.warp_center())
                {
                    let _ = self.ungrab();
                    return Err(e);
                }
                self.mode = CaptureMode::Grab;
                self.rem_x = 0.0;
                self.rem_y = 0.0;
                debug!("grabbed");
                return Ok(());
            }
            let _ = self
                .conn
                .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.pointer);
            let _ = self
                .conn
                .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.keyboard);
            warn!(attempt, ?ok_p, ?ok_k, "grab failed; retrying");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(be(
            "could not grab pointer/keyboard (another client holds a grab)",
        ))
    }

    fn ungrab(&mut self) -> Result<()> {
        self.conn
            .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.pointer)
            .map_err(be)?;
        self.conn
            .xinput_xi_ungrab_device(x11rb::CURRENT_TIME, self.keyboard)
            .map_err(be)?;
        self.conn.xfixes_show_cursor(self.root).map_err(be)?;
        self.conn.flush().map_err(be)?;
        self.mode = CaptureMode::Observe;
        debug!("ungrabbed");
        Ok(())
    }

    fn warp_center(&self) -> Result<()> {
        self.conn
            .warp_pointer(
                x11rb::NONE,
                self.root,
                0,
                0,
                0,
                0,
                self.center.0,
                self.center.1,
            )
            .map_err(be)?;
        self.conn.flush().map_err(be)
    }

    fn send(&self, ev: CaptureEvent) {
        if self.tx.try_send(ev).is_err() {
            warn!("capture channel full; dropping event");
        }
    }

    fn on_event(&mut self, ev: Event) -> Result<()> {
        let down = matches!(
            ev,
            Event::XinputRawButtonPress(_) | Event::XinputRawKeyPress(_)
        );
        match ev {
            Event::XinputRawMotion(m) => {
                if self.mode == CaptureMode::Grab {
                    let (dx, dy) = raw_xy(&m.valuator_mask, &m.axisvalues_raw);
                    self.rem_x += dx;
                    self.rem_y += dy;
                    let ix = self.rem_x.trunc() as i32;
                    let iy = self.rem_y.trunc() as i32;
                    self.rem_x -= ix as f64;
                    self.rem_y -= iy as f64;
                    if ix != 0 || iy != 0 {
                        self.send(CaptureEvent::MotionRel { dx: ix, dy: iy });
                    }
                    self.warp_center()?;
                } else {
                    let p = self
                        .conn
                        .query_pointer(self.root)
                        .map_err(be)?
                        .reply()
                        .map_err(be)?;
                    self.send(CaptureEvent::MotionAbs {
                        x: p.root_x as i32,
                        y: p.root_y as i32,
                    });
                }
            }
            Event::XinputRawButtonPress(b) | Event::XinputRawButtonRelease(b) => {
                if self.mode != CaptureMode::Grab {
                    return Ok(());
                }
                match b.detail {
                    1 => self.send(CaptureEvent::Button {
                        btn: Button::Left,
                        down,
                    }),
                    2 => self.send(CaptureEvent::Button {
                        btn: Button::Middle,
                        down,
                    }),
                    3 => self.send(CaptureEvent::Button {
                        btn: Button::Right,
                        down,
                    }),
                    4 if down => self.send(CaptureEvent::Wheel { dx: 0, dy: 120 }),
                    5 if down => self.send(CaptureEvent::Wheel { dx: 0, dy: -120 }),
                    6 if down => self.send(CaptureEvent::Wheel { dx: -120, dy: 0 }),
                    7 if down => self.send(CaptureEvent::Wheel { dx: 120, dy: 0 }),
                    8 => self.send(CaptureEvent::Button {
                        btn: Button::Back,
                        down,
                    }),
                    9 => self.send(CaptureEvent::Button {
                        btn: Button::Forward,
                        down,
                    }),
                    _ => {}
                }
            }
            Event::XinputRawKeyPress(k) | Event::XinputRawKeyRelease(k) => {
                let Some(code) = k.detail.checked_sub(8).and_then(|e| evdev_to_hid(e as u16))
                else {
                    debug!(keycode = k.detail, "unmapped X11 keycode");
                    return Ok(());
                };
                self.send(CaptureEvent::Key { code, down });
            }
            Event::Error(e) => warn!(?e, "x11 error event"),
            _ => {}
        }
        Ok(())
    }
}

fn event_loop(
    tx: Sender<CaptureEvent>,
    cmd_rx: Receiver<Cmd>,
    wake_window: u32,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let mut lp = match EventLoop::new(tx, wake_window) {
        Ok(lp) => lp,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return Ok(());
        }
    };
    // Raw events and the wake-window property mask are selected (and the
    // selection round-tripped) by this point; safe to let `start()` return
    // and commands start flowing.
    if ready_tx.send(Ok(())).is_err() {
        // The caller gave up waiting; nothing more to do.
        return Ok(());
    }
    loop {
        let ev = lp.conn.wait_for_event().map_err(be)?;
        if let Event::PropertyNotify(p) = &ev {
            if p.window == wake_window {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        Cmd::SetMode(mode, ack) => {
                            // A failed grab must not kill the loop: report it to the
                            // caller (who aborts the switch) and keep observing.
                            let r = if mode == lp.mode {
                                Ok(())
                            } else if mode == CaptureMode::Grab {
                                lp.grab()
                            } else {
                                lp.ungrab()
                            };
                            if let Err(e) = &r {
                                error!(?mode, "mode change failed: {e}");
                            }
                            // The caller may have given up (timeout); nothing to do then.
                            let _ = ack.send(r);
                        }
                        Cmd::Stop => {
                            if lp.mode == CaptureMode::Grab {
                                let _ = lp.ungrab();
                            }
                            return Ok(());
                        }
                    }
                }
                continue;
            }
        }
        lp.on_event(ev)?;
    }
}
