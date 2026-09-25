//! The client's half of monitor input switching (task 9), over a real QUIC
//! link to a stand-in server and a `MockMonitor`.
//!
//! `display_server.rs` pins the server's half the same way, and its own
//! header explains why: a real client had nothing to test against until
//! this half existed, because `run_client` still declared `display_input:
//! None` in its `Hello`. Now the situation is mirrored -- a real client's
//! own `Hello`/`HelloAck`/`Msg::Leave`/`Msg::SwitchDisplay` handling is
//! exercised against a hand-rolled server, the same trade `display_server.rs`
//! makes in the other direction.

use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::config::DisplayCfg;
use pheme_app::display::DisplayService;
use pheme_app::target::Target;
use pheme_display::mock::{MockMonitor, MockMonitorHandle};
use pheme_input::mock::MockInject;
use pheme_net::{Endpoint, Identity, Incoming, Peer, TrustStore};
use pheme_proto::{AudioParams, Msg, ScreenInfo, PROTOCOL_VERSION};
use tokio::sync::watch;

/// The input this machine (the client under test) is cabled to.
const CLIENT_INPUT: u16 = 0x0f;
/// The input the stand-in server declares in its `HelloAck`. Deliberately
/// different from `CLIENT_INPUT`, for the same reason `display_server.rs`
/// keeps its two constants apart: a test that used one value for both sides
/// would pass even if the code reached for the wrong one.
const SERVER_INPUT: u16 = 0x11;
/// A third value, distinct from both, so a `SwitchDisplay` test cannot be
/// satisfied by code that actually ran the `Msg::Leave` handling instead.
const OTHER_INPUT: u16 = 0x12;

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

/// A real `run_client` with monitor switching configured, connected to a
/// stand-in server the test drives by hand.
struct Rig {
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    mon: MockMonitorHandle,
    /// Held, not dropped: dropping it closes the connection and the client
    /// would tear the session down underneath the test.
    peer: Peer,
}

impl Rig {
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.client.await;
    }
}

/// Spawns a real client and completes the handshake by hand, asserting on
/// the way that the client's `Hello` names `CLIENT_INPUT` -- step 1 of task
/// 9 -- and handing back `SERVER_INPUT` in the `HelloAck`, which the client
/// must keep for the crossing-back hook (step 2).
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

    let (inject, _inj) = MockInject::new(screens(1000, 500));
    // The monitor starts on this machine's own input, which is what it
    // really shows before any crossing happens.
    let (monitor, mon) = MockMonitor::new("MOCK", "mock", CLIENT_INPUT);
    let display = DisplayService::spawn(
        &DisplayCfg {
            input: Some(CLIENT_INPUT),
            monitor: None,
            // No cooldown: the policy's holding behaviour has its own tests
            // in `display.rs`, and a real one would only make these wait.
            cooldown_ms: 0,
        },
        Box::new(move || Ok(Box::new(monitor))),
    )
    .expect("the feature is configured on");

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
            display: Some(display),
            display_input: Some(CLIENT_INPUT),
            ipc: None,
        },
        shutdown_tx.clone(),
        shutdown_rx,
    ));

    let Ok(Incoming::Peer(mut peer)) = server_ep.accept().await else {
        panic!("the client never connected");
    };
    let mut rx = peer.take_incoming();
    match rx.recv().await {
        Some(Msg::Hello { display_input, .. }) => assert_eq!(
            display_input,
            Some(CLIENT_INPUT),
            "the client must tell the server which input it is cabled to, or the \
             server can never switch the screen to it"
        ),
        other => panic!("expected Hello, got {other:?}"),
    }
    peer.sender()
        .send_control(&Msg::HelloAck {
            version: PROTOCOL_VERSION,
            name: "server".into(),
            audio: AudioParams::DEFAULT,
            display_input: Some(SERVER_INPUT),
        })
        .await
        .expect("sending HelloAck");

    Rig {
        client,
        shutdown_tx,
        mon,
        peer,
    }
}

/// The hook in the `Msg::Leave` branch, end to end: the pointer crossing
/// back to the server must switch the client's own monitor to the input the
/// server declared in `HelloAck`.
///
/// Break it by deleting the `switch_to` call beside the clipboard's (see the
/// mutation this test was checked against, recorded in the task report):
/// the monitor stays on `CLIENT_INPUT` and the person watches the server's
/// screen come up on a monitor still cabled to the client. Break it just as
/// well by reaching for this machine's own `display_input` instead of the
/// server's: the value asserted is the server's, and this machine's is
/// different (`CLIENT_INPUT != SERVER_INPUT`), so a value already equal to
/// the monitor's current input would be silently deduped and the test would
/// still fail, for a related but distinguishable reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_back_commands_the_monitor_to_the_servers_input() {
    let rig = spawn_rig().await;
    rig.peer
        .sender()
        .send_control(&Msg::Leave { seq: 1 })
        .await
        .expect("sending Leave");
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == SERVER_INPUT, Duration::from_secs(5)).await,
        "the monitor was left on {:#04x}, not the server's {SERVER_INPUT:#04x}",
        mon.input()
    );
    rig.shutdown().await;
}

/// The other end of the hotkey (task 8's `Msg::SwitchDisplay`, received
/// here rather than sent): a peer that cannot tell which input the monitor
/// is listening to asks every machine to select its own, and this machine
/// must force its own monitor to whatever value arrives, bypassing the
/// dedupe/cooldown policy that an ordinary crossing goes through.
///
/// Break it by deleting the `force` call in the client's `SwitchDisplay`
/// handling: the request is received and silently dropped, which is the
/// case the recovery hotkey exists to prevent -- a screen stuck on the
/// wrong machine with no way back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_display_from_the_server_reaches_the_monitor() {
    let rig = spawn_rig().await;
    assert_eq!(rig.mon.input(), CLIENT_INPUT);
    rig.peer
        .sender()
        .send_control(&Msg::SwitchDisplay { input: OTHER_INPUT })
        .await
        .expect("sending SwitchDisplay");
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == OTHER_INPUT, Duration::from_secs(5)).await,
        "the server's request never reached the monitor; it was left on {:#04x}",
        mon.input()
    );
    rig.shutdown().await;
}
