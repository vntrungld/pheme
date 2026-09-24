//! In-memory backends for tests.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use pheme_core::CaptureEvent;
use pheme_proto::{Button, KeyCode, ScreenInfo};

use crate::{CaptureEdge, CaptureMode, Error, InputCapture, InputInject, Result};

#[derive(Default)]
struct CaptureState {
    tx: Option<Sender<CaptureEvent>>,
    mode: Option<CaptureMode>,
    /// Every warp with the capture mode in force when it was recorded.
    warps: Vec<((i32, i32), CaptureMode)>,
    /// When set, the next `set_mode(Grab)` fails once (and clears this).
    fail_next_grab: bool,
    /// When set, the next `set_mode(Observe)` fails once (and clears this).
    fail_next_ungrab: bool,
    /// Every `set_edges` call in order, not just the last, so a test can tell "set
    /// once" from "set repeatedly".
    edges: Vec<Vec<CaptureEdge>>,
    /// When set, `stop()` keeps the event `Sender` instead of dropping it, so nothing
    /// ever observes the channel closing. See `keep_sender_on_stop`.
    keep_sender_on_stop: bool,
    /// Senders `stop()` kept rather than dropped. Held, not leaked, so they die with
    /// the mock rather than for the life of the test binary.
    kept: Vec<Sender<CaptureEvent>>,
}

impl CaptureState {
    fn mode(&self) -> CaptureMode {
        self.mode.unwrap_or(CaptureMode::Observe)
    }
}

pub struct MockCapture {
    screens: Vec<ScreenInfo>,
    state: Arc<Mutex<CaptureState>>,
}

#[derive(Clone)]
pub struct MockCaptureHandle {
    state: Arc<Mutex<CaptureState>>,
}

impl MockCapture {
    pub fn new(screens: Vec<ScreenInfo>) -> (MockCapture, MockCaptureHandle) {
        let state = Arc::new(Mutex::new(CaptureState::default()));
        (
            MockCapture {
                screens,
                state: state.clone(),
            },
            MockCaptureHandle { state },
        )
    }
}

impl MockCaptureHandle {
    /// Delivers an event as if the OS produced it. Returns false if capture is not started
    /// or the receiver is gone.
    pub fn push(&self, ev: CaptureEvent) -> bool {
        let st = self.state.lock().unwrap();
        match &st.tx {
            Some(tx) => tx.send(ev).is_ok(),
            None => false,
        }
    }

    pub fn is_started(&self) -> bool {
        self.state.lock().unwrap().tx.is_some()
    }

    pub fn mode(&self) -> CaptureMode {
        self.state.lock().unwrap().mode()
    }

    pub fn warps(&self) -> Vec<(i32, i32)> {
        self.warps_with_mode().into_iter().map(|(p, _)| p).collect()
    }

    /// Every warp so far, paired with the capture mode in force when it happened.
    pub fn warps_with_mode(&self) -> Vec<((i32, i32), CaptureMode)> {
        self.state.lock().unwrap().warps.clone()
    }

    /// Makes the next `set_mode(Grab)` fail with `Error::Backend("mock grab failure")`,
    /// leaving the mode unchanged; the flag is cleared by that failing call.
    pub fn fail_next_grab(&self) {
        self.state.lock().unwrap().fail_next_grab = true;
    }

    /// Makes the next `set_mode(Observe)` fail with `Error::Backend("mock ungrab
    /// failure")`, leaving the mode unchanged; the flag is cleared by that failing call.
    pub fn fail_next_ungrab(&self) {
        self.state.lock().unwrap().fail_next_ungrab = true;
    }

    /// Simulates the backend thread dying: drops the event `Sender` so the receiver
    /// observes disconnection, as a real backend's event loop exiting would.
    pub fn disconnect(&self) {
        self.state.lock().unwrap().tx = None;
    }

    /// Makes `stop()` hold on to the event `Sender` instead of dropping it, which is
    /// what a backend that breaks the `InputCapture::stop` contract does. It is not a
    /// hypothetical: `PortalCapture::stop()` gives up after 3 s and detaches its
    /// session thread, which still owns a `Sender`. Anything receiving on that channel
    /// then waits forever, which is the case the shutdown path has to survive.
    pub fn keep_sender_on_stop(&self) {
        self.state.lock().unwrap().keep_sender_on_stop = true;
    }

    /// Every `set_edges` call in order.
    pub fn edge_calls(&self) -> Vec<Vec<CaptureEdge>> {
        self.state.lock().unwrap().edges.clone()
    }
}

impl InputCapture for MockCapture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        self.state.lock().unwrap().tx = Some(tx);
        Ok(())
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if mode == CaptureMode::Grab && std::mem::take(&mut st.fail_next_grab) {
            return Err(Error::Backend("mock grab failure".into()));
        }
        if mode == CaptureMode::Observe && std::mem::take(&mut st.fail_next_ungrab) {
            return Err(Error::Backend("mock ungrab failure".into()));
        }
        st.mode = Some(mode);
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let mode = st.mode();
        st.warps.push(((x, y), mode));
        Ok(())
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    fn stop(&mut self) {
        let mut st = self.state.lock().unwrap();
        if let Some(tx) = st.tx.take() {
            if st.keep_sender_on_stop {
                st.kept.push(tx);
            }
        }
    }

    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        let mode = self.set_mode(CaptureMode::Observe);
        let warp = self.warp_cursor(x, y);
        mode.and(warp)
    }

    fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        self.state.lock().unwrap().edges.push(edges.to_vec());
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectCall {
    MoveRel(i32, i32),
    MoveAbs(i32, i32),
    Button(Button, bool),
    Wheel(i32, i32),
    Key(KeyCode, bool),
}

pub struct MockInject {
    screens: Vec<ScreenInfo>,
    log: Arc<Mutex<Vec<InjectCall>>>,
}

#[derive(Clone)]
pub struct MockInjectLog {
    log: Arc<Mutex<Vec<InjectCall>>>,
}

impl MockInject {
    pub fn new(screens: Vec<ScreenInfo>) -> (MockInject, MockInjectLog) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            MockInject {
                screens,
                log: log.clone(),
            },
            MockInjectLog { log },
        )
    }
}

impl MockInjectLog {
    pub fn calls(&self) -> Vec<InjectCall> {
        self.log.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl InputInject for MockInject {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.log.lock().unwrap().push(InjectCall::MoveRel(dx, dy));
        Ok(())
    }
    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        self.log.lock().unwrap().push(InjectCall::MoveAbs(x, y));
        Ok(())
    }
    fn button(&mut self, btn: Button, down: bool) -> Result<()> {
        self.log.lock().unwrap().push(InjectCall::Button(btn, down));
        Ok(())
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.log.lock().unwrap().push(InjectCall::Wheel(dx, dy));
        Ok(())
    }
    fn key(&mut self, code: KeyCode, down: bool) -> Result<()> {
        self.log.lock().unwrap().push(InjectCall::Key(code, down));
        Ok(())
    }
    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::CaptureEvent;
    use pheme_proto::{Button, KeyCode, ScreenInfo};

    fn screens() -> Vec<ScreenInfo> {
        vec![ScreenInfo {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
            primary: true,
        }]
    }

    #[test]
    fn mock_capture_delivers_pushed_events_and_records_mode() {
        let (mut cap, handle) = MockCapture::new(screens());
        assert!(!handle.is_started());
        assert!(
            !handle.push(CaptureEvent::MotionAbs { x: 0, y: 0 }),
            "not started yet"
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        assert!(handle.is_started());
        assert!(handle.push(CaptureEvent::MotionAbs { x: 1, y: 2 }));
        assert_eq!(rx.recv().unwrap(), CaptureEvent::MotionAbs { x: 1, y: 2 });
        cap.set_mode(CaptureMode::Grab).unwrap();
        assert_eq!(handle.mode(), CaptureMode::Grab);
        cap.warp_cursor(5, 6).unwrap();
        assert_eq!(handle.warps(), vec![(5, 6)]);
        assert_eq!(cap.screens(), screens());
    }

    #[test]
    fn fail_next_grab_fails_once_and_leaves_mode_unchanged() {
        let (mut cap, handle) = MockCapture::new(screens());
        let (tx, _rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        handle.fail_next_grab();
        let err = cap.set_mode(CaptureMode::Grab).unwrap_err();
        assert!(matches!(err, Error::Backend(ref m) if m == "mock grab failure"));
        assert_eq!(handle.mode(), CaptureMode::Observe, "mode unchanged");
        // The flag is consumed: the next attempt succeeds.
        cap.set_mode(CaptureMode::Grab).unwrap();
        assert_eq!(handle.mode(), CaptureMode::Grab);
        // Ungrab is never affected by the flag.
        handle.fail_next_grab();
        cap.set_mode(CaptureMode::Observe).unwrap();
        assert_eq!(handle.mode(), CaptureMode::Observe);
        assert!(cap.set_mode(CaptureMode::Grab).is_err(), "flag still armed");
    }

    #[test]
    fn warps_record_the_mode_at_warp_time() {
        let (mut cap, handle) = MockCapture::new(screens());
        cap.warp_cursor(1, 1).unwrap();
        cap.set_mode(CaptureMode::Grab).unwrap();
        cap.warp_cursor(2, 2).unwrap();
        cap.set_mode(CaptureMode::Observe).unwrap();
        cap.warp_cursor(3, 3).unwrap();
        assert_eq!(handle.warps(), vec![(1, 1), (2, 2), (3, 3)]);
        assert_eq!(
            handle.warps_with_mode(),
            vec![
                ((1, 1), CaptureMode::Observe),
                ((2, 2), CaptureMode::Grab),
                ((3, 3), CaptureMode::Observe),
            ]
        );
    }

    #[test]
    fn release_observes_and_warps() {
        let (mut cap, handle) = MockCapture::new(screens());
        cap.set_mode(CaptureMode::Grab).unwrap();
        cap.release(7, 8).unwrap();
        assert_eq!(handle.mode(), CaptureMode::Observe);
        assert_eq!(
            handle.warps_with_mode(),
            vec![((7, 8), CaptureMode::Observe)],
            "the warp must be recorded after the mode drops, or a real backend would \
             still be clipping the pointer when it warps"
        );
    }

    #[test]
    fn a_failed_ungrab_still_warps() {
        let (mut cap, handle) = MockCapture::new(screens());
        cap.set_mode(CaptureMode::Grab).unwrap();
        handle.fail_next_ungrab();
        let r = cap.release(7, 8);
        assert!(r.is_err(), "the error must still be reported");
        assert_eq!(
            handle.warps(),
            vec![(7, 8)],
            "dropping the warp when the ungrab fails strands the pointer off-screen"
        );
    }

    #[test]
    fn disconnect_drops_the_sender_so_the_receiver_sees_eof() {
        let (mut cap, handle) = MockCapture::new(screens());
        let (tx, rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        assert!(handle.is_started());
        handle.disconnect();
        assert!(!handle.is_started());
        assert!(rx.recv().is_err(), "receiver observes disconnection");
        assert!(!handle.push(CaptureEvent::MotionAbs { x: 0, y: 0 }));
        cap.stop(); // still idempotent after disconnect
    }

    #[test]
    fn mock_inject_logs_calls_in_order() {
        let (mut inj, log) = MockInject::new(screens());
        inj.key(KeyCode(0x04), true).unwrap();
        inj.button(Button::Left, false).unwrap();
        inj.mouse_move_rel(1, -1).unwrap();
        inj.mouse_move_abs(7, 8).unwrap();
        inj.wheel(0, 120).unwrap();
        assert_eq!(
            log.calls(),
            vec![
                InjectCall::Key(KeyCode(0x04), true),
                InjectCall::Button(Button::Left, false),
                InjectCall::MoveRel(1, -1),
                InjectCall::MoveAbs(7, 8),
                InjectCall::Wheel(0, 120),
            ]
        );
    }
}
