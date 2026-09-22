//! Audio wiring between the backends in `pheme-audio` and the QUIC session.
//!
//! Each role runs one supervisor thread that owns its backend and rebuilds it every five
//! seconds for as long as it is failing. Audio never affects the keyboard and mouse
//! session: nothing here can make `run_client` or `run_server` return an error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pheme_audio::pack::Packer;
use pheme_audio::{AudioCapture, FRAME_INTERLEAVED, FRAME_US};
use pheme_net::PeerSender;
use pheme_proto::{AudioStream, Msg};
use tracing::{info, warn};

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
                warn!("audio capture unavailable: {e}");
                if !rebuild || !nap(RETRY, &stop) {
                    return;
                }
                continue;
            }
        };
        let (producer, consumer) =
            rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * CAPTURE_RING_FRAMES);
        if let Err(e) = backend.start(producer) {
            warn!("audio capture failed to start: {e}");
            if !rebuild || !nap(RETRY, &stop) {
                return;
            }
            continue;
        }
        info!(device = %backend.device_name(), "audio capture started");
        pump_out(backend.as_ref(), consumer, &peer, &stop, &counters);
        backend.stop();
        if stop.load(Ordering::SeqCst) || !rebuild {
            return;
        }
        warn!("audio capture stopped; rebuilding in 5 s");
        if !nap(RETRY, &stop) {
            return;
        }
    }
}

/// Packs whole frames out of the ring and sends them while a peer is attached. Returns
/// when the backend dies or a stop is requested.
fn pump_out(
    backend: &dyn AudioCapture,
    mut consumer: rtrb::Consumer<i16>,
    peer: &Mutex<Option<PeerSender>>,
    stop: &AtomicBool,
    counters: &OutCounters,
) {
    let mut packer = Packer::new();
    let mut frame = Vec::with_capacity(FRAME_INTERLEAVED);
    let mut taken = 0u64;
    while !stop.load(Ordering::SeqCst) {
        if !backend.healthy() {
            warn!("the audio capture device stopped");
            return;
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
}
