//! In-memory backends for tests.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use pheme_core::CaptureEvent;
use pheme_proto::{Button, KeyCode, ScreenInfo};

use crate::{CaptureMode, InputCapture, InputInject, Result};

#[derive(Default)]
struct CaptureState {
    tx: Option<Sender<CaptureEvent>>,
    mode: Option<CaptureMode>,
    warps: Vec<(i32, i32)>,
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
        self.state
            .lock()
            .unwrap()
            .mode
            .unwrap_or(CaptureMode::Observe)
    }

    pub fn warps(&self) -> Vec<(i32, i32)> {
        self.state.lock().unwrap().warps.clone()
    }
}

impl InputCapture for MockCapture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        self.state.lock().unwrap().tx = Some(tx);
        Ok(())
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        self.state.lock().unwrap().mode = Some(mode);
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.state.lock().unwrap().warps.push((x, y));
        Ok(())
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    fn stop(&mut self) {
        self.state.lock().unwrap().tx = None;
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
