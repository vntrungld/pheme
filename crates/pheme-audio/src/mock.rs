//! In-memory backends for tests: no device, no threads, fully driven by the test.

use std::sync::{Arc, Mutex};

use crate::{AudioCapture, AudioPlayback, Error, Result};

struct CaptureState {
    sink: Option<rtrb::Producer<i16>>,
    started: bool,
    fail_start: bool,
    healthy: bool,
    overruns: u64,
}

impl Default for CaptureState {
    fn default() -> Self {
        CaptureState {
            sink: None,
            started: false,
            fail_start: false,
            healthy: true,
            overruns: 0,
        }
    }
}

pub struct MockCapture {
    state: Arc<Mutex<CaptureState>>,
}

#[derive(Clone)]
pub struct MockCaptureHandle {
    state: Arc<Mutex<CaptureState>>,
}

impl MockCapture {
    pub fn new() -> (MockCapture, MockCaptureHandle) {
        let state = Arc::new(Mutex::new(CaptureState::default()));
        (
            MockCapture {
                state: state.clone(),
            },
            MockCaptureHandle { state },
        )
    }
}

impl MockCaptureHandle {
    /// Writes samples as if the device had produced them. Returns how many were
    /// accepted; the rest are dropped and counted, exactly as a real backend drops
    /// samples when the ring is full.
    pub fn push(&self, samples: &[i16]) -> usize {
        let mut st = self.state.lock().unwrap();
        let mut n = 0;
        if let Some(sink) = st.sink.as_mut() {
            for s in samples {
                if sink.push(*s).is_err() {
                    break;
                }
                n += 1;
            }
        }
        let dropped = samples.len() - n;
        if dropped > 0 {
            st.overruns += dropped as u64;
        }
        n
    }

    pub fn started(&self) -> bool {
        self.state.lock().unwrap().started
    }

    pub fn overruns(&self) -> u64 {
        self.state.lock().unwrap().overruns
    }

    /// Makes the next `start` fail once.
    pub fn fail_next_start(&self) {
        self.state.lock().unwrap().fail_start = true;
    }

    /// Simulates the device thread dying, so the supervisor's rebuild path can be
    /// tested without a real device.
    pub fn set_healthy(&self, healthy: bool) {
        self.state.lock().unwrap().healthy = healthy;
    }
}

impl AudioCapture for MockCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if std::mem::take(&mut st.fail_start) {
            return Err(Error::Device("mock capture failure".into()));
        }
        st.sink = Some(sink);
        st.started = true;
        Ok(())
    }

    fn device_name(&self) -> String {
        "mock capture".into()
    }

    fn healthy(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.started && st.healthy
    }

    fn stop(&mut self) {
        let mut st = self.state.lock().unwrap();
        st.sink = None;
        st.started = false;
    }
}

struct PlaybackState {
    source: Option<rtrb::Consumer<i16>>,
    recorded: Vec<i16>,
    started: bool,
    fail_start: bool,
    rate: u32,
}

pub struct MockPlayback {
    state: Arc<Mutex<PlaybackState>>,
}

#[derive(Clone)]
pub struct MockPlaybackHandle {
    state: Arc<Mutex<PlaybackState>>,
}

impl MockPlayback {
    pub fn new(rate: u32) -> (MockPlayback, MockPlaybackHandle) {
        let state = Arc::new(Mutex::new(PlaybackState {
            source: None,
            recorded: Vec::new(),
            started: false,
            fail_start: false,
            rate,
        }));
        (
            MockPlayback {
                state: state.clone(),
            },
            MockPlaybackHandle { state },
        )
    }
}

impl MockPlaybackHandle {
    /// Consumes everything waiting in the ring, as a device callback would, and appends
    /// it to the recording. Returns how many samples were taken.
    pub fn drain(&self) -> usize {
        let mut st = self.state.lock().unwrap();
        let mut taken = Vec::new();
        if let Some(source) = st.source.as_mut() {
            while let Ok(s) = source.pop() {
                taken.push(s);
            }
        }
        let n = taken.len();
        st.recorded.extend_from_slice(&taken);
        n
    }

    /// Everything drained so far.
    pub fn recorded(&self) -> Vec<i16> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn started(&self) -> bool {
        self.state.lock().unwrap().started
    }

    /// Makes the next `start` fail once.
    pub fn fail_next_start(&self) {
        self.state.lock().unwrap().fail_start = true;
    }
}

impl AudioPlayback for MockPlayback {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if std::mem::take(&mut st.fail_start) {
            return Err(Error::Device("mock playback failure".into()));
        }
        st.source = Some(source);
        st.started = true;
        Ok(())
    }

    fn rate(&self) -> u32 {
        self.state.lock().unwrap().rate
    }

    fn device_name(&self) -> String {
        "mock playback".into()
    }

    fn healthy(&self) -> bool {
        self.state.lock().unwrap().started
    }

    fn stop(&mut self) {
        let mut st = self.state.lock().unwrap();
        st.source = None;
        st.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioCapture, AudioPlayback};

    #[test]
    fn capture_hands_pushed_samples_to_the_sink() {
        let (mut cap, handle) = MockCapture::new();
        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(16);
        assert!(!handle.started());
        cap.start(producer).unwrap();
        assert!(handle.started());

        assert_eq!(handle.push(&[1, 2, 3]), 3);
        assert_eq!(consumer.pop(), Ok(1));
        assert_eq!(consumer.pop(), Ok(2));
        assert_eq!(consumer.pop(), Ok(3));
    }

    #[test]
    fn capture_drops_samples_when_the_ring_is_full() {
        let (mut cap, handle) = MockCapture::new();
        let (producer, _consumer) = rtrb::RingBuffer::<i16>::new(4);
        cap.start(producer).unwrap();
        assert_eq!(handle.push(&[1, 2, 3, 4, 5, 6]), 4, "only four slots");
        assert_eq!(handle.overruns(), 2);
    }

    #[test]
    fn capture_start_can_be_made_to_fail() {
        let (mut cap, handle) = MockCapture::new();
        handle.fail_next_start();
        let (producer, _consumer) = rtrb::RingBuffer::<i16>::new(4);
        assert!(cap.start(producer).is_err());
        assert!(!handle.started());

        let (producer, _consumer) = rtrb::RingBuffer::<i16>::new(4);
        cap.start(producer).unwrap();
        assert!(handle.started(), "the failure is one-shot");
    }

    #[test]
    fn capture_stop_is_idempotent() {
        let (mut cap, handle) = MockCapture::new();
        let (producer, _consumer) = rtrb::RingBuffer::<i16>::new(4);
        cap.start(producer).unwrap();
        cap.stop();
        cap.stop();
        assert!(!handle.started());
        assert_eq!(handle.push(&[1]), 0, "no sink after stop");
    }

    #[test]
    fn playback_records_what_it_drains() {
        let (mut play, handle) = MockPlayback::new(48_000);
        let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(16);
        play.start(consumer).unwrap();
        for s in [7, 8, 9] {
            producer.push(s).unwrap();
        }
        assert_eq!(handle.drain(), 3);
        assert_eq!(handle.drain(), 0, "nothing left");
        producer.push(10).unwrap();
        handle.drain();
        assert_eq!(handle.recorded(), vec![7, 8, 9, 10]);
    }

    #[test]
    fn playback_reports_the_rate_it_was_built_with() {
        let (play, _handle) = MockPlayback::new(44_100);
        assert_eq!(play.rate(), 44_100);
    }

    #[test]
    fn capture_health_can_be_revoked() {
        let (mut cap, handle) = MockCapture::new();
        assert!(!cap.healthy(), "not started yet");
        let (producer, _consumer) = rtrb::RingBuffer::<i16>::new(4);
        cap.start(producer).unwrap();
        assert!(cap.healthy());
        handle.set_healthy(false);
        assert!(!cap.healthy(), "the supervisor rebuilds on this");
    }

    #[test]
    fn playback_start_can_be_made_to_fail() {
        let (mut play, handle) = MockPlayback::new(48_000);
        handle.fail_next_start();
        let (_producer, consumer) = rtrb::RingBuffer::<i16>::new(4);
        assert!(play.start(consumer).is_err());
        assert!(!handle.started());
    }
}
