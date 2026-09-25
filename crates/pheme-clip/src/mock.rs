//! An in-memory clipboard, so the clipboard path can be tested without a desktop.

use std::sync::{Arc, Mutex};

use crate::{ClipError, Clipboard};

#[derive(Debug, Default)]
struct State {
    text: Option<String>,
    /// When set, every call fails with this message.
    fail: Option<String>,
    /// How many times `set_text` succeeded, so a test can tell "wrote the same
    /// thing again" from "wrote nothing".
    sets: u32,
}

/// The test's side of a `MockClipboard`. Cheap to clone; every clone sees the
/// same clipboard.
#[derive(Clone, Debug, Default)]
pub struct MockClipboardHandle(Arc<Mutex<State>>);

impl MockClipboardHandle {
    /// What the clipboard holds now.
    pub fn text(&self) -> Option<String> {
        self.0.lock().unwrap().text.clone()
    }

    /// Stand in for the user copying something.
    pub fn copy(&self, text: &str) {
        self.0.lock().unwrap().text = Some(text.to_string());
    }

    /// How many times the clipboard has been written through the trait.
    pub fn sets(&self) -> u32 {
        self.0.lock().unwrap().sets
    }

    /// Make every later call fail, as a compositor restart would.
    pub fn fail_with(&self, message: &str) {
        self.0.lock().unwrap().fail = Some(message.to_string());
    }

    pub fn stop_failing(&self) {
        self.0.lock().unwrap().fail = None;
    }
}

pub struct MockClipboard(MockClipboardHandle);

impl MockClipboard {
    pub fn new() -> (MockClipboard, MockClipboardHandle) {
        let h = MockClipboardHandle::default();
        (MockClipboard(h.clone()), h)
    }
}

impl Clipboard for MockClipboard {
    fn get_text(&mut self) -> Result<Option<String>, ClipError> {
        let s = self.0 .0.lock().unwrap();
        match &s.fail {
            Some(m) => Err(ClipError::Backend(m.clone())),
            None => Ok(s.text.clone()),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipError> {
        let mut s = self.0 .0.lock().unwrap();
        if let Some(m) = &s.fail {
            return Err(ClipError::Backend(m.clone()));
        }
        s.text = Some(text.to_string());
        s.sets += 1;
        Ok(())
    }
}
