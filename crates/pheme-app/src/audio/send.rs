use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pheme_audio::pack::Packer;
use pheme_audio::{AudioCapture, FRAME_INTERLEAVED, FRAME_US};
use pheme_net::PeerSender;
use pheme_proto::{AudioStream, Msg};
use tracing::{info, warn};

use super::{nap, FailureLog, PumpEnd, RETRY, TICK};

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

/// Owns a capture backend and the thread that packs and sends its frames.
pub struct SendSide {
    peer: Arc<Mutex<Option<PeerSender>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SendSide {
    /// `stream` tags every datagram this side sends: `Playback` on a client, `Mic` on a
    /// server. It is the only thing that distinguishes the two directions here.
    pub fn spawn(
        source: CaptureSource,
        stream: AudioStream,
        counters: Arc<OutCounters>,
    ) -> SendSide {
        let peer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = match source {
            CaptureSource::Disabled => None,
            src => {
                let peer = peer.clone();
                let stop = stop.clone();
                match std::thread::Builder::new()
                    .name("pheme-audio-out".into())
                    .spawn(move || out_thread(src, stream, peer, stop, counters))
                {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warn!("could not start the audio thread: {e}");
                        None
                    }
                }
            }
        };
        SendSide { peer, stop, thread }
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
        let end = pump_out(backend.as_ref(), stream, consumer, &peer, &stop, &counters);
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

/// Packs whole frames out of the ring and sends them while a peer is attached. Returns
/// when the backend dies or a stop is requested.
fn pump_out(
    backend: &dyn AudioCapture,
    stream: AudioStream,
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
    use crate::audio::wait_until;
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
        );
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
        let mut out = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters.clone(),
        );
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
        let mut out = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters,
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!handle.started());
        out.stop(); // must not hang
    }
}
