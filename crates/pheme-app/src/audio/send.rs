use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pheme_audio::pack::Packer;
use pheme_audio::{AudioCapture, FRAME_INTERLEAVED, FRAME_US};
use pheme_net::PeerSender;
use pheme_proto::{AudioStream, Msg};
use tracing::{info, warn};

use super::{nap, FailureLog, PumpEnd, GATE_POLL, RETRY, TICK};

/// Capture ring, in frames. One second of audio is plenty of slack for a thread that
/// wakes every 2 ms.
const CAPTURE_RING_FRAMES: usize = 200;

/// Where the capture backend comes from. Tests inject their own; production detects one.
pub enum CaptureSource {
    /// Detect the platform backend, optionally naming a device.
    Detect(Option<String>),
    /// Use this backend. It is started and stopped as often as the gate asks, which is
    /// what lets a test drive `set_wanted`.
    Backend(Box<dyn AudioCapture>),
    /// Run no audio at all.
    Disabled,
}

/// Per-tag labels so two `SendSide`s in the same process log distinguishably: once each
/// role runs both directions, a shared thread name and a shared failure wording make a
/// failing microphone indistinguishable in the log from a failing speaker capture.
trait StreamLabel {
    /// The name given to this side's thread.
    fn thread_name(&self) -> &'static str;
    /// What this side's failure messages call the device it opens.
    fn subject(&self) -> &'static str;
}

impl StreamLabel for AudioStream {
    fn thread_name(&self) -> &'static str {
        match self {
            AudioStream::Playback => "pheme-audio-out",
            AudioStream::Mic => "pheme-mic-out",
        }
    }

    fn subject(&self) -> &'static str {
        match self {
            AudioStream::Playback => "audio capture",
            AudioStream::Mic => "microphone",
        }
    }
}

#[derive(Default)]
pub struct OutCounters {
    /// Frames handed to the transport.
    pub sent: AtomicU64,
    /// Frames dropped by silence suppression.
    pub suppressed: AtomicU64,
}

/// Owns a capture backend and the thread that packs and sends its frames.
pub struct SendSide {
    peer: Arc<Mutex<Option<PeerSender>>>,
    stop: Arc<AtomicBool>,
    wanted: Arc<AtomicBool>,
    /// True only while a backend is actually started. False both while the gate is shut
    /// and while a wanted device has failed to open, which is what lets `is_open`
    /// distinguish "closed because nothing is recording" from "closed because it
    /// failed" from outside.
    open: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SendSide {
    /// `stream` tags every datagram this side sends: `Playback` on a client, `Mic` on a
    /// server. It is the only thing that distinguishes the two directions here.
    ///
    /// `wanted` is the gate's initial state, applied before the worker thread exists.
    ///
    /// It is a parameter rather than something the caller corrects afterwards because
    /// there is no happens-before relationship between a spawned thread's first read and
    /// a store the parent makes after `spawn` returns: a side created open and closed a
    /// moment later can still have opened its device, which for a microphone means its
    /// indicator lights at every server start. The client's speaker capture passes true;
    /// the server's microphone passes false and waits to be asked.
    pub fn spawn(
        source: CaptureSource,
        stream: AudioStream,
        counters: Arc<OutCounters>,
        wanted: bool,
    ) -> SendSide {
        let peer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let wanted = Arc::new(AtomicBool::new(wanted));
        // No backend has started yet, whatever the gate says: a store made here is not
        // guaranteed to be seen before the worker thread's first read, so this can only
        // ever be false at birth.
        let open = Arc::new(AtomicBool::new(false));
        let thread = match source {
            CaptureSource::Disabled => None,
            src => {
                let peer = peer.clone();
                let stop = stop.clone();
                let wanted = wanted.clone();
                let open = open.clone();
                match std::thread::Builder::new()
                    .name(stream.thread_name().into())
                    .spawn(move || out_thread(src, stream, peer, stop, counters, wanted, open))
                {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warn!("could not start the audio thread: {e}");
                        None
                    }
                }
            }
        };
        SendSide {
            peer,
            stop,
            wanted,
            open,
            thread,
        }
    }

    /// Attaches the session's transport, or detaches it when the session ends. While no
    /// peer is attached the thread keeps draining the device and simply discards frames.
    pub fn set_peer(&self, sender: Option<PeerSender>) {
        *self.peer.lock().unwrap() = sender;
    }

    /// Opens or closes the capture device.
    ///
    /// This stops the device itself rather than merely ceasing to send, so the operating
    /// system reports the microphone as closed and its indicator goes out. A side nobody
    /// calls this on stays open, which is what the client's speaker capture wants.
    pub fn set_wanted(&self, wanted: bool) {
        self.wanted.store(wanted, Ordering::SeqCst);
    }

    /// Whether a capture backend is actually running right now. This is the demand gate
    /// made visible from outside: it reads false both while the gate is shut and while a
    /// wanted device has failed to open, so a caller cannot mistake one for the other.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SendSide {
    fn drop(&mut self) {
        self.stop();
    }
}

fn out_thread(
    source: CaptureSource,
    stream: AudioStream,
    peer: Arc<Mutex<Option<PeerSender>>>,
    stop: Arc<AtomicBool>,
    counters: Arc<OutCounters>,
    wanted: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
) {
    let (device, mut injected) = match source {
        CaptureSource::Detect(d) => (d, None),
        CaptureSource::Backend(b) => (None, Some(b)),
        CaptureSource::Disabled => return,
    };
    let mut failures = FailureLog::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if !wanted.load(Ordering::SeqCst) {
            open.store(false, Ordering::SeqCst);
            if !nap(GATE_POLL, &stop) {
                return;
            }
            continue;
        }
        // The injected backend is reused across every reopen — it is a test double, and
        // the gate may start and stop it any number of times over a session. A detected
        // backend is rebuilt fresh each time instead: a real device that failed to open
        // is not trusted to succeed if merely retried on the same instance.
        let mut fresh: Option<Box<dyn AudioCapture>>;
        let backend: &mut dyn AudioCapture = match injected.as_deref_mut() {
            Some(b) => b,
            None => match match stream {
                AudioStream::Playback => pheme_audio::detect_capture(device.as_deref()),
                AudioStream::Mic => pheme_audio::detect_mic(device.as_deref()),
            } {
                Ok(b) => {
                    fresh = Some(b);
                    fresh.as_deref_mut().expect("just assigned")
                }
                Err(pheme_audio::Error::Unsupported(why)) => {
                    info!("audio capture is not available here: {why}");
                    return;
                }
                Err(e) => {
                    failures.report(format!("{} unavailable: {e}", stream.subject()));
                    if !nap(RETRY, &stop) {
                        return;
                    }
                    continue;
                }
            },
        };
        let (producer, consumer) =
            rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * CAPTURE_RING_FRAMES);
        if let Err(e) = backend.start(producer) {
            failures.report(format!("{} failed to start: {e}", stream.subject()));
            open.store(false, Ordering::SeqCst);
            if !nap(RETRY, &stop) {
                return;
            }
            continue;
        }
        failures.cleared();
        info!(device = %backend.device_name(), "audio capture started");
        open.store(true, Ordering::SeqCst);
        let end = pump_out(backend, stream, consumer, &peer, &stop, &counters, &wanted);
        backend.stop();
        open.store(false, Ordering::SeqCst);
        match end {
            PumpEnd::Stopped => return,
            // The gate closed. Go straight back to the top: reopening must be prompt,
            // and a closed gate is not a failure to back off from.
            PumpEnd::Unwanted => continue,
            PumpEnd::Failed(why) => {
                failures.report(format!("{} stopped: {why}", stream.subject()));
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if !nap(RETRY, &stop) {
            return;
        }
    }
}

/// Packs whole frames out of the ring and sends them while a peer is attached. Returns
/// when the gate closes, the backend dies, or a stop is requested.
fn pump_out(
    backend: &dyn AudioCapture,
    stream: AudioStream,
    mut consumer: rtrb::Consumer<i16>,
    peer: &Mutex<Option<PeerSender>>,
    stop: &AtomicBool,
    counters: &OutCounters,
    wanted: &AtomicBool,
) -> PumpEnd {
    let mut packer = Packer::new();
    let mut frame = Vec::with_capacity(FRAME_INTERLEAVED);
    let mut taken = 0u64;
    while !stop.load(Ordering::SeqCst) {
        if !wanted.load(Ordering::SeqCst) {
            return PumpEnd::Unwanted;
        }
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
                            stream,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{wait_until, SETTLE};
    use pheme_audio::mock::MockCapture;
    use std::time::Duration;

    fn tone() -> Vec<i16> {
        (0..FRAME_INTERLEAVED).map(|i| (i as i16) - 240).collect()
    }

    fn silence() -> Vec<i16> {
        vec![0; FRAME_INTERLEAVED]
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
        let mut out = SendSide::spawn(
            CaptureSource::Disabled,
            AudioStream::Playback,
            counters.clone(),
            true,
        );
        out.set_peer(None);
        out.stop();
        assert_eq!(counters.sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_packer_thread_drains_the_ring_with_no_peer_attached() {
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut out = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters.clone(),
            true,
        );
        assert!(wait_until(|| handle.started(), SETTLE));

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
        let mut out = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters.clone(),
            true,
        );
        assert!(wait_until(|| handle.started(), SETTLE));

        for _ in 0..200 {
            handle.push(&silence());
            std::thread::sleep(Duration::from_micros(200));
        }
        assert!(
            wait_until(
                || counters.suppressed.load(Ordering::Relaxed) >= 100,
                SETTLE
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
        let mut out = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters,
            true,
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!handle.started());
        out.stop(); // must not hang
    }

    #[test]
    fn is_open_reflects_the_device_not_the_gates_intent() {
        // A gate that is open while the device is failing to start must not read as
        // open: `mic_open` in the stats line exists to tell "closed because nothing is
        // recording" apart from "closed because it failed", and a flag that just echoed
        // the gate would collapse that distinction.
        let (cap, handle) = MockCapture::new();
        handle.fail_next_start();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
            true,
        );
        assert!(!audio.is_open(), "the gate is open but the device is not");
        assert!(
            wait_until(|| handle.started(), SETTLE),
            "the retry cycle must bring the microphone up"
        );
        assert!(wait_until(|| audio.is_open(), SETTLE));

        audio.set_wanted(false);
        assert!(wait_until(|| !audio.is_open(), SETTLE));
        audio.stop();
    }

    #[test]
    fn a_side_that_is_not_wanted_closes_its_device() {
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
            true,
        );
        assert!(wait_until(|| handle.started(), SETTLE));

        audio.set_wanted(false);
        assert!(
            wait_until(|| !handle.started(), SETTLE),
            "the device must actually close, not merely stop sending: an open microphone \
             keeps its indicator lit"
        );

        audio.set_wanted(true);
        assert!(wait_until(|| handle.started(), SETTLE));
        assert_eq!(handle.start_count(), 2, "it was really reopened");
        audio.stop();
    }

    #[test]
    fn a_side_that_starts_closed_does_not_open_its_device() {
        // The server's microphone must not be opened before a client asks for it — not
        // even briefly. `MockCaptureHandle::start_count` is what makes "not even briefly"
        // observable; a side that was created open and closed a moment later would show
        // a count of one here.
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
            false,
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            handle.start_count(),
            0,
            "a side that starts closed must never have opened its device"
        );
        assert!(!handle.started());

        audio.set_wanted(true);
        assert!(wait_until(|| handle.started(), SETTLE));
        assert_eq!(handle.start_count(), 1);
        audio.stop();
    }

    #[test]
    fn a_side_nobody_gates_stays_open() {
        // The client's speaker capture is never gated; it must behave exactly as before.
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters,
            true,
        );
        assert!(wait_until(|| handle.started(), SETTLE));
        std::thread::sleep(Duration::from_millis(50));
        assert!(handle.started());
        assert_eq!(handle.start_count(), 1);
        audio.stop();
    }

    #[test]
    fn flipping_the_gate_faster_than_the_device_can_follow_settles_correctly() {
        // Review Focus 2. An application that opens and closes a recording device in a
        // burst must leave the gate and the device agreeing, with no wedged state and
        // no thread left behind.
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
            true,
        );
        assert!(wait_until(|| handle.started(), SETTLE));
        for _ in 0..20 {
            audio.set_wanted(false);
            audio.set_wanted(true);
        }
        assert!(
            wait_until(|| handle.started(), SETTLE),
            "the gate ended on `true`, so the device must end open"
        );
        audio.set_wanted(false);
        assert!(
            wait_until(|| !handle.started(), SETTLE),
            "the gate ended on `false`, so the device must end closed"
        );
        audio.stop();
    }

    #[test]
    fn a_wanted_side_whose_device_will_not_open_keeps_retrying_quietly() {
        // Review Focus 4. The gate says open and the device says no; the retry cycle must
        // be the ordinary one, and the device must come up on its own once it can.
        let (cap, handle) = MockCapture::new();
        handle.fail_next_start();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
            true,
        );
        assert!(
            wait_until(|| handle.started(), SETTLE),
            "the retry cycle must bring the microphone up after a failed start"
        );
        audio.stop();
    }

    #[test]
    fn the_two_streams_get_different_labels() {
        // Authorised addition beyond the brief: once each role runs two `SendSide`s, a
        // shared thread name and a shared failure wording make a failing microphone
        // indistinguishable in the log from a failing speaker capture.
        assert_ne!(
            AudioStream::Mic.thread_name(),
            AudioStream::Playback.thread_name(),
            "two SendSides sharing a thread name would be indistinguishable in the log"
        );
        assert_ne!(
            AudioStream::Mic.subject(),
            AudioStream::Playback.subject(),
            "a failing microphone must not read like a failing speaker capture"
        );
    }
}
