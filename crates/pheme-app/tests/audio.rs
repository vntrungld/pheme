//! Audio from the client to the server over a real QUIC connection, with mock devices
//! on both ends.

use std::time::{Duration, Instant};

use pheme_app::audio::{CaptureSource, PlaybackSource};
use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_audio::mock::{MockCapture, MockCaptureHandle, MockPlayback, MockPlaybackHandle};
use pheme_audio::FRAME_INTERLEAVED;
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{
    InjectCall, MockCapture as MockInputCapture, MockCaptureHandle as MockInputHandle, MockInject,
    MockInjectLog,
};
use pheme_input::CaptureMode;
use pheme_net::{Endpoint, Identity, TrustStore};
use pheme_proto::{KeyCode, ScreenInfo};
use tokio::sync::watch;

fn screens(w: u32, h: u32) -> Vec<ScreenInfo> {
    vec![ScreenInfo {
        x: 0,
        y: 0,
        w,
        h,
        primary: true,
    }]
}

async fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f()
}

/// One frame of a 440 Hz sine at about a third of full scale, continuing from frame `i`.
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

struct Pair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    input_cap: MockInputHandle,
    inj: MockInjectLog,
    mic: MockCaptureHandle,
    speaker: MockPlaybackHandle,
}

impl Pair {
    /// Matches the join pattern in `tests/integration.rs`: a panic or an error returned
    /// by `run_client`/`run_server` during teardown must fail the test, not vanish.
    async fn shutdown(self) {
        self.shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

/// A paired server and client on loopback, with mock input *and* mock audio devices.
/// `fail_capture` makes the client's audio capture backend refuse to start.
fn spawn_pair(fail_capture: bool) -> Pair {
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let sid = Identity::load_or_create(sdir.path(), "server").unwrap();
    let cid = Identity::load_or_create(cdir.path(), "lap").unwrap();
    let strust = TrustStore::load(sdir.path()).unwrap().shared();
    let ctrust = TrustStore::load(cdir.path()).unwrap().shared();
    strust.write().unwrap().add("lap", &cid.fingerprint);
    ctrust.write().unwrap().add("server", &sid.fingerprint);

    let server_ep = Endpoint::server("127.0.0.1:0".parse().unwrap(), &sid, strust).unwrap();
    let server_addr = server_ep.local_addr().unwrap();
    let client_ep = Endpoint::client(&cid, ctrust).unwrap();

    let (input_capture, input_cap) = MockInputCapture::new(screens(1920, 1080));
    let (inject, inj) = MockInject::new(screens(1000, 500));
    let (mic_backend, mic) = MockCapture::new();
    if fail_capture {
        mic.fail_next_start();
    }
    let (speaker_backend, speaker) = MockPlayback::new(48_000);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(run_server(
        ServerDeps {
            name: "server".into(),
            capture: Box::new(input_capture),
            endpoint: server_ep,
            placements: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
            hotkeys: Hotkeys::default(),
            stats: false,
            audio: PlaybackSource::Backend(Box::new(speaker_backend)),
        },
        shutdown_rx.clone(),
    ));
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            server_addr,
            stats: false,
            audio: CaptureSource::Backend(Box::new(mic_backend)),
        },
        shutdown_rx,
    ));
    Pair {
        server,
        client,
        shutdown_tx,
        input_cap,
        inj,
        mic,
        speaker,
    }
}

fn push_edge_crossing(cap: &MockInputHandle) {
    cap.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
    cap.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
}

/// Waits until one edge crossing is accepted, which proves the session is up.
async fn wait_connected(cap: &MockInputHandle) {
    assert!(
        wait_until(|| cap.is_started(), Duration::from_secs(5)).await,
        "input capture never started"
    );
    assert!(
        wait_until(
            || {
                push_edge_crossing(cap);
                cap.mode() == CaptureMode::Grab
            },
            Duration::from_secs(5),
        )
        .await,
        "client never connected"
    );
}

/// The device callback: pulls whatever the worker has produced.
async fn drain_for(speaker: &MockPlaybackHandle, how_long: Duration) {
    let t = Instant::now();
    while t.elapsed() < how_long {
        speaker.drain();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    speaker.drain();
}

/// How many samples in `rec` clear a "clearly not silence" threshold. A peak-only check
/// would be satisfied by silence plus a single stray sample, so the tests also check
/// that a substantial run of samples carries the tone, not just its highest point.
fn loud_samples(rec: &[i16]) -> usize {
    rec.iter().filter(|s| s.unsigned_abs() > 1_000).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_flows_client_to_server() {
    let pair = spawn_pair(false);
    wait_connected(&pair.input_cap).await;
    assert!(
        wait_until(|| pair.speaker.started(), Duration::from_secs(5)).await,
        "the server's playback device never opened"
    );

    // 300 ms of tone, fed at roughly real time so the pipeline behaves as it would live.
    for i in 0..60 {
        pair.mic.push(&sine_frame(i));
        pair.speaker.drain();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drain_for(&pair.speaker, Duration::from_millis(300)).await;

    let rec = pair.speaker.recorded();
    let peak = rec.iter().map(|s| i32::from(s.abs())).max().unwrap_or(0);
    assert!(
        (peak - 10_000).abs() < 1_500,
        "the tone arrived at the wrong level: peak {peak} of an expected 10000"
    );
    assert!(
        rec.len() > 30 * FRAME_INTERLEAVED,
        "only {} samples were played",
        rec.len()
    );
    // Well beyond one stray sample: the recording also includes the warm-up and
    // trailing silence either side of the tone, so this is a fraction of `rec.len()`,
    // not most of it.
    assert!(
        loud_samples(&rec) > 1_000,
        "too few loud samples to be the tone: {} of {}",
        loud_samples(&rec),
        rec.len()
    );
    pair.shutdown().await;
}

/// This test covers resumption only, not suppression itself: an all-zero frame
/// resamples to sub-threshold output whether it was transmitted or suppressed, so a
/// build with silence suppression disabled entirely would pass this test unchanged.
/// Suppression is already pinned by `pack.rs`'s `a_long_silence_is_suppressed` and by
/// `pheme-app/src/audio.rs`'s `a_long_silence_is_counted_as_suppressed`, which exercises
/// the real `AudioOut` pump and asserts nothing is sent. An end-to-end assertion here
/// would need a transport-level counter (`OutCounters`) that `ClientDeps` deliberately
/// does not expose; the manual test matrix's row A5 covers this on real hardware via
/// `--stats`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_resumes_after_a_long_silence() {
    let pair = spawn_pair(false);
    wait_connected(&pair.input_cap).await;
    assert!(wait_until(|| pair.speaker.started(), Duration::from_secs(5)).await);

    for i in 0..20 {
        pair.mic.push(&sine_frame(i));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drain_for(&pair.speaker, Duration::from_millis(100)).await;
    let after_tone = pair.speaker.recorded().len();
    assert!(
        loud_samples(&pair.speaker.recorded()) > 3_000,
        "too few loud samples in the first burst"
    );

    // 400 ms of digital silence: past the 200 ms suppression window.
    for _ in 0..80 {
        pair.mic.push(&vec![0i16; FRAME_INTERLEAVED]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drain_for(&pair.speaker, Duration::from_millis(200)).await;

    // Resuming must produce audible output again.
    let before_resume = pair.speaker.recorded().len();
    for i in 100..140 {
        pair.mic.push(&sine_frame(i));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drain_for(&pair.speaker, Duration::from_millis(300)).await;

    let rec = pair.speaker.recorded();
    assert!(after_tone > 0, "no audio before the silence");
    let resumed = &rec[before_resume.min(rec.len())..];
    let peak = resumed
        .iter()
        .map(|s| i32::from(s.abs()))
        .max()
        .unwrap_or(0);
    assert!(
        (peak - 10_000).abs() < 1_500,
        "audio did not come back after the pause: peak {peak}"
    );
    assert!(
        loud_samples(resumed) > 5_000,
        "too few loud samples after resuming: {} of {}",
        loud_samples(resumed),
        resumed.len()
    );
    pair.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_failure_does_not_break_kvm() {
    let pair = spawn_pair(true);
    wait_connected(&pair.input_cap).await;
    assert!(!pair.mic.started(), "the capture backend was meant to fail");

    // Keyboard and mouse must be completely unaffected.
    pair.input_cap.push(CaptureEvent::Key {
        code: KeyCode(0x04),
        down: true,
    });
    assert!(
        wait_until(
            || pair
                .inj
                .calls()
                .iter()
                .any(|c| matches!(c, InjectCall::Key(code, true) if *code == KeyCode(0x04))),
            Duration::from_secs(5),
        )
        .await,
        "the key never reached the client"
    );
    pair.shutdown().await;
}
