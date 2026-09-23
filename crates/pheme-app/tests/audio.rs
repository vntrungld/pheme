//! Audio from the client to the server over a real QUIC connection, with mock devices
//! on both ends.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pheme_app::audio::{CaptureSource, InStats, OutCounters, PlaybackSource};
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
    sent: Arc<OutCounters>,
    heard: Arc<InStats>,
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
    let sent = Arc::new(OutCounters::default());
    let heard = Arc::new(InStats::default());

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
            audio_stats: Some(heard.clone()),
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
            audio_counters: Some(sent.clone()),
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
        sent,
        heard,
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

/// Runs the pipeline for `count` iterations of one 5 ms frame each: the client's device
/// produces `f(i)` and the server's device consumes exactly one period, which is what
/// both of them do in real life.
///
/// Two things matter here. The server's device drains on its own clock — `drain()`
/// would empty the ring however full it is, which lets the playback worker run away
/// from the sender and puts the whole pipeline in permanent underrun, so every test
/// would pass on two frames of audio plus concealment. And the client's device never
/// stops producing, not even through silence: letting it idle while the server's device
/// keeps consuming walks the jitter buffer's read cursor past the sender's sequence
/// numbers, and everything that arrives afterwards is `late`. That is a property of a
/// harness that stops the client's clock, not of the pipeline.
async fn run_frames(pair: &Pair, count: usize, mut f: impl FnMut(usize) -> Vec<i16>) {
    run_frames_watching_depth(pair, count, &mut f).await;
}

/// `run_frames`, returning the `audio_depth_ms` the server published on each iteration.
async fn run_frames_watching_depth(
    pair: &Pair,
    count: usize,
    f: &mut impl FnMut(usize) -> Vec<i16>,
) -> Vec<u64> {
    let mut depths = Vec::with_capacity(count);
    for i in 0..count {
        pair.mic.push(&f(i));
        pair.speaker.drain_frames(1);
        depths.push(pair.heard.depth_ms.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    depths
}

/// The server's device alone, for the tail of a test where the client has stopped.
async fn drain_for(speaker: &MockPlaybackHandle, how_long: Duration) {
    let t = Instant::now();
    while t.elapsed() < how_long {
        speaker.drain_frames(1);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    speaker.drain_frames(1);
}

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

    // 1.5 s of tone at one 5 ms frame per iteration, so the pipeline runs at the rate it
    // would live: the client emits 200 frames a second and the server's device consumes
    // 200 frames a second.
    let depths = run_frames_watching_depth(&pair, 300, &mut sine_frame).await;
    // Read the counters while the stream is still running. After the tone stops the
    // jitter buffer runs dry and conceals, which is correct behaviour but would count
    // as loss and mask a pipeline that was dropping frames all along.
    let (late, underruns, lost) = (
        pair.heard.late.load(Ordering::Relaxed),
        pair.heard.underruns.load(Ordering::Relaxed),
        pair.heard.lost.load(Ordering::Relaxed),
    );
    drain_for(&pair.speaker, Duration::from_millis(300)).await;

    // A coarse guard, deliberately. This test paces on a wall clock, which it has to:
    // lock-stepping it would mean the harness deciding when the client's capture device
    // produces, and that is the thing being tested. On a loaded machine a stall in the
    // client's packer thread backs frames up in its 200-frame capture ring and they
    // arrive in a burst after their slots have passed, which is honest behaviour and
    // has been seen to cost 33 frames in 300. The precise assertions — late, lost and
    // underruns within two of zero — live in `pheme-app/src/audio/recv.rs`'s lock-stepped
    // unit tests. What this budget still catches is the defect it was written for: a
    // playback mock with no device clock let the server discard 95 % of the stream
    // while every assertion in this file passed.
    let budget = 300 / 5;
    assert!(
        late <= budget,
        "{late} frames arrived after their slot had passed"
    );
    assert!(
        underruns <= budget,
        "the playback buffer ran dry {underruns} times"
    );
    assert!(
        lost <= budget,
        "{lost} frames never reached the jitter buffer"
    );

    // `audio_depth_ms` reports the depth *before* each pop, which is the audio waiting
    // to be played, so a 2-frame target reads as a flat 10 ms rather than an alternation
    // of 5 and 10 — matching the spec's 10-15 ms definition of done, which names the
    // target rather than the depth. Before the playback mock had a device clock this
    // counter was pinned at 0 for the whole run. Skip the first 50 iterations, while the
    // buffer is still prefilling, and assert the lower bound only: a scheduling stall
    // parks the buffer deeper and only the drift controller's 0.1 % correction brings it
    // back, which takes about five seconds per frame — far longer than this test runs.
    let mut steady = depths[50..].to_vec();
    steady.sort_unstable();
    let median = steady[steady.len() / 2];
    assert!(
        median >= 5,
        "median jitter depth {median} ms: the buffer is running empty, so the playback \
         worker is outrunning the sender"
    );

    let rec = pair.speaker.recorded();
    let level = tone_level(&rec);
    assert!(
        (level - 10_000).abs() < 1_500,
        "the tone arrived at the wrong level: {level} of an expected 10000"
    );
    // The device consumed one frame per iteration for 300 iterations, so anything much
    // below that means the worker could not keep it fed.
    assert!(
        rec.len() > 280 * FRAME_INTERLEAVED,
        "only {} samples were played, expected about {}",
        rec.len(),
        300 * FRAME_INTERLEAVED
    );
    // Nearly all of the recording is the tone: a 440 Hz sine spends most of its period
    // above a tenth of full scale. A pipeline that delivered a couple of frames and
    // concealed the rest would not reach this.
    assert!(
        loud_samples(&rec) > 100_000,
        "too few loud samples to be the tone: {} of {}",
        loud_samples(&rec),
        rec.len()
    );
    // Every frame the client sent reached the buffer and was played.
    assert!(
        pair.sent.sent.load(Ordering::Relaxed) >= 290,
        "the client only sent {} frames",
        pair.sent.sent.load(Ordering::Relaxed)
    );
    pair.shutdown().await;
}

/// Silence suppression end to end: with the server's device draining on its own clock
/// the jitter buffer tracks the stream, so the client's frame counter standing still
/// through the quiet window is a real observation of "no traffic", not an artefact of a
/// pipeline that was discarding frames either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silence_stops_the_traffic_and_resuming_restores_it() {
    let pair = spawn_pair(false);
    wait_connected(&pair.input_cap).await;
    assert!(wait_until(|| pair.speaker.started(), Duration::from_secs(5)).await);

    // 500 ms of tone.
    run_frames(&pair, 100, sine_frame).await;
    assert!(
        loud_samples(&pair.speaker.recorded()) > 30_000,
        "too few loud samples in the first burst: {}",
        loud_samples(&pair.speaker.recorded())
    );

    // 300 ms of digital silence: past the 200 ms window, so the client is now quiet.
    run_frames(&pair, 60, |_| vec![0i16; FRAME_INTERLEAVED]).await;
    let quiet_start = pair.sent.sent.load(Ordering::Relaxed);

    // A further 500 ms of silence must put nothing at all on the wire.
    run_frames(&pair, 100, |_| vec![0i16; FRAME_INTERLEAVED]).await;
    assert_eq!(
        pair.sent.sent.load(Ordering::Relaxed),
        quiet_start,
        "the client kept sending through the silence window"
    );
    assert!(
        pair.sent.suppressed.load(Ordering::Relaxed) >= 100,
        "only {} frames were suppressed",
        pair.sent.suppressed.load(Ordering::Relaxed)
    );
    assert_eq!(
        pair.heard.depth_ms.load(Ordering::Relaxed),
        0,
        "the server's jitter buffer still holds frames, so audio was still arriving"
    );

    // Resuming must produce audible output again.
    let before_resume = pair.speaker.recorded().len();
    run_frames(&pair, 100, |i| sine_frame(i + 500)).await;
    // A short silent tail so the last frames in flight reach the device.
    run_frames(&pair, 20, |_| vec![0i16; FRAME_INTERLEAVED]).await;

    assert!(
        pair.sent.sent.load(Ordering::Relaxed) > quiet_start + 90,
        "the client sent only {} frames after resuming",
        pair.sent.sent.load(Ordering::Relaxed) - quiet_start
    );
    let rec = pair.speaker.recorded();
    let resumed = &rec[before_resume.min(rec.len())..];
    let level = tone_level(resumed);
    assert!(
        (level - 10_000).abs() < 1_500,
        "audio did not come back after the pause: level {level}"
    );
    assert!(
        loud_samples(resumed) > 30_000,
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
