//! The clipboard end to end: a server and a client in one process over a real QUIC
//! connection, each with its own mock clipboard.
//!
//! Design §3.1: the clipboard crosses when the pointer crosses, and at no other time.
//! The server sends its clipboard with the `Enter` that hands the pointer over; the
//! client sends its own when `Leave` or `Bye` brings the pointer back; both sides apply
//! whatever arrives.

use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::clipboard::ClipboardService;
use pheme_app::server::{run_server, ServerDeps};
use pheme_clip::mock::{MockClipboard, MockClipboardHandle};
use pheme_clip::Clipboard;
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{InjectCall, MockCapture, MockCaptureHandle, MockInject, MockInjectLog};
use pheme_input::CaptureMode;
use pheme_net::{Endpoint, Identity, TrustStore};
use pheme_proto::ScreenInfo;
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

/// Pushes a right-edge crossing (two absolute moves) as the mock OS would report it.
/// The mock server screen is 1920 wide with the client on its right edge.
fn push_edge_crossing(cap: &MockCaptureHandle) {
    cap.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
    cap.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
}

/// A paired server and client on loopback, each with its own mock clipboard.
struct ClipPair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    inj: MockInjectLog,
    server_clip: MockClipboardHandle,
    client_clip: MockClipboardHandle,
}

impl ClipPair {
    /// True once the client has injected the `MoveAbs` that its `Msg::Enter` handling
    /// produces. The client's injection screen is 1000x500 and the crossing lands at
    /// the same fraction of the server's edge (y = 540 of 1080 -> 250 of 500), entering
    /// from the client's left edge (x = 0) -- the same call
    /// `server_and_client_exchange_input_over_quic` asserts on in `integration.rs`.
    fn injected_enter(&self) -> bool {
        self.inj.calls().contains(&InjectCall::MoveAbs(0, 250))
    }

    async fn shutdown(self) {
        self.shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.client)
            .await
            .expect("client task timed out")
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .expect("server task timed out")
            .unwrap()
            .unwrap();
    }
}

/// Builds a `ClipPair`, optionally without a clipboard on the client side (`with_client_clip
/// = false`), which is what a GNOME Wayland client looks like: `ClipboardService::spawn`
/// returned `None` and input must not notice.
///
/// Modelled on `spawn_pair_with_hotkeys` in `integration.rs`.
async fn spawn_clip_pair(with_client_clip: bool) -> ClipPair {
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

    let (server_clip_mock, server_clip) = MockClipboard::new();
    let server_clipboard =
        ClipboardService::spawn(move || Ok(Box::new(server_clip_mock) as Box<dyn Clipboard>));

    let (client_clip_mock, client_clip) = MockClipboard::new();
    let client_clipboard = if with_client_clip {
        ClipboardService::spawn(move || Ok(Box::new(client_clip_mock) as Box<dyn Clipboard>))
    } else {
        None
    };

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
            clipboard: server_clipboard,
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
            clipboard: client_clipboard,
        },
        shutdown_rx,
    ));
    ClipPair {
        server,
        client,
        shutdown_tx,
        cap,
        inj,
        server_clip,
        client_clip,
    }
}

async fn cross_to_the_client(cap: &MockCaptureHandle) {
    assert!(
        wait_until(
            || {
                push_edge_crossing(cap);
                cap.mode() == CaptureMode::Grab
            },
            Duration::from_secs(5),
        )
        .await,
        "the pointer never reached the client"
    );
}

async fn cross_back_to_the_server(cap: &MockCaptureHandle) {
    // The same walk back off the client's left edge every other test uses.
    cap.push(CaptureEvent::MotionRel { dx: -20_000, dy: 0 });
    assert!(
        wait_until(
            || cap.mode() == CaptureMode::Observe,
            Duration::from_secs(5)
        )
        .await,
        "the pointer never came back to the server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_clipboard_reaches_the_client_when_the_pointer_crosses() {
    let p = spawn_clip_pair(true).await;
    p.server_clip.copy("copied on the server");
    cross_to_the_client(&p.cap).await;
    assert!(
        wait_until(
            || p.client_clip.text().as_deref() == Some("copied on the server"),
            Duration::from_secs(5)
        )
        .await,
        "the client's clipboard never received the server's text"
    );
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_clipboard_returns_when_the_pointer_comes_back() {
    let p = spawn_clip_pair(true).await;
    cross_to_the_client(&p.cap).await;
    p.client_clip.copy("copied on the client");
    cross_back_to_the_server(&p.cap).await;
    assert!(
        wait_until(
            || p.server_clip.text().as_deref() == Some("copied on the client"),
            Duration::from_secs(5)
        )
        .await,
        "the server's clipboard never received the client's text"
    );
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossing_twice_without_copying_sends_one_clipboard() {
    let p = spawn_clip_pair(true).await;
    p.server_clip.copy("just once");
    cross_to_the_client(&p.cap).await;
    assert!(wait_until(|| p.client_clip.sets() == 1, Duration::from_secs(5)).await);
    cross_back_to_the_server(&p.cap).await;
    cross_to_the_client(&p.cap).await;
    // Give a second transfer every chance to appear before asserting it did not.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        p.client_clip.sets(),
        1,
        "the same clipboard was written again; the echo guard is not holding"
    );
    p.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_without_a_clipboard_still_crosses_the_edge() {
    // GNOME Wayland on the client: `ClipboardService::spawn` returned None.
    // Input must be completely unaffected.
    let p = spawn_clip_pair(false).await;
    cross_to_the_client(&p.cap).await;
    assert!(
        wait_until(|| p.injected_enter(), Duration::from_secs(5)).await,
        "the client never received Enter"
    );
    p.shutdown().await;
}
