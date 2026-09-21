use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{InjectCall, MockCapture, MockInject};
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
