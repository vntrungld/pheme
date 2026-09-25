//! The server's half of monitor input switching, over a real QUIC link to a
//! stand-in client and a `MockMonitor`.
//!
//! Almost everything in task 8 is wiring that only a cabled monitor
//! exercises. These tests replace the monitor with a mock and the client
//! with a peer the test drives by hand, which is enough to pin the three
//! things that are easy to get wrong and invisible when they are: that a
//! crossing commands the monitor at all, that the value commanded is the
//! *client's* rather than this machine's, and that the recovery hotkey asks
//! both machines rather than one.
//!
//! The stand-in client is hand-rolled rather than `run_client` because the
//! client half is task 9: `run_client` still declares `display_input: None`
//! in its `Hello`, so a real client could not yet tell the server which
//! input it is cabled to.

use std::time::{Duration, Instant};

use pheme_app::config::DisplayCfg;
use pheme_app::display::DisplayService;
use pheme_app::server::{run_server, ServerDeps};
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_display::mock::{opens_once, MockMonitor, MockMonitorHandle};
use pheme_input::mock::{MockCapture, MockCaptureHandle};
use pheme_input::CaptureMode;
use pheme_net::{Endpoint, Identity, Peer, TrustStore};
use pheme_proto::{AudioParams, KeyCode, Msg, Os, ScreenInfo, PROTOCOL_VERSION};
use tokio::sync::{mpsc, watch};

/// The input this machine is cabled to.
const SERVER_INPUT: u16 = 0x11;
/// The input the stand-in client declares in its `Hello`. Deliberately
/// different: two machines claiming the same input is the misconfiguration
/// `display_input_conflict` warns about, and it would make every assertion
/// here pass for the wrong reason.
const CLIENT_INPUT: u16 = 0x0f;
const SWITCH: KeyCode = KeyCode(0x5a);

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

/// A server with monitor switching configured, and a stand-in client that has
/// completed the handshake declaring `CLIENT_INPUT`.
struct Rig {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    mon: MockMonitorHandle,
    /// Held, not dropped: dropping it closes the connection and the server
    /// would tear the session down underneath the test.
    _peer: Peer,
    rx: mpsc::Receiver<Msg>,
}

impl Rig {
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.server.await;
    }
}

async fn spawn_rig() -> Rig {
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
    // The monitor starts on this machine's own input, which is what it
    // really shows while the pointer is here.
    let (monitor, mon) = MockMonitor::new("MOCK", "mock", SERVER_INPUT);
    let display = DisplayService::spawn(
        &DisplayCfg {
            input: Some(SERVER_INPUT),
            monitor: None,
            // No cooldown: the policy's holding behaviour has its own tests
            // in `display.rs`, and a real one would only make these wait.
            cooldown_ms: 0,
        },
        Box::new(opens_once(monitor)),
    )
    .expect("the feature is configured on");

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
                lock: None,
                switch_display: Some(SWITCH),
            },
            lock_hotkey_trigger: None,
            stats: false,
            audio: pheme_app::audio::PlaybackSource::Disabled,
            audio_stats: None,
            mic: pheme_app::audio::CaptureSource::Disabled,
            mic_counters: None,
            clipboard: None,
            display: Some(display),
            display_input: Some(SERVER_INPUT),
            ipc: None,
        },
        shutdown_tx.clone(),
        shutdown_rx,
    ));

    let mut peer = client_ep
        .connect(server_addr)
        .await
        .expect("connecting to the server");
    let mut rx = peer.take_incoming();
    peer.sender()
        .send_control(&Msg::Hello {
            version: PROTOCOL_VERSION,
            name: "lap".into(),
            os: Os::Linux,
            screens: screens(1000, 500),
            audio: AudioParams::DEFAULT,
            display_input: Some(CLIENT_INPUT),
        })
        .await
        .expect("sending Hello");
    match rx.recv().await {
        Some(Msg::HelloAck { display_input, .. }) => assert_eq!(
            display_input,
            Some(SERVER_INPUT),
            "the server must tell the client which input it is cabled to, or the \
             client can never switch the screen back to it"
        ),
        other => panic!("expected HelloAck, got {other:?}"),
    }

    Rig {
        server,
        shutdown_tx,
        cap,
        mon,
        _peer: peer,
        rx,
    }
}

/// Pushes right-edge crossings until the core accepts one, which is also how
/// the test knows the client is registered with the core: nothing grabs
/// before `client_connected` has run.
async fn cross_to_the_client(cap: &MockCaptureHandle) {
    assert!(
        wait_until(|| cap.is_started(), Duration::from_secs(5)).await,
        "capture started"
    );
    let crossed = wait_until(
        || {
            cap.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
            cap.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
            cap.mode() == CaptureMode::Grab
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(crossed, "the pointer never crossed to the client");
}

/// Reads until a `SwitchDisplay` arrives, ignoring the Enter/Leave/motion
/// traffic a session produces around it.
async fn next_switch_display(rx: &mut mpsc::Receiver<Msg>) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(Msg::SwitchDisplay { input })) => return input,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the control stream ended before a SwitchDisplay arrived"),
            Err(_) => panic!("no SwitchDisplay arrived within 5 s"),
        }
    }
}

/// The hook in the `Msg::Enter` branch, end to end.
///
/// Break it by deleting the `switch_to` call beside the clipboard's: the
/// monitor stays on `SERVER_INPUT` and the person watches their pointer
/// vanish onto a screen they cannot see. Break it just as well by filling
/// `Link::display_input` with `None` instead of the client's `Hello` value,
/// or by reaching for `self.display_input` there: the value asserted is the
/// client's, and this machine's is different.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_commands_the_monitor_to_the_clients_input() {
    let rig = spawn_rig().await;
    cross_to_the_client(&rig.cap).await;
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == CLIENT_INPUT, Duration::from_secs(5)).await,
        "the monitor was left on {:#04x}, not the client's {CLIENT_INPUT:#04x}",
        mon.input()
    );
    rig.shutdown().await;
}

/// The recovery hotkey, both halves and both directions.
///
/// Pressed while the pointer is on the client, the target is the *client's*
/// input; pressed while it is here, this machine's. Either way both machines
/// are asked, because the machine that wants the screen back is by
/// definition not the one the monitor is answering.
///
/// Break it by hard-coding the local branch: the remote press then asks for
/// `SERVER_INPUT` and the first assertion fails. Break it by dropping the
/// `Msg::SwitchDisplay` send and both `next_switch_display` calls time out --
/// which is the case the hotkey exists for, a screen showing the client with
/// no way back. Break it by dropping the local `force` and the last
/// assertion fails, because only this machine can act on a monitor that is
/// showing this machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recovery_hotkey_asks_both_machines() {
    let mut rig = spawn_rig().await;
    cross_to_the_client(&rig.cap).await;

    // Remote: the target is the client's input.
    rig.cap.push(CaptureEvent::Key {
        code: SWITCH,
        down: true,
    });
    assert_eq!(
        next_switch_display(&mut rig.rx).await,
        CLIENT_INPUT,
        "while the pointer is on the client, the peer must be asked for the client's input"
    );

    // Back to local, the way a compositor ends a capture.
    rig.cap.push(CaptureEvent::CaptureEnded);
    assert!(
        wait_until(
            || rig.cap.mode() == CaptureMode::Observe,
            Duration::from_secs(5)
        )
        .await,
        "the core never returned to local"
    );

    rig.cap.push(CaptureEvent::Key {
        code: SWITCH,
        down: true,
    });
    assert_eq!(
        next_switch_display(&mut rig.rx).await,
        SERVER_INPUT,
        "while the pointer is here, the peer must be asked for this machine's input"
    );
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == SERVER_INPUT, Duration::from_secs(5)).await,
        "the local half of the hotkey never reached the monitor; it was left on {:#04x}",
        mon.input()
    );
    rig.shutdown().await;
}

/// The other end of the same hotkey: the client pressed it, and asks every
/// machine to select its own input because it cannot know which one the
/// monitor is listening to.
///
/// Break it by leaving the `Msg::SwitchDisplay` arm out of the server's
/// receive loop -- it falls into the "ignoring message from client" arm,
/// where a client that has lost the screen stays lost.
///
/// The request here asks for the *client's* input while the monitor is
/// showing this machine's, so the assertion cannot pass by accident; a real
/// monitor would refuse this machine's command and honour the client's,
/// which is the asymmetry a mock cannot model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_display_from_the_client_reaches_the_monitor() {
    let rig = spawn_rig().await;
    assert_eq!(rig.mon.input(), SERVER_INPUT);
    rig._peer
        .sender()
        .send_control(&Msg::SwitchDisplay {
            input: CLIENT_INPUT,
        })
        .await
        .expect("sending SwitchDisplay");
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == CLIENT_INPUT, Duration::from_secs(5)).await,
        "the client's request never reached the monitor; it was left on {:#04x}",
        mon.input()
    );
    rig.shutdown().await;
}
