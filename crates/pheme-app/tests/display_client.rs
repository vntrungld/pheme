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
use pheme_app::ipc::{Command, IpcConnection, IpcListener};
use pheme_app::target::Target;
use pheme_display::mock::{opens_once, MockMonitor, MockMonitorHandle};
use pheme_input::mock::{InjectCall, MockInject, MockInjectLog};
use pheme_net::{Endpoint, Identity, Incoming, Peer, TrustStore};
use pheme_proto::{AudioParams, Modifiers, Msg, ScreenInfo, PROTOCOL_VERSION};
use tokio::sync::{mpsc, watch};

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
    /// Control messages from the client, already taken from `peer`.
    rx: mpsc::Receiver<Msg>,
    /// The front-end's side of the IPC socket, for the tests that click a
    /// button rather than move a pointer. `None` unless the rig was asked
    /// for one, because binding a socket per test is not free.
    gui: Option<IpcConnection>,
    /// What the client injected. The session and the IPC command loop are
    /// separate tasks reading separate sockets, so a command sent from the
    /// test can overtake a `Msg` sent from the test; this is how a test
    /// waits for the session to have caught up first.
    inj: MockInjectLog,
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
    rig(false).await
}

/// As `spawn_rig`, but with a front-end attached, so a test can send the
/// `Command::SwitchDisplay` the window's button and the tray's item both
/// send.
async fn spawn_rig_with_gui() -> Rig {
    rig(true).await
}

async fn rig(gui: bool) -> Rig {
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

    let (inject, inj) = MockInject::new(screens(1000, 500));
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
        Box::new(opens_once(monitor)),
    )
    .expect("the feature is configured on");

    // Bound before the client is spawned: `run_client` connects to this
    // socket as its first act when it is given one.
    let mut listener = match gui {
        true => Some(IpcListener::bind().await.expect("binding the ipc socket")),
        false => None,
    };
    let ipc = listener
        .as_ref()
        .map(|l| std::path::PathBuf::from(l.path().to_string()));

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
            ipc,
        },
        shutdown_tx.clone(),
        shutdown_rx,
    ));
    let gui = match listener.as_mut() {
        Some(l) => Some(l.accept().await.expect("the client never attached")),
        None => None,
    };

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
        rx,
        gui,
        inj,
    }
}

/// Reads until a `SwitchDisplay` arrives, ignoring the handshake and status
/// traffic around it.
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

/// The front-end's "Switch display", on the client.
///
/// The window, the tray and `Command::SwitchDisplay`'s own documentation
/// all say the same thing: it re-asserts the input of whichever machine
/// holds the pointer, and does what the hotkey does. What it used to do was
/// force the *server's* input unconditionally, so a click while the pointer
/// was on this machine threw the picture over to the machine the person was
/// not using, and the server was never asked at all.
///
/// Break it by reading `server_input` directly instead of
/// `SwitchDisplayState::target` -- that is the old behaviour, and the first
/// pair of assertions fails. Break it by dropping the `send_control`, and
/// both `next_switch_display` calls time out: only the machine the monitor
/// is listening to can act, and this end can never know which one that is.
///
/// The two halves ask for different values, so neither can pass for the
/// other's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gui_switch_asks_for_the_machine_that_holds_the_pointer() {
    let mut rig = spawn_rig_with_gui().await;

    // The pointer is here: the target is this machine's own input.
    rig.peer
        .sender()
        .send_control(&Msg::Enter {
            seq: 1,
            x: 10,
            y: 20,
            mods: Modifiers(0),
        })
        .await
        .expect("sending Enter");
    // The session task and the IPC command loop read different sockets,
    // so the command can overtake the `Enter` unless the test waits: the
    // injected warp is the client session having processed it.
    assert!(
        wait_until(
            || rig.inj.calls().contains(&InjectCall::MoveAbs(10, 20)),
            Duration::from_secs(5)
        )
        .await,
        "the client never acted on the Enter"
    );
    let gui = rig.gui.as_mut().expect("the rig was asked for a front-end");
    gui.send_command(Command::SwitchDisplay)
        .await
        .expect("sending the command");
    assert_eq!(
        next_switch_display(&mut rig.rx).await,
        CLIENT_INPUT,
        "with the pointer here, both machines must be asked for this machine's input"
    );
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == CLIENT_INPUT, Duration::from_secs(5)).await,
        "the monitor was left on {:#04x}, not this machine's {CLIENT_INPUT:#04x}",
        mon.input()
    );

    // And back: the pointer is on the server, so the target is the
    // server's input.
    rig.peer
        .sender()
        .send_control(&Msg::Leave { seq: 2 })
        .await
        .expect("sending Leave");
    // Same race, and this time the crossing back has a visible effect of
    // its own: the client commands its own monitor to the server's input.
    let mon = rig.mon.clone();
    assert!(
        wait_until(|| mon.input() == SERVER_INPUT, Duration::from_secs(5)).await,
        "the client never acted on the Leave"
    );
    let gui = rig.gui.as_mut().expect("the rig was asked for a front-end");
    gui.send_command(Command::SwitchDisplay)
        .await
        .expect("sending the command");
    assert_eq!(
        next_switch_display(&mut rig.rx).await,
        SERVER_INPUT,
        "with the pointer on the server, both machines must be asked for the server's input"
    );
    rig.shutdown().await;
}

/// A link that dies while the pointer is on this machine.
///
/// The `Msg::Bye` path has a deliberate argument for not switching. The
/// silent drop never had one: the server's core returns to local the
/// instant the connection goes, the server cannot command the monitor
/// because it is not the input being displayed, and the client used to
/// just break out to reconnect -- leaving the picture on the machine the
/// pointer is not on, with nobody able to move it but the recovery hotkey.
///
/// Break it by deleting the `stranded` block after `session` returns: the
/// monitor stays on `CLIENT_INPUT`.
///
/// Nothing else could satisfy this: no `Leave` is ever sent, so the
/// crossing-back hook does not run, and a reconnect commands nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_that_drops_while_the_pointer_is_here_brings_the_picture_back() {
    let rig = spawn_rig().await;
    rig.peer
        .sender()
        .send_control(&Msg::Enter {
            seq: 1,
            x: 10,
            y: 20,
            mods: Modifiers(0),
        })
        .await
        .expect("sending Enter");
    assert!(
        wait_until(
            || rig.inj.calls().contains(&InjectCall::MoveAbs(10, 20)),
            Duration::from_secs(5)
        )
        .await,
        "the client never acted on the Enter"
    );
    assert_eq!(rig.mon.input(), CLIENT_INPUT, "the pointer is here");

    // The link dies the way a network failure kills it: no Bye, no Leave,
    // just a connection that is gone.
    let Rig {
        client,
        shutdown_tx,
        mon,
        peer,
        ..
    } = rig;
    drop(peer);

    assert!(
        wait_until(|| mon.input() == SERVER_INPUT, Duration::from_secs(10)).await,
        "the picture was left on {:#04x}, on a machine the pointer is no longer on",
        mon.input()
    );
    let _ = shutdown_tx.send(true);
    let _ = client.await;
}

/// The exclusion beside it: quitting pheme does not move the picture.
///
/// The person quit on the machine they are looking at. Throwing their
/// screen over to the other one on the way out is not what they asked for,
/// and the shutdown path is the one ending of a session that is not a
/// failure to recover from.
///
/// Break it by dropping the `!*shutdown.borrow()` from `stranded`.
///
/// A negative assertion, and bounded by time rather than by an event,
/// which the rest of this file avoids: there is no later effect to wait
/// for once the client has exited. It is still exact in one direction --
/// `Rig::shutdown` awaits the client task, so the switch, if the code made
/// one, was already queued before the window opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutting_down_with_the_pointer_here_leaves_the_picture_alone() {
    let rig = spawn_rig().await;
    rig.peer
        .sender()
        .send_control(&Msg::Enter {
            seq: 1,
            x: 10,
            y: 20,
            mods: Modifiers(0),
        })
        .await
        .expect("sending Enter");
    assert!(
        wait_until(
            || rig.inj.calls().contains(&InjectCall::MoveAbs(10, 20)),
            Duration::from_secs(5)
        )
        .await,
        "the client never acted on the Enter"
    );
    let mon = rig.mon.clone();
    rig.shutdown().await;
    assert!(
        !wait_until(|| mon.input() != CLIENT_INPUT, Duration::from_millis(300)).await,
        "quitting moved the picture to {:#04x}",
        mon.input()
    );
}
