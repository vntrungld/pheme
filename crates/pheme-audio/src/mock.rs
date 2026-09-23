//! In-memory backends for tests: no device, no threads, fully driven by the test.

use std::sync::{Arc, Mutex};

use crate::{AudioCapture, AudioPlayback, Demand, Error, Result, CHANNELS, FRAME_SAMPLES, RATE};

struct CaptureState {
    sink: Option<rtrb::Producer<i16>>,
    started: bool,
    fail_start: bool,
    healthy: bool,
    overruns: u64,
    start_count: u64,
    stopped: bool,
}

impl Default for CaptureState {
    fn default() -> Self {
        CaptureState {
            sink: None,
            started: false,
            fail_start: false,
            healthy: true,
            overruns: 0,
            start_count: 0,
            stopped: false,
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

    /// How many times `start` has succeeded. The demand gate is expected to drive this
    /// past one over a session.
    pub fn start_count(&self) -> u64 {
        self.state.lock().unwrap().start_count
    }

    /// Whether `stop` has been called at least once.
    pub fn stopped(&self) -> bool {
        self.state.lock().unwrap().stopped
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
        st.start_count += 1;
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
        st.stopped = true;
    }
}

struct PlaybackState {
    source: Option<rtrb::Consumer<i16>>,
    recorded: Vec<i16>,
    started: bool,
    fail_start: bool,
    rate: u32,
    /// Samples per channel the modelled device clock has earned but not yet consumed.
    /// Only the fractional remainder is carried over: a device that finds the ring
    /// short plays silence for the rest of its period rather than banking the deficit
    /// and swallowing a burst later.
    credit: f64,
    demand: Demand,
}

/// Samples per channel a device running at `rate` consumes in one 5 ms wire frame's
/// worth of time. 240 at 48 kHz, 220.5 at 44.1 kHz.
fn period_samples(rate: u32) -> f64 {
    f64::from(rate) * FRAME_SAMPLES as f64 / f64::from(RATE)
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
            credit: 0.0,
            demand: Demand::Unknown,
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
    /// Consumes everything waiting in the ring and appends it to the recording. Returns
    /// how many samples were taken.
    ///
    /// **No device clock**: this empties the ring however full it is, so a test that
    /// drives it in a loop lets the playback worker pull frames as fast as it can
    /// resample them. That is not what a sound card does, and a pipeline driven this
    /// way runs in permanent underrun with the jitter buffer discarding nearly
    /// everything that arrives. Use it only where a test wants to read back whatever
    /// has been produced so far; anything testing the audio path wants `drain_frames`.
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

    /// Consumes at most `frames` wire frames' worth of device time, as a real device
    /// callback would, and appends what it got to the recording. Returns how many
    /// samples were taken.
    ///
    /// The allowance is counted at the device's own rate — 240 samples per channel per
    /// frame at 48 kHz, 220.5 at 44.1 kHz — with the fraction carried across calls, so
    /// a test that calls this once per 5 ms consumes audio at exactly the rate the
    /// device would. Anything the ring could not supply is silence the device played,
    /// not credit to spend on the next call.
    pub fn drain_frames(&self, frames: usize) -> usize {
        let mut st = self.state.lock().unwrap();
        st.credit += frames as f64 * period_samples(st.rate);
        let whole = st.credit.floor();
        st.credit -= whole;
        let mut allowed = whole as usize * CHANNELS;
        let mut taken = Vec::new();
        if let Some(source) = st.source.as_mut() {
            while allowed > 0 {
                match source.pop() {
                    Ok(s) => taken.push(s),
                    Err(_) => break,
                }
                allowed -= 1;
            }
        }
        let n = taken.len();
        st.recorded.extend_from_slice(&taken);
        n
    }

    /// Samples waiting in the ring for the device to consume.
    ///
    /// A test that wants to advance in lock step with the playback worker, rather than
    /// on a wall clock, watches this: the worker tops the ring back up a whole frame at
    /// a time, so the count strictly increases when it has popped one.
    pub fn queued(&self) -> usize {
        let st = self.state.lock().unwrap();
        st.source.as_ref().map_or(0, |s| s.slots())
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

    /// Sets what the backend will report about its consumers.
    pub fn set_demand(&self, d: Demand) {
        self.state.lock().unwrap().demand = d;
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

    fn demand(&self) -> Demand {
        self.state.lock().unwrap().demand
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
    use crate::{AudioCapture, AudioPlayback, Demand};

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
    fn a_paced_drain_takes_one_device_period_at_a_time() {
        let (mut play, handle) = MockPlayback::new(48_000);
        let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(crate::FRAME_INTERLEAVED * 4);
        play.start(consumer).unwrap();
        for i in 0..crate::FRAME_INTERLEAVED * 3 {
            producer.push(i as i16).unwrap();
        }
        assert_eq!(handle.drain_frames(1), crate::FRAME_INTERLEAVED);
        assert_eq!(handle.drain_frames(2), crate::FRAME_INTERLEAVED * 2);
        assert_eq!(handle.drain_frames(1), 0, "the ring is empty now");
    }

    #[test]
    fn a_paced_drain_at_44_1_khz_carries_the_fractional_sample() {
        let (mut play, handle) = MockPlayback::new(44_100);
        let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(crate::FRAME_INTERLEAVED * 4);
        play.start(consumer).unwrap();
        for _ in 0..crate::FRAME_INTERLEAVED * 3 {
            producer.push(1).unwrap();
        }
        // 220.5 samples per channel per period: 220, then 221, then 220 again.
        assert_eq!(handle.drain_frames(1), 220 * CHANNELS);
        assert_eq!(handle.drain_frames(1), 221 * CHANNELS);
        assert_eq!(handle.drain_frames(1), 220 * CHANNELS);
    }

    #[test]
    fn a_paced_drain_does_not_bank_what_the_ring_could_not_supply() {
        let (mut play, handle) = MockPlayback::new(48_000);
        let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(crate::FRAME_INTERLEAVED * 4);
        play.start(consumer).unwrap();
        assert_eq!(handle.drain_frames(1), 0, "an empty ring plays silence");
        for _ in 0..crate::FRAME_INTERLEAVED * 3 {
            producer.push(1).unwrap();
        }
        assert_eq!(
            handle.drain_frames(1),
            crate::FRAME_INTERLEAVED,
            "the missed period is gone, not owed"
        );
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

    #[test]
    fn a_playback_backend_reports_unknown_demand_unless_it_knows_better() {
        let (play, _handle) = MockPlayback::new(48_000);
        assert_eq!(
            play.demand(),
            Demand::Unknown,
            "the default must be the one that keeps a microphone open"
        );
    }

    #[test]
    fn a_playback_backend_can_report_what_its_consumers_are_doing() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.set_demand(Demand::Idle);
        assert_eq!(play.demand(), Demand::Idle);
        handle.set_demand(Demand::Wanted);
        assert_eq!(play.demand(), Demand::Wanted);
    }

    #[test]
    fn a_capture_backend_can_be_started_again_after_being_stopped() {
        // The demand gate stops the microphone outright so its indicator goes out, then
        // starts it again when a consumer comes back. A backend that can only be started
        // once would make the gate a one-way door.
        let (mut cap, handle) = MockCapture::new();
        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(16);
        cap.start(producer).unwrap();
        assert_eq!(handle.start_count(), 1);
        assert_eq!(handle.push(&[1, 2]), 2);
        cap.stop();
        assert!(handle.stopped());
        assert!(!handle.started());

        let (producer, mut consumer2) = rtrb::RingBuffer::<i16>::new(16);
        cap.start(producer).unwrap();
        assert_eq!(handle.start_count(), 2, "a second start really started it");
        assert!(handle.started());
        assert_eq!(handle.push(&[7, 8]), 2);
        assert_eq!(consumer2.pop(), Ok(7));
        assert_eq!(consumer2.pop(), Ok(8));
        // The first ring received only what was pushed before the stop.
        assert_eq!(consumer.pop(), Ok(1));
        assert_eq!(consumer.pop(), Ok(2));
        assert!(consumer.pop().is_err(), "nothing went to the old ring");
    }
}
