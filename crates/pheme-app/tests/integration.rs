use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_app::target::Target;
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{InjectCall, MockCapture, MockCaptureHandle, MockInject, MockInjectLog};
use pheme_input::CaptureMode;
use pheme_net::{Endpoint, Identity, SharedTrust, TrustStore};
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

/// Binds a server endpoint to `addr`, retrying until the socket is free.
///
/// Awaiting the previous server's task is not the same as the operating system having
/// released its socket: quinn's endpoint driver can hold it for a moment after
/// `run_server` returns, and a rebind that loses that race fails with `AddrInUse`. The
/// test is asserting that a client reconnects to a restarted server, not that sockets are
/// released instantly, so waiting for the port is part of restarting the server.
async fn bind_server_retrying(
    addr: std::net::SocketAddr,
    id: &Identity,
    trust: SharedTrust,
) -> Endpoint {
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

/// A paired server + client on loopback with mock backends, both already spawned.
struct Pair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    inj: MockInjectLog,
}

fn spawn_pair() -> Pair {
    spawn_pair_with_hotkeys(Hotkeys::default())
}

/// As `spawn_pair`, but with a lock hotkey the mock capture backend can press.
fn spawn_pair_with_hotkeys(hotkeys: Hotkeys) -> Pair {
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
            hotkeys,
            lock_hotkey_trigger: None,
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
            mic: pheme_app::audio::CaptureSource::Disabled,
            mic_counters: None,
            clipboard: None,
        },
        shutdown_rx.clone(),
    ));
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Fixed(server_addr),
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
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
            lock_hotkey_trigger: None,
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
            mic: pheme_app::audio::CaptureSource::Disabled,
            mic_counters: None,
            clipboard: None,
        },
        shutdown_rx.clone(),
    ));
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Fixed(server_addr),
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
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
            target: Target::Fixed(server_addr),
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
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
                lock_hotkey_trigger: None,
                stats: false,
                audio: pheme_app::audio::PlaybackSource::Disabled,
                audio_stats: None,
                mic: pheme_app::audio::CaptureSource::Disabled,
                mic_counters: None,
                clipboard: None,
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
    let server_ep2 = bind_server_retrying(server_addr, &sid, strust).await;
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

/// A shutdown that arrives while `target.resolve()` is in flight must be observed at
/// once, not only once the resolution itself completes.
///
/// The target is an mDNS name nothing on the network advertises, so resolving it walks
/// the mDNS lookup (bounded by `Target`'s multi-second timeout) and, on no answer, falls
/// through to an unbounded system resolver call. Left unraced against `shutdown`, either
/// one would hold `run_client` up well past the point the caller asked it to stop — the
/// same shape this repository has already had to fix twice for the router thread. This
/// asserts `run_client` returns in well under a second, which it can only do by racing
/// the resolution itself against `shutdown.changed()`, exactly as the `connect` below it
/// already does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_resolution_is_observed_promptly() {
    let cdir = tempfile::tempdir().unwrap();
    let cid = Identity::load_or_create(cdir.path(), "lap").unwrap();
    let ctrust = TrustStore::load(cdir.path()).unwrap().shared();
    let client_ep = Endpoint::client(&cid, ctrust).unwrap();
    let (inject, _inj) = MockInject::new(screens(1000, 500));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Mdns("nothing-on-this-network-advertises-this-name".into()),
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
        },
        shutdown_rx,
    ));

    // Give the reconnect loop a moment to actually start resolving before shutdown
    // arrives, so this exercises the race rather than the `shutdown.borrow()` check
    // at the top of the loop.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    shutdown_tx.send(true).unwrap();

    tokio::time::timeout(Duration::from_millis(500), client)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "run_client did not return within 500ms of shutdown; \
                 the mDNS timeout alone is several seconds"
            )
        })
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "shutdown took {elapsed:?} to be observed"
    );
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
            lock_hotkey_trigger: None,
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
            mic: pheme_app::audio::CaptureSource::Disabled,
            mic_counters: None,
            clipboard: None,
        },
        shutdown_rx.clone(),
    ));
    let client = tokio::spawn(run_client(
        ClientDeps {
            name: "lap".into(),
            inject: Box::new(inject),
            endpoint: client_ep,
            target: Target::Fixed(server_addr),
            stats: false,
            audio: pheme_app::audio::CaptureSource::Disabled,
            audio_counters: None,
            mic: pheme_app::audio::PlaybackSource::Disabled,
            mic_stats: None,
            clipboard: None,
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

/// A backend that breaks the `InputCapture::stop` contract and keeps an event sender
/// must not stop the *process* from exiting.
///
/// Deliberately not a `#[tokio::test]`: the hazard this guards against is in dropping
/// the runtime, not in `run_server`. A bounded `spawn_blocking(|| router.join())` lets
/// `run_server` return on time and still hangs, because tokio's runtime drop waits
/// forever for a blocking task to return. The runtime is therefore built, used and
/// dropped inside a thread of its own, and the assertion is that the thread reaches the
/// far side of that drop.
#[test]
fn a_router_thread_that_cannot_finish_does_not_keep_the_process_alive() {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let elapsed = rt.block_on(async {
            let Pair {
                server,
                client,
                shutdown_tx,
                cap,
                inj: _,
            } = spawn_pair();
            wait_connected(&cap).await;
            // From here on `stop()` keeps its sender, so the router thread's
            // `ev_rx.recv()` never returns -- exactly what `PortalCapture::stop()`
            // does when it gives up and detaches its session thread.
            cap.keep_sender_on_stop();

            let t = Instant::now();
            shutdown_tx.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(20), server)
                .await
                .expect("run_server did not return at all")
                .unwrap()
                .unwrap();
            let elapsed = t.elapsed();
            let _ = tokio::time::timeout(Duration::from_secs(5), client).await;
            elapsed
        });
        // The hang the old shape merely relocated: an abandoned `spawn_blocking` task
        // is still running here, and `Runtime::drop` waits for it forever.
        drop(rt);
        let _ = done_tx.send(elapsed);
    });

    let elapsed = done_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the runtime never finished dropping: the process would hang on exit");
    // Proves the test exercised the abandoning path rather than a router thread that
    // quietly exited: `ROUTER_JOIN_TIMEOUT` is 5 s, and shutdown cannot beat it here.
    assert!(
        elapsed >= Duration::from_millis(4_500),
        "shutdown returned in {elapsed:?}, so the router thread was not stuck and this \
         test proves nothing"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_capture_backend_learns_which_edges_have_clients() {
    let Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj: _,
    } = spawn_pair();
    wait_connected(&cap).await;

    let calls = cap.edge_calls();
    let last = calls
        .last()
        .expect("connecting a client must declare its edge");
    assert_eq!(
        last.len(),
        1,
        "exactly the connected client's edge, nothing else: {calls:?}"
    );
    assert_eq!(last[0].side, Side::Right);

    // Tear down the client connection the same way other tests do: aborting the task
    // drops its `Peer`, closing the QUIC connection without a clean Bye.
    client.abort();
    assert!(
        wait_until(
            || cap.edge_calls().last().is_some_and(|c| c.is_empty()),
            Duration::from_secs(8)
        )
        .await,
        "edges never cleared after the client vanished"
    );

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locking_withdraws_the_edges_and_unlocking_puts_them_back() {
    // Spec §7: "locking removes the barriers". On Wayland an edge left declared while
    // locked is not a harmless snag -- the compositor starts capturing, the core
    // declines, and the user is inside a capture that swallows every key.
    const LOCK: KeyCode = KeyCode(0x47); // ScrollLock
    let Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj: _,
    } = spawn_pair_with_hotkeys(Hotkeys { lock: Some(LOCK) });
    wait_connected(&cap).await;
    // `wait_connected` proves the client is there by crossing the edge, which leaves
    // the input on the client. Come back first: withdrawing the barriers is what a
    // lock does on the *server* screen, and the lock taken while remote is the
    // neighbouring test's subject, not this one's.
    cap.push(CaptureEvent::MotionRel { dx: -20_000, dy: 0 });
    assert!(
        wait_until(
            || cap.mode() == CaptureMode::Observe,
            Duration::from_secs(5)
        )
        .await,
        "never came back from the client"
    );
    assert_eq!(
        cap.edge_calls().last().map(|c| c.len()),
        Some(1),
        "the connected client's edge: {:?}",
        cap.edge_calls()
    );

    cap.push(CaptureEvent::Key {
        code: LOCK,
        down: true,
    });
    assert!(
        wait_until(
            || cap.edge_calls().last().is_some_and(|c| c.is_empty()),
            Duration::from_secs(5)
        )
        .await,
        "locking left the edges declared: {:?}",
        cap.edge_calls()
    );

    cap.push(CaptureEvent::Key {
        code: LOCK,
        down: false,
    });
    cap.push(CaptureEvent::Key {
        code: LOCK,
        down: true,
    });
    assert!(
        wait_until(
            || cap
                .edge_calls()
                .last()
                .is_some_and(|c| c.len() == 1 && c[0].side == Side::Right),
            Duration::from_secs(5)
        )
        .await,
        "unlocking did not put the edges back: {:?}",
        cap.edge_calls()
    );

    client.abort();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locking_while_remote_waits_for_the_return_before_withdrawing_the_edges() {
    // The other half of spec §7. Withdrawing the barriers matters only while the
    // pointer is local, because only a local pointer can reach one. Doing it while the
    // input is on the client means `SetPointerBarriers([])` and `Disable()` against a
    // capture the compositor is actively running, which it may answer by ending the
    // capture -- dumping the user back on the server screen at the very moment the
    // core's own rule (`lock_blocks_switching_and_leaving`) says to stay put.
    //
    // So: locking while remote changes nothing at the backend, and the withdrawal
    // happens on the way back to local, for a lock that outlives the return.
    const LOCK: KeyCode = KeyCode(0x47); // ScrollLock
    let Pair {
        server,
        client,
        shutdown_tx,
        cap,
        inj,
    } = spawn_pair_with_hotkeys(Hotkeys { lock: Some(LOCK) });
    wait_connected(&cap).await;
    let before = cap.edge_calls();
    assert_eq!(
        before.last().map(|c| c.len()),
        Some(1),
        "the connected client's edge: {before:?}"
    );

    cap.push(CaptureEvent::Key {
        code: LOCK,
        down: true,
    });
    cap.push(CaptureEvent::Key {
        code: LOCK,
        down: false,
    });
    // A button pressed after the lock key. The router thread takes events from one
    // channel in order and runs each event's actions before the next, so seeing this
    // button arrive at the client means the lock's own actions have already run: the
    // edge calls below are then a settled fact rather than a race.
    cap.push(CaptureEvent::Button {
        btn: pheme_proto::Button::Left,
        down: true,
    });
    assert!(
        wait_until(
            || inj
                .calls()
                .contains(&InjectCall::Button(pheme_proto::Button::Left, true)),
            Duration::from_secs(5)
        )
        .await,
        "the button never reached the client, so the lock may not have been handled yet"
    );
    assert_eq!(
        cap.edge_calls(),
        before,
        "locking while remote touched the barriers; on Wayland that is a Disable() \
         against a running capture"
    );
    assert_eq!(
        cap.mode(),
        CaptureMode::Grab,
        "the input must stay on the client while locked"
    );

    // The compositor ends the capture on its own (spec §4.3), which is one of the four
    // ways back to local. The lock is still on, so the withdrawal deferred above is now
    // due -- and this path never touches the lock, so only the deferral can deliver it.
    cap.push(CaptureEvent::CaptureEnded);
    assert!(
        wait_until(
            || cap.edge_calls().last().is_some_and(|c| c.is_empty()),
            Duration::from_secs(5)
        )
        .await,
        "the return to local did not withdraw the edges the lock had asked for: {:?}",
        cap.edge_calls()
    );
    assert_eq!(
        cap.mode(),
        CaptureMode::Observe,
        "back on the server screen"
    );

    client.abort();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
