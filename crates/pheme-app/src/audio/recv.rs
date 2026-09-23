use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use pheme_audio::drift::DriftController;
use pheme_audio::frame::Frame;
use pheme_audio::jitter::{JitterBuffer, JitterStats, Pop};
use pheme_audio::{AudioPlayback, Demand, CHANNELS, FRAME_INTERLEAVED, FRAME_SAMPLES, RATE};
use rubato::Resampler;
use tokio::sync::watch;
use tracing::{info, warn};

use super::{nap, FailureLog, PumpEnd, LINGER, RETRY, TICK};

/// Playback ring, in wire frames' worth of samples. It is sized for the worst plausible
/// device rate; the worker keeps the actual fill near one frame regardless.
const PLAYBACK_RING_SAMPLES: usize = FRAME_INTERLEAVED * 16;
/// Frames the receive task may queue for the worker before dropping them.
const FRAME_QUEUE: usize = 256;

/// Where the playback backend comes from. Tests inject their own.
pub enum PlaybackSource {
    Detect(Option<String>),
    /// Started once and never rebuilt — for tests.
    Backend(Box<dyn AudioPlayback>),
    Disabled,
}

/// What `pump_in` needs to report demand and act on a reset, bundled so the function
/// stays under the argument-count lint rather than growing a tenth positional parameter.
struct DemandGate<'a> {
    wanted_tx: &'a watch::Sender<bool>,
    linger: Duration,
    reset_requested: &'a AtomicBool,
}

/// Cumulative counters; the stats line reports the difference per interval.
#[derive(Default)]
pub struct InStats {
    /// Last observed jitter-buffer depth, in milliseconds. Not cumulative.
    pub depth_ms: AtomicU64,
    pub lost: AtomicU64,
    pub late: AtomicU64,
    pub underruns: AtomicU64,
    pub resets: AtomicU64,
    /// Frames the receive task had to drop because the worker was not keeping up.
    pub dropped: AtomicU64,
    /// Times the jitter buffer hit its ceiling and discarded a backlog. Anything other
    /// than zero means the playback device is consuming persistently slower than the
    /// sender produces, by more than the drift controller is allowed to correct.
    pub overflows: AtomicU64,
}

/// Owns a playback backend and the worker that feeds it.
pub struct RecvSide {
    frames: Option<Sender<Frame>>,
    stats: Arc<InStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    wanted_rx: watch::Receiver<bool>,
    reset_requested: Arc<AtomicBool>,
}

impl RecvSide {
    pub fn spawn(source: PlaybackSource, stats: Arc<InStats>) -> RecvSide {
        RecvSide::spawn_with_linger(source, stats, LINGER)
    }

    /// As `spawn`, with the closing-edge debounce named explicitly. Tests use a short one.
    pub fn spawn_with_linger(
        source: PlaybackSource,
        stats: Arc<InStats>,
        linger: Duration,
    ) -> RecvSide {
        let stop = Arc::new(AtomicBool::new(false));
        let (wanted_tx, wanted_rx) = watch::channel(false);
        let reset_requested = Arc::new(AtomicBool::new(false));
        if matches!(source, PlaybackSource::Disabled) {
            return RecvSide {
                frames: None,
                stats,
                stop,
                thread: None,
                wanted_rx,
                reset_requested,
            };
        }
        let (tx, rx) = crossbeam_channel::bounded::<Frame>(FRAME_QUEUE);
        let thread = {
            let stop = stop.clone();
            let stats = stats.clone();
            let reset_requested = reset_requested.clone();
            match std::thread::Builder::new()
                .name("pheme-audio-in".into())
                .spawn(move || {
                    in_thread(source, rx, stop, stats, wanted_tx, linger, reset_requested)
                }) {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!("could not start the audio playback thread: {e}");
                    None
                }
            }
        };
        RecvSide {
            frames: Some(tx),
            stats,
            stop,
            thread,
            wanted_rx,
            reset_requested,
        }
    }

    /// Hands a received frame to the worker. Never blocks: a full queue means the worker
    /// is not keeping up, and a dropped frame is better than a stalled receive task.
    pub fn push(&self, f: Frame) {
        match self.frames.as_ref() {
            Some(tx) if tx.try_send(f).is_ok() => {}
            _ => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Whether anything is consuming what this side plays, debounced by the linger.
    ///
    /// False whenever no backend is running: a backend that is not running cannot deliver
    /// audio to anything, so asking the far end to open a microphone for it would be pure
    /// cost. That is not a violation of the fail-open rule — that rule protects against
    /// *not knowing*, and this is knowing the answer is no.
    pub fn wanted(&self) -> watch::Receiver<bool> {
        self.wanted_rx.clone()
    }

    /// Drops everything buffered and prefills again.
    ///
    /// The client calls this as it asks the server to reopen its microphone, *before* the
    /// audio starts arriving. The worker acts on it at its next tick, which is within
    /// `TICK`, long before the first frame of a resumed stream can cross the network.
    pub fn reset(&self) {
        self.reset_requested.store(true, Ordering::SeqCst);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.frames = None; // disconnects the worker's receiver
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RecvSide {
    fn drop(&mut self) {
        self.stop();
    }
}

fn in_thread(
    source: PlaybackSource,
    rx: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    stats: Arc<InStats>,
    wanted_tx: watch::Sender<bool>,
    linger: Duration,
    reset_requested: Arc<AtomicBool>,
) {
    let (device, mut injected, rebuild) = match source {
        PlaybackSource::Detect(d) => (d, None, true),
        PlaybackSource::Backend(b) => (None, Some(b), false),
        PlaybackSource::Disabled => return,
    };
    let mut failures = FailureLog::default();
    loop {
        // Between backends nothing can be playing to anyone, so the gate is shut: before
        // a backend is built, after one stops, and on every return below.
        let _ = wanted_tx.send(false);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let built = match injected.take() {
            Some(b) => Ok(b),
            None => pheme_audio::detect_playback(device.as_deref()),
        };
        let mut backend = match built {
            Ok(b) => b,
            Err(pheme_audio::Error::Unsupported(why)) => {
                info!("audio playback is not available here: {why}");
                return;
            }
            Err(e) => {
                failures.report(format!("audio playback unavailable: {e}"));
                if !rebuild || !nap(RETRY, &stop) {
                    return;
                }
                continue;
            }
        };
        let (producer, consumer) = rtrb::RingBuffer::<i16>::new(PLAYBACK_RING_SAMPLES);
        if let Err(e) = backend.start(consumer) {
            failures.report(format!("audio playback failed to start: {e}"));
            if !rebuild || !nap(RETRY, &stop) {
                return;
            }
            continue;
        }
        failures.cleared();
        let rate = backend.rate();
        info!(device = %backend.device_name(), rate, "audio playback started");
        let gate = DemandGate {
            wanted_tx: &wanted_tx,
            linger,
            reset_requested: &reset_requested,
        };
        let end = pump_in(backend.as_ref(), producer, &rx, &stop, &stats, rate, &gate);
        backend.stop();
        let _ = wanted_tx.send(false);
        match end {
            PumpEnd::Stopped => return,
            // `pump_in` has no gate of its own to close; the enum is shared with
            // send.rs's `pump_out`, so the match must still cover it.
            PumpEnd::Unwanted => return,
            PumpEnd::Failed(why) => {
                failures.report(format!("audio playback stopped: {why}"));
            }
        }
        if stop.load(Ordering::SeqCst) || !rebuild {
            return;
        }
        if !nap(RETRY, &stop) {
            return;
        }
    }
}

fn pump_in(
    backend: &dyn AudioPlayback,
    mut producer: rtrb::Producer<i16>,
    rx: &Receiver<Frame>,
    stop: &AtomicBool,
    stats: &InStats,
    rate: u32,
    gate: &DemandGate,
) -> PumpEnd {
    let base = f64::from(rate) / f64::from(RATE);
    let params = rubato::SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: rubato::SincInterpolationType::Cubic,
        oversampling_factor: 128,
        window: rubato::WindowFunction::BlackmanHarris2,
    };
    // 1.1 is the widest ratio change the resampler will accept later; the drift
    // controller never asks for more than 0.1 %.
    let mut resampler =
        match rubato::SincFixedIn::<f32>::new(base, 1.1, params, FRAME_SAMPLES, CHANNELS) {
            Ok(r) => r,
            Err(e) => return PumpEnd::Failed(format!("building the playback resampler: {e}")),
        };
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; FRAME_SAMPLES]; CHANNELS];
    let mut output = resampler.output_buffer_allocate(true);
    let mut jitter = JitterBuffer::new();
    let mut drift = DriftController::new(base);
    let frame_out = (FRAME_SAMPLES as f64 * base).ceil() as usize * CHANNELS;
    let keep = 2 * frame_out;
    let mut idle_since: Option<Instant> = None;

    while !stop.load(Ordering::SeqCst) {
        if gate.reset_requested.swap(false, Ordering::SeqCst) {
            jitter.restart();
        }
        if !backend.healthy() {
            return PumpEnd::Failed("the playback device stopped".into());
        }
        loop {
            match rx.try_recv() {
                Ok(f) => jitter.push(f),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return PumpEnd::Stopped,
            }
        }
        while PLAYBACK_RING_SAMPLES - producer.slots() < keep && producer.slots() >= frame_out {
            let st = jitter.stats();
            let ratio = drift.tick(st.depth, st.target);
            let _ = resampler.set_resample_ratio(ratio, true);
            let samples = match jitter.pop() {
                Pop::Data(s) | Pop::Conceal(s) => s,
                Pop::Idle => vec![0i16; FRAME_INTERLEAVED],
            };
            for (i, pair) in samples.chunks_exact(CHANNELS).enumerate() {
                for (c, plane) in input.iter_mut().enumerate() {
                    plane[i] = f32::from(pair[c]) / 32_768.0;
                }
            }
            let produced = match resampler.process_into_buffer(&input, &mut output, None) {
                Ok((_, out)) => out,
                Err(e) => return PumpEnd::Failed(format!("resampling: {e}")),
            };
            for i in 0..produced {
                // Whole frames only. Pushing sample by sample and discarding the
                // `Result` means a ring that fills mid-pair takes one channel and drops
                // the other, which shifts every later sample by one and swaps left and
                // right permanently — the ring never resynchronises on its own.
                if producer.slots() < CHANNELS {
                    break;
                }
                for plane in output.iter() {
                    let _ = producer.push(to_i16(plane[i]));
                }
            }
        }
        let now = backend.demand();
        match now {
            Demand::Idle => {
                if idle_since.is_none() {
                    idle_since = Some(Instant::now());
                }
                if idle_since.is_some_and(|t| t.elapsed() >= gate.linger) {
                    let _ = gate.wanted_tx.send(false);
                }
            }
            Demand::Wanted | Demand::Unknown => {
                idle_since = None;
                let _ = gate.wanted_tx.send(true);
            }
        }
        publish(stats, jitter.stats());
        std::thread::sleep(TICK);
    }
    PumpEnd::Stopped
}

/// One resampled sample, converted to the wire's i16.
///
/// The sinc resampler overshoots on near-full-scale input, so values outside +/-1.0 reach
/// this in normal operation and something has to decide what they become. A plain `f32 as
/// i16` cast already saturates rather than wrapping, so the explicit clamp is
/// defence-in-depth: it states the intent, and it keeps the rails correct if this is ever
/// rewritten into a conversion that would not saturate on its own. No test can
/// distinguish the clamped form from the bare cast, which is exactly why the intent is
/// written down here.
fn to_i16(v: f32) -> i16 {
    (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

fn publish(stats: &InStats, s: JitterStats) {
    // The pre-pop depth is the audio *waiting* to be played, which is what the latency
    // budget in the spec counts. `s.depth` is sampled after the pop removed its frame
    // and reads one frame lower.
    stats
        .depth_ms
        .store(s.depth_prepop as u64 * 5, Ordering::Relaxed);
    stats.lost.store(s.lost, Ordering::Relaxed);
    stats.late.store(s.late, Ordering::Relaxed);
    stats.underruns.store(s.underruns, Ordering::Relaxed);
    stats.resets.store(s.resets, Ordering::Relaxed);
    stats.overflows.store(s.overflows, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wait_until;
    use pheme_audio::mock::{MockPlayback, MockPlaybackHandle};
    use pheme_audio::pack::Packer;
    use pheme_audio::FRAME_US;
    use std::time::Duration;

    /// The tone's amplitude, measured robustly.
    ///
    /// Not the peak. The sinc resampler overshoots by up to about 16 % of the step
    /// whenever the jitter buffer splices two frames that do not join smoothly — the
    /// end of the stream, or a concealed frame after a lost one — and that ringing is
    /// correct behaviour, inaudible next to the splice that caused it. A 440 Hz sine
    /// spends about 9 % of its samples within 1 % of full scale, so the 99th percentile
    /// of |s| is the amplitude, and a handful of ringing samples does not move it. A
    /// pipeline that changed the level moves it by exactly the change.
    fn tone_level(rec: &[i16]) -> i32 {
        if rec.is_empty() {
            return 0;
        }
        let mut mags: Vec<i32> = rec.iter().map(|s| i32::from(s.unsigned_abs())).collect();
        mags.sort_unstable();
        mags[(mags.len() * 99 / 100).min(mags.len() - 1)]
    }

    /// One frame of a 440 Hz sine at about a third of full scale, continuing from frame
    /// index `i` so consecutive frames join smoothly.
    fn sine_frame(i: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(FRAME_INTERLEAVED);
        for n in 0..pheme_audio::FRAME_SAMPLES {
            let t = (i * pheme_audio::FRAME_SAMPLES + n) as f32 / 48_000.0;
            let v = (10_000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16;
            out.push(v);
            out.push(v);
        }
        out
    }

    #[test]
    fn disabled_playback_drops_what_it_is_given() {
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Disabled, stats.clone());
        audio.push(Frame {
            seq: 0,
            ts_us: 0,
            bytes: vec![0; pheme_audio::FRAME_BYTES],
        });
        audio.stop();
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
    }

    /// Feeds `count` frames of a 440 Hz sine into `audio`, one frame per device period,
    /// in lock step with the playback worker. Returns the `depth_ms` seen on each
    /// iteration.
    ///
    /// Two things this does not do. It does not use `MockPlaybackHandle::drain`, which
    /// empties the ring however full it is and so lets the worker run away from the
    /// sender: the jitter buffer's read cursor overtakes everything that arrives and
    /// the test passes on two frames of audio plus concealment copies of them. And it
    /// does not sleep. Pacing on a wall clock looks realistic but is not reproducible
    /// — a thread the OS deschedules for a few tens of milliseconds moves one side of
    /// the loop and not the other, and the two stay apart afterwards because only the
    /// drift controller can close the gap, at 0.1 %. Waiting for the worker to top the
    /// ring back up instead costs a stall nothing but time, and keeps pops and pushes
    /// one to one.
    fn play_paced(
        audio: &RecvSide,
        handle: &MockPlaybackHandle,
        rate: u32,
        count: usize,
    ) -> Vec<u64> {
        let target = ring_target(rate);
        let mut packer = Packer::new();
        let mut depths = Vec::with_capacity(count);
        for i in 0..count {
            let f = packer
                .push(&sine_frame(i), i as u64 * FRAME_US)
                .expect("a sine is never silent");
            audio.push(f);
            handle.drain_frames(1);
            depths.push(audio.stats.depth_ms.load(Ordering::Relaxed));
            assert!(
                wait_until(|| handle.queued() >= target, Duration::from_secs(5)),
                "the playback worker never refilled the ring after frame {i}"
            );
        }
        depths
    }

    /// The ring fill the playback worker maintains: two output frames at the device's
    /// rate. This mirrors `keep` in `pump_in`, and waiting for the ring to come back to
    /// it after each device period is what keeps the test in lock step with the worker.
    fn ring_target(rate: u32) -> usize {
        (FRAME_SAMPLES as f64 * f64::from(rate) / f64::from(RATE)).ceil() as usize * CHANNELS * 2
    }

    /// The jitter buffer must be holding about its target: neither empty nor filling.
    ///
    /// `audio_depth_ms` reports the depth *before* each pop, which is the audio waiting
    /// to be played. With a 2-frame target that reads as 10 ms, dipping to 5 at 44.1 kHz
    /// where one wire frame resamples to a hair more than one device period and the
    /// worker occasionally takes two pops to refill the ring. So a lower bound of 5 has
    /// a whole frame of headroom, where the same number against the post-pop depth had
    /// none and was a latent flake. The upper bound catches a buffer that is quietly
    /// filling up; it rises by one frame for the same reason.
    fn assert_the_buffer_holds_audio(depths: &[u64]) {
        let mut steady = depths[50..].to_vec();
        steady.sort_unstable();
        let median = steady[steady.len() / 2];
        let max = steady.last().copied().unwrap_or(0);
        assert!(
            median >= 5,
            "median jitter depth {median} ms: the buffer is running empty, so the \
             playback worker is outrunning the sender"
        );
        assert!(
            max <= 20,
            "jitter depth reached {max} ms: the buffer is filling up, so the sender is \
             outrunning the playback worker"
        );
    }

    #[test]
    fn frames_reach_the_playback_device() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let depths = play_paced(&audio, &handle, 48_000, 200);
        // Read the counters before the tail: once the sender stops, the buffer runs dry
        // and conceals, which is correct but would mask frames lost mid-stream.
        let late = stats.late.load(Ordering::Relaxed);
        let lost = stats.lost.load(Ordering::Relaxed);
        let underruns = stats.underruns.load(Ordering::Relaxed);
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(5));
            handle.drain_frames(1);
        }
        audio.stop();

        // Lock step keeps pops and pushes one to one, but not perfectly: the drift
        // controller runs the ratio a fraction above the base, so about one frame in
        // five hundred resamples to one extra sample and the worker eventually needs a
        // second pop to refill the ring. That costs a single frame. Anything beyond a
        // couple in two hundred is the pipeline, not the arithmetic.
        assert!(
            late <= 2,
            "{late} frames arrived after their slot had passed"
        );
        assert!(lost <= 2, "{lost} frames never reached the jitter buffer");
        assert!(
            underruns <= 2,
            "the playback buffer ran dry {underruns} times"
        );
        assert_the_buffer_holds_audio(&depths);

        let rec = handle.recorded();
        let level = tone_level(&rec);
        assert!(
            (level - 10_000).abs() < 1_500,
            "played level {level}, expected about 10000 — the pipeline changed the level"
        );
        // The device consumed one frame per iteration, so anything much below 200 means
        // the worker could not keep it fed.
        assert!(
            rec.len() > 190 * FRAME_INTERLEAVED,
            "only {} samples reached the device, expected about {}",
            rec.len(),
            200 * FRAME_INTERLEAVED
        );
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_device_at_another_rate_is_resampled() {
        let (play, handle) = MockPlayback::new(44_100);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let depths = play_paced(&audio, &handle, 44_100, 200);
        let late = stats.late.load(Ordering::Relaxed);
        let lost = stats.lost.load(Ordering::Relaxed);
        let underruns = stats.underruns.load(Ordering::Relaxed);
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(5));
            handle.drain_frames(1);
        }
        audio.stop();

        // Lock step keeps pops and pushes one to one, but not perfectly: the drift
        // controller runs the ratio a fraction above the base, so about one frame in
        // five hundred resamples to one extra sample and the worker eventually needs a
        // second pop to refill the ring. That costs a single frame. Anything beyond a
        // couple in two hundred is the pipeline, not the arithmetic.
        assert!(
            late <= 2,
            "{late} frames arrived after their slot had passed"
        );
        assert!(lost <= 2, "{lost} frames never reached the jitter buffer");
        assert!(
            underruns <= 2,
            "the playback buffer ran dry {underruns} times"
        );
        assert_the_buffer_holds_audio(&depths);

        let rec = handle.recorded();
        let level = tone_level(&rec);
        assert!(
            (level - 10_000).abs() < 1_500,
            "resampling to 44.1 kHz changed the level: {level}"
        );
        // 220.5 device samples per channel per 5 ms period, 200 periods.
        assert!(
            rec.len() > 190 * 220 * CHANNELS,
            "only {} samples reached the 44.1 kHz device",
            rec.len()
        );
    }

    #[test]
    fn an_injected_playback_backend_that_fails_gives_up_quietly() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.fail_next_start();
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Backend(Box::new(play)), stats);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!handle.started());
        audio.stop(); // must not hang
    }

    /// A full-scale sine, which is where a conversion bug shows up.
    fn loud_sine_frame(i: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(FRAME_INTERLEAVED);
        for n in 0..FRAME_SAMPLES {
            let t = (i * FRAME_SAMPLES + n) as f32 / 48_000.0;
            let v = (32_000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16;
            out.push(v);
            out.push(v);
        }
        out
    }

    #[test]
    fn a_full_scale_signal_is_not_clipped_wrapped_or_inverted() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let mut packer = Packer::new();
        for i in 0..200 {
            let f = packer
                .push(&loud_sine_frame(i), i as u64 * FRAME_US)
                .expect("a sine is never silent");
            audio.push(f);
            handle.drain_frames(1);
            assert!(wait_until(
                || handle.queued() >= FRAME_INTERLEAVED,
                Duration::from_secs(5)
            ));
        }
        audio.stop();

        let rec = handle.recorded();
        // Skip the prefill, where the worker is emitting silence.
        let body = &rec[FRAME_INTERLEAVED * 10..];
        let peak = body.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        // The upper bound is 32_768, not 32_767: `i16::MIN.abs()` is 32768, and -32768 is
        // exactly what the production clamp produces when the resampler's sinc overshoot
        // on a near-full-scale signal reaches the rail. That is correct behaviour, not
        // clipping damage. The bound that earns its keep here is the lower one, which
        // catches attenuation; the rails and scale are pinned separately, by `to_i16`'s
        // own test below.
        assert!(
            (30_000..=32_768).contains(&peak),
            "peak {peak} out of a 32000 input: the signal was clipped or attenuated"
        );

        // No step bound is taken here. A pipeline recording legitimately contains large
        // inter-sample steps: the jitter buffer emits a silence frame when it has nothing
        // (`Pop::Idle`) and a full-gain copy of the previous frame when it conceals
        // (`Pop::Conceal`), and either one is a phase discontinuity in a continuous sine —
        // up to 32 768 for a drop to silence and roughly twice that across a half period.
        // A step bound here would be measuring whether the buffer ever ran dry, not
        // whether the conversion's rails and scale are correct. Those are pinned directly
        // instead, by `to_i16`'s own test below.
    }

    const FAST_LINGER: Duration = Duration::from_millis(100);

    #[test]
    fn demand_is_false_before_a_backend_is_running() {
        let (play, _handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        // Nothing has reported consumers yet, and a backend that is not running cannot
        // deliver audio to anything, so asking for a microphone would be pure cost.
        assert!(!*audio.wanted().borrow());
        audio.stop();
    }

    #[test]
    fn an_unknown_backend_is_treated_as_wanted() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));
        let w = audio.wanted();
        assert!(
            wait_until(|| *w.borrow(), Duration::from_secs(2)),
            "Unknown must mean open, or a backend that cannot detect consumers silently \
             kills the feature"
        );
        audio.stop();
    }

    #[test]
    fn an_idle_backend_closes_the_gate_only_after_the_linger() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.set_demand(Demand::Wanted);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        let w = audio.wanted();
        assert!(wait_until(|| *w.borrow(), Duration::from_secs(2)));

        handle.set_demand(Demand::Idle);
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            *w.borrow(),
            "the gate must not close on the first idle poll: applications probe devices"
        );
        assert!(
            wait_until(|| !*w.borrow(), Duration::from_secs(2)),
            "but it must close once the linger has passed"
        );
        audio.stop();
    }

    #[test]
    fn a_consumer_returning_inside_the_linger_never_closes_the_gate() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.set_demand(Demand::Wanted);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            Duration::from_millis(400),
        );
        let w = audio.wanted();
        assert!(wait_until(|| *w.borrow(), Duration::from_secs(2)));

        for _ in 0..5 {
            handle.set_demand(Demand::Idle);
            std::thread::sleep(Duration::from_millis(80));
            handle.set_demand(Demand::Wanted);
            std::thread::sleep(Duration::from_millis(20));
            assert!(*w.borrow(), "the microphone must not flap");
        }
        audio.stop();
    }

    #[test]
    fn a_reset_empties_the_buffer() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats.clone(),
            FAST_LINGER,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let mut packer = Packer::new();
        for i in 0..20 {
            let f = packer
                .push(&sine_frame(i), i as u64 * FRAME_US)
                .expect("a sine is never silent");
            audio.push(f);
        }
        assert!(wait_until(
            || stats.depth_ms.load(Ordering::Relaxed) > 0,
            Duration::from_secs(2)
        ));
        let before = stats.resets.load(Ordering::Relaxed);
        audio.reset();
        assert!(
            wait_until(
                || stats.resets.load(Ordering::Relaxed) > before,
                Duration::from_secs(2)
            ),
            "the worker must act on the reset"
        );
        audio.stop();
    }

    #[test]
    fn the_sample_conversion_scales_and_rails_correctly() {
        // Debt (e), pinned where it actually lives. The resampler overshoots on
        // near-full-scale input, so values outside +/-1.0 reach this conversion in normal
        // operation. `f32 as i16` already saturates rather than wraps in safe Rust — see
        // `to_i16`'s doc comment — so what these pin is the scale and the rounding, plus
        // the rails the clamp states as intent: a wrong scale factor or a truncation
        // instead of a round would still be a real, audible defect, just not a wrap.
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(0.5), 16_384);
        assert_eq!(to_i16(-0.5), -16_384);
        assert_eq!(to_i16(1.0), 32_767, "the positive rail");
        assert_eq!(to_i16(-1.0), -32_768, "the negative rail");
        assert_eq!(to_i16(1.5), 32_767, "an overshoot clamps, it does not wrap");
        assert_eq!(to_i16(-1.5), -32_768, "and the same below");
        assert_eq!(to_i16(1e9), 32_767, "however far outside it lands");
        // These two are the ones that can fail. 0.9 pins the scale factor: a 32_767.0
        // scale yields 29_490. A third pins the rounding: truncation yields 10_922.
        assert_eq!(to_i16(0.9), 29_491, "the scale is 32_768, not 32_767");
        assert_eq!(
            to_i16(1.0 / 3.0),
            10_923,
            "the conversion rounds, it does not truncate"
        );
        assert_eq!(to_i16(-1e9), -32_768);
    }
}
