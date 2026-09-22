//! Audio wiring between the backends in `pheme-audio` and the QUIC session.
//!
//! Each role runs one supervisor thread that owns its backend and rebuilds it every five
//! seconds for as long as it is failing. Audio never affects the keyboard and mouse
//! session: nothing here can make `run_client` or `run_server` return an error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use pheme_audio::drift::DriftController;
use pheme_audio::frame::Frame;
use pheme_audio::jitter::{JitterBuffer, JitterStats, Pop};
use pheme_audio::pack::Packer;
use pheme_audio::{AudioCapture, AudioPlayback, CHANNELS, FRAME_INTERLEAVED, FRAME_SAMPLES};
use pheme_audio::{FRAME_US, RATE};
use pheme_net::PeerSender;
use pheme_proto::{AudioStream, Msg};
use rubato::Resampler;
use tracing::{debug, info, warn};

/// How long a failed backend waits before it is rebuilt.
const RETRY: Duration = Duration::from_secs(5);
/// How often the supervisor threads wake up.
const TICK: Duration = Duration::from_millis(2);
/// Capture ring, in frames. One second of audio is plenty of slack for a thread that
/// wakes every 2 ms.
const CAPTURE_RING_FRAMES: usize = 200;

/// Where the capture backend comes from. Tests inject their own; production detects one.
pub enum CaptureSource {
    /// Detect the platform backend, optionally naming a device.
    Detect(Option<String>),
    /// Use this backend. It is started once and never rebuilt, because a `Box` cannot be
    /// started twice — this variant exists for tests.
    Backend(Box<dyn AudioCapture>),
    /// Run no audio at all.
    Disabled,
}

#[derive(Default)]
pub struct OutCounters {
    /// Frames handed to the transport.
    pub sent: AtomicU64,
    /// Frames dropped by silence suppression.
    pub suppressed: AtomicU64,
}

/// Owns the client's capture backend and the thread that packs and sends its frames.
pub struct AudioOut {
    peer: Arc<Mutex<Option<PeerSender>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioOut {
    pub fn spawn(source: CaptureSource, counters: Arc<OutCounters>) -> AudioOut {
        let peer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = match source {
            CaptureSource::Disabled => None,
            src => {
                let peer = peer.clone();
                let stop = stop.clone();
                match std::thread::Builder::new()
                    .name("pheme-audio-out".into())
                    .spawn(move || out_thread(src, peer, stop, counters))
                {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warn!("could not start the audio thread: {e}");
                        None
                    }
                }
            }
        };
        AudioOut { peer, stop, thread }
    }

    /// Attaches the session's transport, or detaches it when the session ends. While no
    /// peer is attached the thread keeps draining the device and simply discards frames.
    pub fn set_peer(&self, sender: Option<PeerSender>) {
        *self.peer.lock().unwrap() = sender;
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Logs a repeating failure loudly once and quietly while it repeats unchanged.
///
/// `detect_*` never returns `Unsupported` on Linux or Windows — a daemon that is not
/// running at startup but appears later must still be picked up — so a machine with no
/// usable audio fails the same way every five seconds for the life of the process. At
/// one `warn!` each that is roughly 17 000 lines a day, which buries the input logs
/// this project actually needs. The first failure of a kind stays at `warn!`; identical
/// repeats drop to `debug!`; a success restores the volume.
#[derive(Default)]
struct FailureLog {
    last: Option<String>,
}

impl FailureLog {
    /// Records `msg` and returns whether it deserves a `warn!` rather than a `debug!`.
    fn is_new(&mut self, msg: &str) -> bool {
        if self.last.as_deref() == Some(msg) {
            return false;
        }
        self.last = Some(msg.to_string());
        true
    }

    fn report(&mut self, msg: String) {
        if self.is_new(&msg) {
            warn!("{msg}");
        } else {
            debug!("{msg}");
        }
    }

    /// The backend came up: the next failure is loud again.
    fn cleared(&mut self) {
        self.last = None;
    }
}

/// Sleeps in short steps so `stop` is noticed promptly. Returns false if asked to stop.
fn nap(total: Duration, stop: &AtomicBool) -> bool {
    let mut left = total;
    while left > Duration::ZERO {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let step = left.min(Duration::from_millis(50));
        std::thread::sleep(step);
        left -= step;
    }
    !stop.load(Ordering::SeqCst)
}

fn out_thread(
    source: CaptureSource,
    peer: Arc<Mutex<Option<PeerSender>>>,
    stop: Arc<AtomicBool>,
    counters: Arc<OutCounters>,
) {
    let (device, mut injected, rebuild) = match source {
        CaptureSource::Detect(d) => (d, None, true),
        CaptureSource::Backend(b) => (None, Some(b), false),
        CaptureSource::Disabled => return,
    };
    let mut failures = FailureLog::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let built = match injected.take() {
            Some(b) => Ok(b),
            None => pheme_audio::detect_capture(device.as_deref()),
        };
        let mut backend = match built {
            Ok(b) => b,
            Err(pheme_audio::Error::Unsupported(why)) => {
                info!("audio capture is not available here: {why}");
                return;
            }
            Err(e) => {
                failures.report(format!("audio capture unavailable: {e}"));
                if !rebuild || !nap(RETRY, &stop) {
                    return;
                }
                continue;
            }
        };
        let (producer, consumer) =
            rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * CAPTURE_RING_FRAMES);
        if let Err(e) = backend.start(producer) {
            failures.report(format!("audio capture failed to start: {e}"));
            if !rebuild || !nap(RETRY, &stop) {
                return;
            }
            continue;
        }
        failures.cleared();
        info!(device = %backend.device_name(), "audio capture started");
        let end = pump_out(backend.as_ref(), consumer, &peer, &stop, &counters);
        backend.stop();
        if let PumpEnd::Failed(why) = end {
            failures.report(format!("audio capture stopped: {why}"));
        }
        if stop.load(Ordering::SeqCst) || !rebuild {
            return;
        }
        if !nap(RETRY, &stop) {
            return;
        }
    }
}

/// Why a pump returned.
enum PumpEnd {
    /// A stop was requested, or the sender was dropped: do not rebuild.
    Stopped,
    /// The device, or something the pump owns, failed: rebuild after the retry delay.
    Failed(String),
}

/// Packs whole frames out of the ring and sends them while a peer is attached. Returns
/// when the backend dies or a stop is requested.
fn pump_out(
    backend: &dyn AudioCapture,
    mut consumer: rtrb::Consumer<i16>,
    peer: &Mutex<Option<PeerSender>>,
    stop: &AtomicBool,
    counters: &OutCounters,
) -> PumpEnd {
    let mut packer = Packer::new();
    let mut frame = Vec::with_capacity(FRAME_INTERLEAVED);
    let mut taken = 0u64;
    while !stop.load(Ordering::SeqCst) {
        if !backend.healthy() {
            return PumpEnd::Failed("the capture device stopped".into());
        }
        while consumer.slots() >= FRAME_INTERLEAVED {
            frame.clear();
            for _ in 0..FRAME_INTERLEAVED {
                match consumer.pop() {
                    Ok(s) => frame.push(s),
                    Err(_) => break,
                }
            }
            if frame.len() != FRAME_INTERLEAVED {
                break;
            }
            let ts_us = taken * FRAME_US;
            taken += 1;
            match packer.push(&frame, ts_us) {
                None => {
                    counters.suppressed.fetch_add(1, Ordering::Relaxed);
                }
                Some(f) => {
                    let sender = peer.lock().unwrap().clone();
                    if let Some(sender) = sender {
                        sender.send_datagram(&Msg::Audio {
                            stream: AudioStream::Playback,
                            seq: f.seq,
                            ts_us: f.ts_us,
                            samples: f.bytes,
                        });
                        counters.sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        std::thread::sleep(TICK);
    }
    PumpEnd::Stopped
}

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
}

/// Owns the server's playback backend and the worker that feeds it.
pub struct AudioIn {
    frames: Option<Sender<Frame>>,
    stats: Arc<InStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioIn {
    pub fn spawn(source: PlaybackSource, stats: Arc<InStats>) -> AudioIn {
        let stop = Arc::new(AtomicBool::new(false));
        if matches!(source, PlaybackSource::Disabled) {
            return AudioIn {
                frames: None,
                stats,
                stop,
                thread: None,
            };
        }
        let (tx, rx) = crossbeam_channel::bounded::<Frame>(FRAME_QUEUE);
        let thread = {
            let stop = stop.clone();
            let stats = stats.clone();
            match std::thread::Builder::new()
                .name("pheme-audio-in".into())
                .spawn(move || in_thread(source, rx, stop, stats))
            {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!("could not start the audio playback thread: {e}");
                    None
                }
            }
        };
        AudioIn {
            frames: Some(tx),
            stats,
            stop,
            thread,
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

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.frames = None; // disconnects the worker's receiver
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioIn {
    fn drop(&mut self) {
        self.stop();
    }
}

fn in_thread(
    source: PlaybackSource,
    rx: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    stats: Arc<InStats>,
) {
    let (device, mut injected, rebuild) = match source {
        PlaybackSource::Detect(d) => (d, None, true),
        PlaybackSource::Backend(b) => (None, Some(b), false),
        PlaybackSource::Disabled => return,
    };
    let mut failures = FailureLog::default();
    loop {
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
        match pump_in(backend.as_ref(), producer, &rx, &stop, &stats, rate) {
            PumpEnd::Stopped => {
                backend.stop();
                return;
            }
            PumpEnd::Failed(why) => {
                backend.stop();
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

    while !stop.load(Ordering::SeqCst) {
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
                    let v = (plane[i] * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
                    let _ = producer.push(v);
                }
            }
        }
        publish(stats, jitter.stats());
        std::thread::sleep(TICK);
    }
    PumpEnd::Stopped
}

fn publish(stats: &InStats, s: JitterStats) {
    stats.depth_ms.store(s.depth as u64 * 5, Ordering::Relaxed);
    stats.lost.store(s.lost, Ordering::Relaxed);
    stats.late.store(s.late, Ordering::Relaxed);
    stats.underruns.store(s.underruns, Ordering::Relaxed);
    stats.resets.store(s.resets, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_audio::mock::MockCapture;
    use std::time::Instant;

    fn tone() -> Vec<i16> {
        (0..FRAME_INTERLEAVED).map(|i| (i as i16) - 240).collect()
    }

    fn silence() -> Vec<i16> {
        vec![0; FRAME_INTERLEAVED]
    }

    fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < timeout {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        f()
    }

    #[test]
    fn a_repeating_failure_is_only_loud_once() {
        let mut log = FailureLog::default();
        assert!(log.is_new("audio capture unavailable: no PipeWire"));
        assert!(!log.is_new("audio capture unavailable: no PipeWire"));
        assert!(!log.is_new("audio capture unavailable: no PipeWire"));
        assert!(
            log.is_new("audio capture unavailable: something else"),
            "a different failure is news again"
        );
        log.cleared();
        assert!(
            log.is_new("audio capture unavailable: something else"),
            "a success in between makes the next failure news again"
        );
    }

    #[test]
    fn disabled_audio_starts_no_thread() {
        let counters = Arc::new(OutCounters::default());
        let mut out = AudioOut::spawn(CaptureSource::Disabled, counters.clone());
        out.set_peer(None);
        out.stop();
        assert_eq!(counters.sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_packer_thread_drains_the_ring_with_no_peer_attached() {
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut out = AudioOut::spawn(CaptureSource::Backend(Box::new(cap)), counters.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        // Twenty bursts of 100 frames. The ring holds 200 frames, so anything that does
        // not drain continuously overruns.
        for _ in 0..20 {
            for _ in 0..100 {
                handle.push(&tone());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(handle.overruns(), 0, "the packer thread fell behind");
        out.stop();
    }

    #[test]
    fn a_long_silence_is_counted_as_suppressed() {
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut out = AudioOut::spawn(CaptureSource::Backend(Box::new(cap)), counters.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        for _ in 0..200 {
            handle.push(&silence());
            std::thread::sleep(Duration::from_micros(200));
        }
        assert!(
            wait_until(
                || counters.suppressed.load(Ordering::Relaxed) >= 100,
                Duration::from_secs(2)
            ),
            "expected most of 200 silent frames to be suppressed, got {}",
            counters.suppressed.load(Ordering::Relaxed)
        );
        assert_eq!(
            counters.sent.load(Ordering::Relaxed),
            0,
            "nothing is sent without a peer"
        );
        out.stop();
    }

    #[test]
    fn an_injected_backend_that_fails_to_start_gives_up_quietly() {
        let (cap, handle) = MockCapture::new();
        handle.fail_next_start();
        let counters = Arc::new(OutCounters::default());
        let mut out = AudioOut::spawn(CaptureSource::Backend(Box::new(cap)), counters);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!handle.started());
        out.stop(); // must not hang
    }

    use pheme_audio::mock::{MockPlayback, MockPlaybackHandle};

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
        let mut audio = AudioIn::spawn(PlaybackSource::Disabled, stats.clone());
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
        audio: &AudioIn,
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
    /// `audio_depth_ms` is the depth sampled just after a pop, so a 2-frame target
    /// reads as 5 or 10 rather than a flat 10 — the spec's 10-15 ms names the target,
    /// not the depth — and at 44.1 kHz, where one wire frame resamples to a hair more
    /// than one device period, it sits at 5 and dips to 0 whenever the worker takes two
    /// pops to refill the ring. The median is therefore what the lower bound looks at.
    /// Before the playback mock had a device clock this counter was pinned at 0 for
    /// whole runs, which is what that bound pins down; the maximum catches a buffer
    /// that is quietly filling up instead. The first 50 iterations are skipped while
    /// the buffer prefills.
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
            max <= 15,
            "jitter depth reached {max} ms: the buffer is filling up, so the sender is \
             outrunning the playback worker"
        );
    }

    #[test]
    fn frames_reach_the_playback_device() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = AudioIn::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
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
        let mut audio = AudioIn::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
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
        let mut audio = AudioIn::spawn(PlaybackSource::Backend(Box::new(play)), stats);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!handle.started());
        audio.stop(); // must not hang
    }
}
