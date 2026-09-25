//! The server's microphone reaching the client's virtual microphone over a real QUIC
//! connection, with mock devices on both ends.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pheme_app::audio::{CaptureSource, InStats, PlaybackSource};
use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_app::target::Target;
use pheme_audio::mock::{MockCapture, MockCaptureHandle, MockPlayback, MockPlaybackHandle};
use pheme_audio::{Demand, FRAME_INTERLEAVED};
use pheme_core::{ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{MockCapture as MockInputCapture, MockInject};
use pheme_input::InputCapture;
use pheme_net::{Endpoint, Identity, Incoming, SharedTrust, TrustStore};
use pheme_proto::{AudioParams, Msg, ScreenInfo, PROTOCOL_VERSION};
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

/// Binds a server endpoint to `addr`, retrying until the socket is free.
///
/// Awaiting the previous server's task is not the same as the operating system having
/// released its socket: quinn's endpoint driver can hold it for a moment after
/// `run_server` returns, and a rebind that loses that race fails with `AddrInUse`. The
/// test is asserting that a client reconnects to a restarted server, not that sockets are
/// released instantly, so waiting for the port is part of restarting the server.
async fn bind_server_retrying(addr: SocketAddr, id: &Identity, trust: SharedTrust) -> Endpoint {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Endpoint::server(addr, id, trust.clone()) {
            Ok(ep) => return ep,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("could not rebind {addr} within 10 s: {e}"),
        }
    }
}

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

struct MicPair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    /// Ends only the *current* server instance, independently of `shutdown_tx`. Used by
    /// `drop_client_connection` to break the link without touching the client task.
    server_kill: watch::Sender<bool>,
    /// Ends only the client task, independently of `shutdown_tx`. Used by
    /// `a_disconnect_closes_the_server_microphone`: firing the shared `shutdown_tx`
    /// there would also start the server's own teardown, and `Shared`'s drop stops the
    /// mic capture backend as a side effect of *that*, which would satisfy the test's
    /// assertion even if the disconnect path's `shared.mic.set_wanted(false)` were
    /// deleted. `client_kill` ends only the client, so the mic can only close because
    /// the server noticed the client leave.
    client_kill: watch::Sender<bool>,
    server_addr: SocketAddr,
    server_identity: Identity,
    server_trust: SharedTrust,
    /// The server's microphone device.
    server_mic: MockCaptureHandle,
    /// The client's virtual microphone: what applications would record from.
    client_mic: MockPlaybackHandle,
    heard: Arc<InStats>,
}

impl MicPair {
    /// Matches the join pattern in `tests/audio.rs`: a panic or an error returned by
    /// `run_client`/`run_server` during teardown must fail the test, not vanish.
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

    /// Ends the *current* server instance and replaces it with a fresh one listening on
    /// the same address, without ever touching the client task. Returns the new
    /// instance's microphone handle.
    ///
    /// This is deliberately a server-side bounce, not a client-side one, and that is not
    /// a shortcut: it is the only form that still pins the property under test. The
    /// client's `RecvSide` (and the `watch::Receiver` for mic demand it hands to every
    /// session) is created once in `run_client` and lives for the task's whole life;
    /// bouncing only the server leaves that receiver exactly as it was mid-recording, so
    /// the reconnecting session clones a receiver that already sees `Wanted` and nothing
    /// changes on it. Rebuilding the *client* task instead would rebuild that receiver
    /// too, so a fresh subscriber would see the `Idle` -> `Wanted` edge from the demand
    /// poller's very first tick and resend on its own, masking the bug this test exists
    /// to catch.
    ///
    /// The old server is joined before the new one binds, mirroring
    /// `client_reconnects_after_server_restart` in `tests/integration.rs`: the OS only
    /// frees the port once the previous `quinn::Endpoint` (and its UDP socket) is
    /// actually dropped.
    async fn drop_client_connection(&mut self) -> MockCaptureHandle {
        self.server_kill.send(true).unwrap();
        let old = std::mem::replace(&mut self.server, tokio::spawn(std::future::ready(Ok(()))));
        tokio::time::timeout(Duration::from_secs(5), old)
            .await
            .expect("the old server instance did not stop")
            .unwrap()
            .unwrap();

        let server_ep = bind_server_retrying(
            self.server_addr,
            &self.server_identity,
            self.server_trust.clone(),
        )
        .await;
        let (input_capture, _input_cap) = MockInputCapture::new(screens(1920, 1080));
        let (mic_backend, mic) = MockCapture::new();
        let (server, kill) = spawn_server_instance(
            server_ep,
            Box::new(input_capture),
            mic_backend,
            &self.shutdown_tx,
        );
        self.server = server;
        self.server_kill = kill;
        self.server_mic = mic.clone();
        mic
    }
}

/// Wraps `shutdown_tx` (the pair's shared, final teardown signal) with a private
/// per-instance kill switch: the returned receiver fires when either one does, so a
/// caller can end just the one task built from it without ending everything else the
/// pair owns. Used for both the client and each server instance, so
/// `a_disconnect_closes_the_server_microphone` can end only the client and
/// `drop_client_connection` can end only the current server, each independently of the
/// other and of the pair's own final teardown.
fn relay(shutdown_tx: &watch::Sender<bool>) -> (watch::Sender<bool>, watch::Receiver<bool>) {
    let (kill_tx, mut kill_rx) = watch::channel(false);
    let (run_tx, run_rx) = watch::channel(false);
    let mut master_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        tokio::select! {
            _ = master_rx.changed() => {}
            _ = kill_rx.changed() => {}
        }
        let _ = run_tx.send(true);
    });
    (kill_tx, run_rx)
}

/// Spawns one server instance whose lifetime ends when either `shutdown_tx` (the pair's
/// final teardown signal, shared with the client) or the returned per-instance sender
/// fires. The latter is what `drop_client_connection` uses to end just this instance.
fn spawn_server_instance(
    endpoint: Endpoint,
    capture: Box<dyn InputCapture>,
    mic_backend: MockCapture,
    shutdown_tx: &watch::Sender<bool>,
) -> (
    tokio::task::JoinHandle<anyhow::Result<()>>,
    watch::Sender<bool>,
) {
    let (kill_tx, run_rx) = relay(shutdown_tx);
    let server = tokio::spawn(run_server(
        ServerDeps {
            name: "server".into(),
            capture,
            endpoint,
            placements: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
            hotkeys: Hotkeys::default(),
            lock_hotkey_trigger: None,
            stats: false,
            audio: PlaybackSource::Disabled,
            audio_stats: None,
            mic: CaptureSource::Backend(Box::new(mic_backend)),
            mic_counters: None,
            clipboard: None,
        },
        run_rx,
    ));
    (server, kill_tx)
}

/// A paired server and client on loopback, with the mic direction wired to mocks. The
/// playback direction is disabled on both sides so nothing competes for the mock
/// devices, and so a failure names the direction under test. `demand` is what the
/// client's virtual microphone reports about its consumers. `virtual_mic` false models a
/// client with no virtual microphone at all — a Windows client today.
fn spawn_mic_pair(demand: Demand, virtual_mic: bool) -> MicPair {
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let sid = Identity::load_or_create(sdir.path(), "server").unwrap();
    let cid = Identity::load_or_create(cdir.path(), "lap").unwrap();
    let strust = TrustStore::load(sdir.path()).unwrap().shared();
    let ctrust = TrustStore::load(cdir.path()).unwrap().shared();
    strust.write().unwrap().add("lap", &cid.fingerprint);
    ctrust.write().unwrap().add("server", &sid.fingerprint);

    let server_ep = Endpoint::server("127.0.0.1:0".parse().unwrap(), &sid, strust.clone()).unwrap();
    let server_addr = server_ep.local_addr().unwrap();
    let client_ep = Endpoint::client(&cid, ctrust).unwrap();

    let (input_capture, _input_cap) = MockInputCapture::new(screens(1920, 1080));
    let (inject, _inj) = MockInject::new(screens(1000, 500));

    let (server_mic_backend, server_mic) = MockCapture::new();
    let (client_mic_backend, client_mic) = MockPlayback::new(48_000);
    client_mic.set_demand(demand);
    let heard = Arc::new(InStats::default());

    let (shutdown_tx, _) = watch::channel(false);
    let (server, server_kill) = spawn_server_instance(
        server_ep,
        Box::new(input_capture),
        server_mic_backend,
        &shutdown_tx,
    );
    let (client_kill, client_run_rx) = relay(&shutdown_tx);
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Fixed(server_addr),
            stats: false,
            audio: CaptureSource::Disabled,
            audio_counters: None,
            mic: if virtual_mic {
                PlaybackSource::Backend(Box::new(client_mic_backend))
            } else {
                PlaybackSource::Disabled
            },
            mic_stats: Some(heard.clone()),
            clipboard: None,
        },
        client_run_rx,
    ));

    MicPair {
        server,
        client,
        shutdown_tx,
        server_kill,
        client_kill,
        server_addr,
        server_identity: sid,
        server_trust: strust,
        server_mic,
        client_mic,
        heard,
    }
}

#[tokio::test]
async fn the_server_microphone_reaches_the_client_when_something_records() {
    let pair = spawn_mic_pair(Demand::Wanted, true);
    assert!(
        wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await,
        "a recording consumer must open the server's microphone"
    );

    // Drive both clocks: the server's microphone produces a frame, the client's device
    // consumes one. Without the consumer side the jitter buffer discards nearly
    // everything and the test passes while the audio is broken.
    for i in 0..300 {
        pair.server_mic.push(&sine_frame(i));
        pair.client_mic.drain_frames(1);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let rec = pair.client_mic.recorded();
    assert!(
        rec.len() > 280 * FRAME_INTERLEAVED,
        "only {} samples reached the virtual microphone",
        rec.len()
    );
    let peak = rec.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
    assert!(
        (8_000..=12_000).contains(&peak),
        "the tone arrived at level {peak}, expected about 10000"
    );
    assert_eq!(
        pair.heard.underruns.load(Ordering::Relaxed),
        0,
        "the virtual microphone ran dry"
    );
    pair.shutdown().await;
}

#[tokio::test]
async fn the_server_microphone_stays_shut_while_nothing_records() {
    let pair = spawn_mic_pair(Demand::Idle, true);
    // Give the session time to hand shake and settle; the gate must never open.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !pair.server_mic.started(),
        "nothing is recording, so the microphone must stay closed and its light out"
    );
    assert_eq!(pair.server_mic.start_count(), 0);
    pair.shutdown().await;
}

#[tokio::test]
async fn a_client_with_no_virtual_microphone_never_opens_the_server_one() {
    // A Windows client today: detect_virtual_mic returns Unsupported, so nothing could
    // consume the audio and asking for it would be pure cost. Manual row M9.
    let pair = spawn_mic_pair(Demand::Unknown, false);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!pair.server_mic.started());
    assert_eq!(pair.server_mic.start_count(), 0);
    pair.shutdown().await;
}

/// A counting stand-in for the server: completes the handshake and then tallies what
/// arrives on the control stream. The test below is about the client's own session loop,
/// not about anything the real server does with these messages, and a real `run_server`
/// cannot report how often the client spoke.
async fn count_control_messages(
    endpoint: Endpoint,
    pings: Arc<AtomicU64>,
    mic_wanted: Arc<AtomicU64>,
) {
    let Ok(Incoming::Peer(mut peer)) = endpoint.accept().await else {
        panic!("the client never connected");
    };
    let mut rx = peer.take_incoming();
    let sender = peer.sender();
    match rx.recv().await {
        Some(Msg::Hello { .. }) => {}
        other => panic!("expected Hello, got {other:?}"),
    }
    sender
        .send_control(&Msg::HelloAck {
            version: PROTOCOL_VERSION,
            name: "server".into(),
            audio: AudioParams::DEFAULT,
        })
        .await
        .unwrap();
    while let Some(m) = rx.recv().await {
        match m {
            Msg::Ping(_) => {
                pings.fetch_add(1, Ordering::Relaxed);
            }
            Msg::MicWanted { .. } => {
                mic_wanted.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    // `peer` is held to here on purpose: dropping it closes the connection, and the
    // client would reconnect and start the handshake over.
    drop(peer);
}

#[tokio::test]
async fn a_client_with_no_virtual_microphone_still_runs_its_session_loop() {
    // The other half of `a_client_with_no_virtual_microphone_never_opens_the_server_one`,
    // which only ever looked at the server. With no virtual microphone there is no
    // playback worker and so no `watch::Sender` for mic demand unless `RecvSide` holds
    // one itself; without it `changed()` returns `Err` at once and for ever, its
    // `select!` arm is permanently ready, and because the arm is ahead of the ping and
    // the stats tick in a `biased` select it starves both while flooding the control
    // stream with `MicWanted { wanted: false }`. That is every Windows client today, and
    // its only symptom is a hot core.
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
    let (inject, _inj) = MockInject::new(screens(1000, 500));

    let pings = Arc::new(AtomicU64::new(0));
    let asks = Arc::new(AtomicU64::new(0));
    let server = tokio::spawn(count_control_messages(
        server_ep,
        pings.clone(),
        asks.clone(),
    ));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Fixed(server_addr),
            stats: false,
            audio: CaptureSource::Disabled,
            audio_counters: None,
            mic: PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
        },
        shutdown_rx,
    ));

    // Long enough for the one-second ping interval to have fired at least twice.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let asked = asks.load(Ordering::Relaxed);
    let pinged = pings.load(Ordering::Relaxed);

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();

    assert!(
        asked <= 3,
        "the client asked about the microphone {asked} times in 1.5 s; it has no \
         virtual microphone, so it should say so once and then stay quiet"
    );
    assert!(
        pinged >= 1,
        "the client sent no ping in 1.5 s: its session loop is starved"
    );
}

#[tokio::test]
async fn a_disconnect_closes_the_server_microphone() {
    // Review Focus 1. No client means no consumer. Clearing the peer alone would stop the
    // frames but leave the device open for the life of the process.
    let pair = spawn_mic_pair(Demand::Wanted, true);
    assert!(wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await);

    // Tear the client down using its own kill switch, leaving the server running and
    // untouched. Firing the pair's shared `shutdown_tx` here instead would also start
    // the *server's* own teardown, and `Shared`'s drop stops the mic capture backend as
    // a side effect of that shutdown — which would satisfy the assertion below even if
    // the disconnect path's `shared.mic.set_wanted(false)` were deleted. Only
    // `client_kill` isolates the property this test is for: the indicator going out
    // because the server noticed the client leave, not because the server itself quit.
    pair.client_kill.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), pair.client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let mic = pair.server_mic.clone();
    assert!(
        wait_until(move || !mic.started(), Duration::from_secs(5)).await,
        "the microphone must close when the client goes away"
    );

    // The disconnect path has been proven on a server that is still running; now tear
    // that server down too so the test does not leak it.
    pair.shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), pair.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_resumed_stream_is_not_discarded_as_late() {
    // Spec §3.4. The gate closes, the sender's numbering stands still while the client's
    // read cursor keeps advancing, then the gate reopens. Without the client resetting
    // its buffer as it asks, every arriving frame is counted late and the listener hears
    // silence for up to 750 ms.
    let pair = spawn_mic_pair(Demand::Wanted, true);
    assert!(wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await);
    for i in 0..100 {
        pair.server_mic.push(&sine_frame(i));
        pair.client_mic.drain_frames(1);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Close the gate, let the cursor walk on, then reopen it.
    pair.client_mic.set_demand(Demand::Idle);
    assert!(wait_until(|| !pair.server_mic.started(), Duration::from_secs(10)).await);
    for _ in 0..40 {
        pair.client_mic.drain_frames(1);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let late_before = pair.heard.late.load(Ordering::Relaxed);
    // A zero-is-good counter alone cannot tell "every frame arrived on time" apart from
    // "nothing arrived at all" — exactly the failure task 16 found in the demand-gate
    // bug, where every counter read clean while the recorded audio was silence. Snapshot
    // what has reached the virtual microphone so far, and after the resume check that
    // real, correctly-leveled audio actually crossed, not just that the gate reopened.
    let before_resume = pair.client_mic.recorded().len();

    pair.client_mic.set_demand(Demand::Wanted);
    assert!(wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await);
    for i in 0..200 {
        pair.server_mic.push(&sine_frame(i));
        pair.client_mic.drain_frames(1);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let late = pair.heard.late.load(Ordering::Relaxed) - late_before;
    assert!(
        late < 10,
        "{late} frames were discarded as late after the stream resumed: the client is \
         not resetting its buffer when it asks for the microphone"
    );

    let rec = pair.client_mic.recorded();
    let resumed = &rec[before_resume.min(rec.len())..];
    assert!(
        resumed.len() > 180 * FRAME_INTERLEAVED,
        "only {} samples reached the virtual microphone after the resume, expected \
         about {}",
        resumed.len(),
        200 * FRAME_INTERLEAVED
    );
    let peak = resumed
        .iter()
        .map(|s| i32::from(*s).abs())
        .max()
        .unwrap_or(0);
    assert!(
        (8_000..=12_000).contains(&peak),
        "the resumed tone arrived at level {peak}, expected about 10000"
    );
    pair.shutdown().await;
}

/// The test that matters most in this file.
///
/// A review found that the client's post-handshake `MicWanted` send (the unconditional
/// one right after `HelloAck`, in `client.rs::session`) is the only thing that reopens a
/// reconnecting client's microphone. `session()` calls `mic.wanted()` — a fresh clone of
/// `RecvSide`'s long-lived `watch::Receiver` — at the top of every call, including a
/// reconnect. The clone itself is *not* what catches it up: `watch::Receiver::clone`
/// copies the *source* receiver's recorded version, and `RecvSide.wanted_rx` (the field
/// `.wanted()` clones from) is never itself polled, so its version sits at
/// `Version::INITIAL` forever — every clone is born stale, not caught up. What actually
/// consumes any pending change is the very next line in `session()`,
/// `let wanted = *mic_wanted.borrow_and_update();`, which silently marks the fresh clone
/// caught up to whatever demand is *right now*, with no `changed()` event and no send.
/// From that instant on, `changed()` only resolves on a version newer than the one
/// `borrow_and_update()` just recorded — which is exactly why demand staying `Wanted`
/// across a reconnect (nothing newer to see) produces no edge for the main select loop's
/// `mic_wanted.changed()` arm to react to. Only the unconditional `MicWanted` send two
/// lines later, right after that same `borrow_and_update()`, tells the far end anything.
/// Without it, a user whose network blipped mid-recording finds the microphone shut for
/// good, with no counter and no log line saying why.
///
/// This property turned out not to be exclusive to reconnects — with the demand-gate fix
/// in `recv.rs` (`set_wanted`, using `send_if_modified` instead of an unconditional
/// `send`) every test in this file that needs the microphone to open now depends on the
/// same handshake send, since the QUIC handshake takes longer than the mock backend's
/// startup and the poller has usually already settled by the time any `session()` first
/// subscribes. That is a good thing for coverage, not a reason to drop this test: it is
/// still the one that pins the *reconnect* scenario specifically — the same `RecvSide`
/// surviving a real disconnect and being asked again, with the client task never
/// restarted — which is exactly the case the original review found unguarded.
#[tokio::test]
async fn a_reconnecting_client_that_is_still_recording_reopens_the_microphone() {
    let mut pair = spawn_mic_pair(Demand::Wanted, true);
    assert!(
        wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await,
        "the first session must open the microphone before the reconnect is even tried"
    );

    let new_mic = pair.drop_client_connection().await;
    // The linger is three seconds and the client's backoff starts at 500 ms, so ten
    // seconds is comfortable room for a reconnect that also has to rebind a UDP socket.
    assert!(
        wait_until(|| new_mic.started(), Duration::from_secs(10)).await,
        "a reconnecting client that is still recording must reopen the microphone on its \
         own, with no change in demand to trigger it"
    );
    pair.shutdown().await;
}
