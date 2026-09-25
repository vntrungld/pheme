//! An in-memory monitor for tests.
//!
//! The handle is cloneable and readable from the test thread while the
//! `MockMonitor` itself is owned by the service thread, the same split
//! `pheme_clip::mock` uses.

use std::sync::{Arc, Mutex};

use crate::{DisplayError, Monitor};

#[derive(Debug)]
struct Inner {
    input: u16,
    /// Every call to `set_input`, successful or not.
    sets: usize,
    fail: Option<String>,
    caps: String,
}

#[derive(Clone, Debug)]
pub struct MockMonitorHandle(Arc<Mutex<Inner>>);

impl MockMonitorHandle {
    /// The input last set *successfully*. Distinct from `sets`, which counts
    /// attempts: a test that asserts only on one of them cannot tell a
    /// refused command from a failed one.
    pub fn input(&self) -> u16 {
        self.0.lock().unwrap().input
    }

    /// How many times `set_input` was called, including calls that failed.
    pub fn sets(&self) -> usize {
        self.0.lock().unwrap().sets
    }

    pub fn fail_with(&self, message: &str) {
        self.0.lock().unwrap().fail = Some(message.to_string());
    }

    pub fn stop_failing(&self) {
        self.0.lock().unwrap().fail = None;
    }

    pub fn set_caps(&self, caps: &str) {
        self.0.lock().unwrap().caps = caps.to_string();
    }
}

pub struct MockMonitor {
    identity: String,
    location: String,
    inner: Arc<Mutex<Inner>>,
}

impl MockMonitor {
    pub fn new(identity: &str, location: &str, input: u16) -> (Self, MockMonitorHandle) {
        let inner = Arc::new(Mutex::new(Inner {
            input,
            sets: 0,
            fail: None,
            caps: String::new(),
        }));
        let mon = MockMonitor {
            identity: identity.to_string(),
            location: location.to_string(),
            inner: Arc::clone(&inner),
        };
        (mon, MockMonitorHandle(inner))
    }
}

impl Monitor for MockMonitor {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn location(&self) -> &str {
        &self.location
    }

    fn get_input(&mut self) -> Result<u16, DisplayError> {
        let inner = self.inner.lock().unwrap();
        match &inner.fail {
            Some(m) => Err(DisplayError::Backend(m.clone())),
            None => Ok(inner.input),
        }
    }

    fn set_input(&mut self, value: u16) -> Result<(), DisplayError> {
        let mut inner = self.inner.lock().unwrap();
        // Counted before the failure check on purpose. Counting only
        // successes would make "a failed command was still attempted"
        // untestable, and sub-project 5 shipped a test made vacuous by
        // exactly that ordering.
        inner.sets += 1;
        if let Some(m) = &inner.fail {
            return Err(DisplayError::Backend(m.clone()));
        }
        inner.input = value;
        Ok(())
    }

    fn capabilities(&mut self) -> Result<String, DisplayError> {
        let inner = self.inner.lock().unwrap();
        match &inner.fail {
            Some(m) => Err(DisplayError::Backend(m.clone())),
            None => Ok(inner.caps.clone()),
        }
    }
}
