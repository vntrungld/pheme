//! Both machines, one monitor, four crossings.
//!
//! Every other test in this sub-project drives one machine against a mock
//! the other machine cannot touch, and that is exactly what hid the defect
//! this file exists for: the monitor is a resource **both** machines
//! command, so each machine's belief about what is on screen goes stale
//! the moment the other one switches it. One crossing never shows it. The
//! second one does.
//!
//! So this runs a real `run_server` and a real `run_client` over a real
//! QUIC link, with one shared `MockMonitor` behind both `DisplayService`s,
//! and asserts what the monitor is showing after each of four transitions:
//! out, back, out, back.
//!
//! What the mock cannot model is the asymmetry of the hardware -- that a
//! command from the machine which is *not* displayed would be ignored (see
//! design section 2). It does not need to: every command here is issued by
//! the machine that is on screen at that instant, which is the whole point
//! of the design, and a command the design says should not have been
//! issued would land on the mock and be caught by the very next assertion.

use std::time::{Duration, Instant};

use pheme_app::client::{run_client, ClientDeps};
use pheme_app::config::DisplayCfg;
use pheme_app::display::DisplayService;
use pheme_app::server::{run_server, ServerDeps};
use pheme_app::target::Target;
use pheme_core::{CaptureEvent, ClientPlacement, Hotkeys, Side};
use pheme_display::mock::{opens_once, MockMonitor, MockMonitorHandle};
use pheme_input::mock::{MockCapture, MockCaptureHandle, MockInject};
use pheme_input::CaptureMode;
use pheme_net::{Endpoint, Identity, TrustStore};
use pheme_proto::ScreenInfo;
use tokio::sync::watch;

/// The input the server is cabled to, and what the monitor shows at the
/// start -- which is what it really shows while the pointer is on the
/// server.
const SERVER_INPUT: u16 = 0x11;
/// The input the client is cabled to. Different from the server's, because
/// two machines naming the same input is the misconfiguration
/// `display_input_conflict` warns about and would make every assertion here
/// pass for the wrong reason.
const CLIENT_INPUT: u16 = 0x0f;

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

/// A paired server and client, both with monitor switching configured over
/// one shared mock monitor.
struct Pair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    mon: MockMonitorHandle,
}

impl Pair {
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.client.await;
        let _ = self.server.await;
    }
}

fn display_service(input: u16, mon: MockMonitor, cooldown_ms: u64) -> DisplayService {
    DisplayService::spawn(
        &DisplayCfg {
            input: Some(input),
            monitor: None,
            cooldown_ms,
        },
        Box::new(opens_once(mon)),
    )
    .expect("the feature is configured on")
}

fn spawn_pair() -> Pair {
    // No cooldown by default: the policy's holding behaviour has its own
    // tests, and a real one would only make these wait.
    spawn_pair_with_cooldowns(0, 0)
}

fn spawn_pair_with_cooldowns(server_cooldown_ms: u64, client_cooldown_ms: u64) -> Pair {
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
    let (inject, _inj) = MockInject::new(screens(1000, 500));

    // One monitor, two machines. `MockMonitorHandle::monitor` hands out a
    // second view of the same state; giving each service its own mock is
    // precisely the mistake that made this defect invisible.
    let (server_monitor, mon) = MockMonitor::new("MOCK", "mock", SERVER_INPUT);
    let client_monitor = mon.monitor("MOCK", "mock");

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
            display: Some(display_service(
                SERVER_INPUT,
                server_monitor,
                server_cooldown_ms,
            )),
            display_input: Some(SERVER_INPUT),
            ipc: None,
        },
        shutdown_tx.clone(),
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
            display: Some(display_service(
                CLIENT_INPUT,
                client_monitor,
                client_cooldown_ms,
            )),
            display_input: Some(CLIENT_INPUT),
            ipc: None,
        },
        shutdown_tx.clone(),
        shutdown_rx,
    ));
    Pair {
        server,
        client,
        shutdown_tx,
        cap,
        mon,
    }
}

/// Pushes a right-edge crossing the way the mock OS would report it.
fn push_edge_crossing(cap: &MockCaptureHandle) {
    cap.push(CaptureEvent::MotionAbs { x: 1900, y: 540 });
    cap.push(CaptureEvent::MotionAbs { x: 1919, y: 540 });
}

/// Crosses to the client and waits for the core to have grabbed. The first
/// call also waits for the client to be connected, which is what makes a
/// crossing possible at all.
async fn cross_out(cap: &MockCaptureHandle) {
    assert!(
        wait_until(|| cap.is_started(), Duration::from_secs(5)).await,
        "capture started"
    );
    assert!(
        wait_until(
            || {
                push_edge_crossing(cap);
                cap.mode() == CaptureMode::Grab
            },
            Duration::from_secs(10),
        )
        .await,
        "the pointer never crossed to the client"
    );
}

/// Walks the virtual pointer off the client's left edge, which is how a
/// person comes back.
async fn cross_back(cap: &MockCaptureHandle) {
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

async fn expect_monitor(mon: &MockMonitorHandle, want: u16, what: &str) {
    assert!(
        wait_until(|| mon.input() == want, Duration::from_secs(5)).await,
        "{what}: the monitor was left on {:#04x}, not {want:#04x}",
        mon.input()
    );
}

/// The test this whole sub-project turns on: the picture follows the
/// pointer *every* time, not once.
///
/// Break it by deleting the `became_displayed` call in `client.rs`'s
/// `Msg::Enter` arm: the client never learns that the server switched the
/// monitor to the client's own input, so it still believes the monitor is
/// showing `SERVER_INPUT`, and rule 1 silently refuses its own correct
/// command on the way back. The failure is at transition 2, the first
/// crossing back.
///
/// Break it instead by deleting the `became_displayed` call in
/// `server.rs`'s `Msg::Leave` arm and the failure moves to transition 3:
/// the server still believes the monitor shows `CLIENT_INPUT` from the
/// first crossing, so the second crossing out is refused by the same rule.
///
/// Neither break is visible before transition 2, which is why every test
/// written before this one passed with both of them in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_picture_follows_the_pointer_across_repeated_crossings() {
    let pair = spawn_pair();

    cross_out(&pair.cap).await;
    expect_monitor(&pair.mon, CLIENT_INPUT, "first crossing out").await;

    cross_back(&pair.cap).await;
    expect_monitor(&pair.mon, SERVER_INPUT, "first crossing back").await;

    cross_out(&pair.cap).await;
    expect_monitor(&pair.mon, CLIENT_INPUT, "second crossing out").await;

    cross_back(&pair.cap).await;
    expect_monitor(&pair.mon, SERVER_INPUT, "second crossing back").await;

    pair.shutdown().await;
}

/// The held hand-away, end to end, at a cooldown a person would really
/// configure.
///
/// Rule 3 holds a crossing made inside the cooldown and hands it out when
/// the cooldown ends. If the pointer has come home by then, that held
/// value is a command to throw the picture at the machine the pointer left.
/// `poll`'s own discard clause cannot catch it, because `selected` names
/// this machine and `pending` names the peer, so the two are never equal in
/// this direction.
///
/// Break it by deleting `self.pending = None` from `DisplaySwitch::observe`
/// (`pheme-display/src/switch.rs`): the server's held crossing comes due
/// after the pointer is home, the monitor goes to the client, and the last
/// assertion fails. Nothing corrects it afterwards -- the client's cooldown
/// is zero here on purpose, so its own commands are never held and cannot
/// heal the server's mistake and hide it.
///
/// The precondition is asserted rather than assumed: every crossing has to
/// happen inside the server's cooldown for anything to be held at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_held_by_the_cooldown_is_dropped_when_the_pointer_comes_home() {
    const COOLDOWN_MS: u64 = 2_000;
    let pair = spawn_pair_with_cooldowns(COOLDOWN_MS, 0);

    let t0 = Instant::now();
    cross_out(&pair.cap).await;
    expect_monitor(&pair.mon, CLIENT_INPUT, "first crossing out").await;
    cross_back(&pair.cap).await;
    expect_monitor(&pair.mon, SERVER_INPUT, "first crossing back").await;
    // Inside the server's cooldown, so this one is held rather than issued.
    cross_out(&pair.cap).await;
    // And home again before it comes due.
    cross_back(&pair.cap).await;
    expect_monitor(&pair.mon, SERVER_INPUT, "second crossing back").await;
    assert!(
        t0.elapsed() < Duration::from_millis(COOLDOWN_MS),
        "the crossings must all fall inside one cooldown for anything to be held"
    );

    // Past the point where a held command would come due.
    tokio::time::sleep(Duration::from_millis(COOLDOWN_MS + 500) - t0.elapsed()).await;
    assert_eq!(
        pair.mon.input(),
        SERVER_INPUT,
        "the picture was thrown to the client after the pointer had come home"
    );
    pair.shutdown().await;
}
