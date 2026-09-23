use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{InjectCall, MockCapture, MockCaptureHandle, MockInject, MockInjectLog};
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

/// A paired server + client on loopback with mock backends, both already spawned.
struct Pair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    inj: MockInjectLog,
}

fn spawn_pair() -> Pair {
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

    let (capture, cap) = MockCapture::new(screens(1920, 1080));
    let (inject, inj) = MockInject::new(screens(1000, 500));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(run_server(
        ServerDeps {
            name: "server".into(),
            capture: Box::new(capture),
            endpoint: server_ep,
            placements: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
            hotkeys: Hotkeys::default(),
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
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
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
        },
        shutdown_rx,
    ));
    Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj,
    }
}

/// Pushes a right-edge crossing (two absolute moves) as the mock OS would report it.
fn push_edge_crossing(cap: &MockCaptureHandle) {
    cap.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
    cap.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
}

/// Waits for the capture to start and for one edge crossing to be accepted (Grab), which
/// proves the client is connected.
async fn wait_connected(cap: &MockCaptureHandle) {
    assert!(
        wait_until(|| cap.is_started(), Duration::from_secs(5)).await,
        "capture started"
    );
    let connected = wait_until(
        || {
            push_edge_crossing(cap);
            cap.mode() == CaptureMode::Grab
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(connected, "client never connected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_and_client_exchange_input_over_quic() {
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

    let (capture, cap_handle) = MockCapture::new(screens(1920, 1080));
    let (inject, inj_log) = MockInject::new(screens(1000, 500));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(run_server(
        ServerDeps {
            name: "server".into(),
            capture: Box::new(capture),
            endpoint: server_ep,
            placements: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
            hotkeys: Hotkeys {
                lock: Some(KeyCode(0x47)),
            },
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
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
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
        },
        shutdown_rx.clone(),
    ));

    // Wait for the capture to be started (start() is called once the server runtime is up)
    // and for the client to be connected: pushing an edge crossing must produce an Enter.
    assert!(
        wait_until(|| cap_handle.is_started(), Duration::from_secs(5)).await,
        "capture started"
    );
    let connected = wait_until(
        || {
            cap_handle.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
            cap_handle.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
            cap_handle.mode() == CaptureMode::Grab
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(connected, "client never connected");
    assert!(
        wait_until(
            || inj_log.calls().contains(&InjectCall::MoveAbs(0, 250)),
            Duration::from_secs(2)
        )
        .await
    );
    assert_eq!(cap_handle.warps(), vec![(960, 540)]);

    // Throughput / latency: 10 000 relative moves must arrive within 5 s (< 0.5 ms each on average).
    let before = inj_log.len();
    let t = Instant::now();
    for _ in 0..10_000 {
        cap_handle.push(CaptureEvent::MotionRel { dx: 1, dy: 0 });
    }
    assert!(
        wait_until(|| inj_log.len() >= before + 10_000, Duration::from_secs(5)).await,
        "got {} of 10000",
        inj_log.len() - before
    );
    let elapsed = t.elapsed();
    eprintln!("10000 moves in {elapsed:?} ({:?} each)", elapsed / 10_000);
    assert!(elapsed < Duration::from_secs(5));

    // Keys go over the control stream; a held key is released when we leave.
    cap_handle.push(CaptureEvent::Key {
        code: KeyCode(0x04),
        down: true,
    });
    assert!(
        wait_until(
            || inj_log
                .calls()
                .contains(&InjectCall::Key(KeyCode(0x04), true)),
            Duration::from_secs(2)
        )
        .await
    );
    cap_handle.push(CaptureEvent::MotionRel { dx: -20_000, dy: 0 });
    assert!(
        wait_until(
            || cap_handle.mode() == CaptureMode::Observe,
            Duration::from_secs(2)
        )
        .await,
        "left the client"
    );
    assert!(
        wait_until(
            || inj_log
                .calls()
                .contains(&InjectCall::Key(KeyCode(0x04), false)),
            Duration::from_secs(2)
        )
        .await,
        "key released on Leave"
    );
    // The return warp lands one pixel inside the right edge at the matching height
    // (vy = 250 of 500 → y = 540 of 1080), and only after the grab has been released:
    // `set_mode` is synchronous, so a warp issued while a grab/clip was still active
    // could otherwise be re-centred or clamped away.
    assert_eq!(
        cap_handle.warps_with_mode().last().copied(),
        Some(((1918, 540), CaptureMode::Observe)),
        "warps: {:?}",
        cap_handle.warps_with_mode()
    );

    // Shutdown both; server must not be left grabbed.
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(cap_handle.mode(), CaptureMode::Observe);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_reconnects_after_server_restart() {
    let sdir = tempfile::tempdir().unwrap();
    let cdir = tempfile::tempdir().unwrap();
    let sid = Identity::load_or_create(sdir.path(), "server").unwrap();
    let cid = Identity::load_or_create(cdir.path(), "lap").unwrap();
    let strust = TrustStore::load(sdir.path()).unwrap().shared();
    let ctrust = TrustStore::load(cdir.path()).unwrap().shared();
    strust.write().unwrap().add("lap", &cid.fingerprint);
    ctrust.write().unwrap().add("server", &sid.fingerprint);

    let listen: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server_ep = Endpoint::server(listen, &sid, strust.clone()).unwrap();
    let server_addr = server_ep.local_addr().unwrap();
    let client_ep = Endpoint::client(&cid, ctrust).unwrap();
    let (inject, inj_log) = MockInject::new(screens(1000, 500));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            server_addr,
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
        },
        shutdown_rx.clone(),
    ));

    let run_once = |ep: Endpoint, stop: watch::Receiver<bool>| {
        let (capture, handle) = MockCapture::new(screens(1920, 1080));
        let task = tokio::spawn(run_server(
            ServerDeps {
                name: "server".into(),
                capture: Box::new(capture),
                endpoint: ep,
                placements: vec![ClientPlacement {
                    name: "lap".into(),
                    side: Side::Right,
                    span: (0.0, 1.0),
                }],
                hotkeys: Hotkeys::default(),
                stats: false,
                audio: pheme_app::audio::PlaybackSource::Disabled,
                audio_stats: None,
            },
            stop,
        ));
        (task, handle)
    };

    let (stop1_tx, stop1_rx) = watch::channel(false);
    let (server1, handle1) = run_once(server_ep, stop1_rx);
    assert!(wait_until(|| handle1.is_started(), Duration::from_secs(5)).await);
    assert!(
        wait_until(
            || {
                handle1.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
                handle1.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
                handle1.mode() == CaptureMode::Grab
            },
            Duration::from_secs(5)
        )
        .await
    );
    stop1_tx.send(true).unwrap();
    server1.await.unwrap().unwrap();

    // Restart the server on the same port; the client must come back on its own.
    let server_ep2 = Endpoint::server(server_addr, &sid, strust).unwrap();
    let (stop2_tx, stop2_rx) = watch::channel(false);
    let (server2, handle2) = run_once(server_ep2, stop2_rx);
    assert!(wait_until(|| handle2.is_started(), Duration::from_secs(5)).await);
    let before = inj_log.len();
    assert!(
        wait_until(
            || {
                handle2.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
                handle2.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
                inj_log.len() > before
            },
            Duration::from_secs(15)
        )
        .await,
        "client did not reconnect"
    );
    stop2_tx.send(true).unwrap();
    shutdown_tx.send(true).unwrap();
    server2.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_releases_grab_when_client_vanishes_silently() {
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

    let (capture, cap_handle) = MockCapture::new(screens(1920, 1080));
    let (inject, _inj_log) = MockInject::new(screens(1000, 500));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(run_server(
        ServerDeps {
            name: "server".into(),
            capture: Box::new(capture),
            endpoint: server_ep,
            placements: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
            hotkeys: Hotkeys::default(),
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
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
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
        },
        shutdown_rx.clone(),
    ));

    assert!(
        wait_until(|| cap_handle.is_started(), Duration::from_secs(5)).await,
        "capture started"
    );
    let connected = wait_until(
        || {
            cap_handle.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
            cap_handle.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
            cap_handle.mode() == CaptureMode::Grab
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(connected, "client never connected");

    // The client vanishes without a clean Bye/Leave handshake: aborting the task drops its
    // `Peer` (and the endpoint it was moved into), which closes the QUIC connection but never
    // sends an application-level Bye.
    client.abort();

    assert!(
        wait_until(
            || cap_handle.mode() == CaptureMode::Observe,
            Duration::from_secs(8)
        )
        .await,
        "server never released the grab after the client vanished (idle timeout is 5s)"
    );

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_grab_leaves_server_local() {
    let Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj,
    } = spawn_pair();
    wait_connected(&cap).await;
    // One full successful cycle first: Enter reaches the client, then cross back.
    assert!(
        wait_until(
            || inj.calls().contains(&InjectCall::MoveAbs(0, 250)),
            Duration::from_secs(2)
        )
        .await
    );
    cap.push(CaptureEvent::MotionRel { dx: -20_000, dy: 0 });
    assert!(
        wait_until(
            || cap.mode() == CaptureMode::Observe,
            Duration::from_secs(2)
        )
        .await,
        "left the client"
    );
    let warps_before = cap.warps().len();
    let inj_before = inj.calls();

    // The next grab fails (e.g. another X client holds a grab): the server must give up
    // the switch instead of staying Remote with the OS still delivering input locally.
    cap.fail_next_grab();
    push_edge_crossing(&cap);
    assert!(
        wait_until(|| cap.warps().len() > warps_before, Duration::from_secs(2)).await,
        "abort warp never happened"
    );
    assert_eq!(
        cap.warps_with_mode()[warps_before..],
        [((960, 540), CaptureMode::Observe)],
        "aborted switch re-centres the pointer, nothing else"
    );
    assert_eq!(cap.mode(), CaptureMode::Observe);
    // No Enter was sent, so the client injected nothing for that crossing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_calls = &inj.calls()[inj_before.len()..];
    assert!(
        !new_calls
            .iter()
            .any(|c| matches!(c, InjectCall::MoveAbs(..))),
        "client got input for an aborted switch: {new_calls:?}"
    );

    // The server is not wedged: the next (non-failing) crossing switches again.
    push_edge_crossing(&cap);
    assert!(
        wait_until(|| cap.mode() == CaptureMode::Grab, Duration::from_secs(2)).await,
        "did not switch after the aborted attempt"
    );
    assert!(
        wait_until(
            || inj.calls().len() > inj_before.len()
                && inj.calls()[inj_before.len()..].contains(&InjectCall::MoveAbs(0, 250)),
            Duration::from_secs(2)
        )
        .await,
        "Enter never reached the client"
    );

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(cap.mode(), CaptureMode::Observe);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_server_fails_when_the_capture_backend_dies() {
    let Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj: _,
    } = spawn_pair();
    wait_connected(&cap).await;

    // The backend's event thread dies while a client is connected and being controlled:
    // the router thread sees the channel close and exits, and `run_server` must fail
    // instead of silently never switching again.
    cap.disconnect();
    let result = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("run_server did not return within 2 s")
        .unwrap();
    let err = result.expect_err("run_server must fail when the capture backend dies");
    assert!(
        err.to_string()
            .contains("input capture backend stopped unexpectedly"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        cap.mode(),
        CaptureMode::Observe,
        "grab released on the way out"
    );

    // The client is told and goes back to reconnecting; it shuts down cleanly.
    assert!(
        !client.is_finished(),
        "client keeps reconnecting on its own"
    );
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
