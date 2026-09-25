//! Supervisor tests, plus the stub core they spawn.
//!
//! Unix-only, and deliberately so. The stub is this same test binary,
//! re-invoked; `Supervisor` always spawns its child as
//! `<exe> <role> --ipc <path>`, which would otherwise collide with the
//! standard test harness parsing `--ipc` as an unrecognized flag. To dodge
//! that, each `stub_exe*` helper hands back a symlink to this binary whose
//! *name* carries the desired behaviour (`pheme-core-stub-…`), and a tiny
//! hand-written `.init_array` constructor — an ELF pre-main hook run by the
//! C runtime before Rust's own `main`, well before the test harness gets
//! anywhere near `argv` — checks that name and, if it matches, runs the
//! stub core and exits instead of ever reaching the harness. A plain
//! `cargo test` invocation's argv0 never matches the prefix, so normal test
//! runs are unaffected.
//!
//! `.init_array` has no equivalent on the `windows-msvc` target this crate
//! also ships for; its analogue would be `.CRT$XCU`, and building that
//! second interception path (plus a non-Unix way to hand back a
//! distinguishable "executable") is not worth maintaining for a fixture.
//! The supervisor itself still compiles for Windows through the lib target;
//! what this file does not cover there is its *behaviour*, which rests on
//! manual rows G2, G6 and G7 like everything else Windows-specific in this
//! crate.
//!
//! The stub itself is an honest, minimal imitation of the real core: it
//! connects to the socket named by `--ipc`, sends a `Status` every 200 ms,
//! watches for the connection closing (`recv_command` returning `Ok(None)`),
//! and exits the moment either happens — the same lifetime rule
//! `CoreLink::recv_command`'s own docs describe for the real thing.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use pheme_app::config::{Config, Role};
use pheme_app::frontend::{CoreState, Supervisor};
use pheme_app::ipc::{CoreLink, LinkState, Status};

// --- The pre-main stub intercept -------------------------------------------

#[used]
#[link_section = ".init_array"]
static STUB_INTERCEPT: extern "C" fn() = {
    extern "C" fn run() {
        let argv0 = std::env::args().next().unwrap_or_default();
        let name = Path::new(&argv0)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if let Some(mode) = name.strip_prefix("pheme-core-stub-") {
            stub_main(mode);
        }
    }
    run
};

/// Runs the stub core for `mode` and never returns: every path ends the
/// process, so control never reaches the test harness's own `main`.
fn stub_main(mode: &str) -> ! {
    if mode == "normal" {
        let ipc_path = ipc_path_from_argv();
        let rt = tokio::runtime::Runtime::new().expect("stub tokio runtime");
        rt.block_on(run_stub_core(&ipc_path));
        std::process::exit(0);
    } else if let Some(rest) = mode.strip_prefix("exit-") {
        let code: i32 = rest.parse().unwrap_or(1);
        std::process::exit(code);
    } else if let Some(reason) = mode.strip_prefix("fail-") {
        // A second front-end whose child cannot bind the port: it says why
        // on stderr and exits nonzero without ever reaching the ipc socket.
        eprintln!("{reason}");
        std::process::exit(1);
    } else {
        eprintln!("pheme test stub: unrecognized mode {mode:?}");
        std::process::exit(111);
    }
}

fn ipc_path_from_argv() -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--ipc")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .expect("stub invoked without --ipc <path>")
}

async fn run_stub_core(ipc_path: &str) {
    let mut link = match CoreLink::connect(ipc_path).await {
        Ok(link) => link,
        Err(_) => return,
    };
    let status = Status {
        role: Role::Server,
        state: LinkState::Connected,
        peer: None,
        rtt_us: 0,
        locked: false,
        events: 0,
        lost: 0,
        audio_depth_ms: 0,
        audio_lost: 0,
        mic_depth_ms: 0,
        mic_lost: 0,
    };
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if link.send_status(&status).await.is_err() {
                    return;
                }
            }
            cmd = link.recv_command() => {
                match cmd {
                    Ok(Some(_)) => {}
                    _ => return,
                }
            }
        }
    }
}

// --- Test helpers ------------------------------------------------------

fn server_config() -> Config {
    Config {
        role: Role::Server,
        ..Config::default()
    }
}

/// One directory, shared by every `make_stub` call in this process, rather
/// than a fresh one per call: a `static` is never dropped, so whatever it
/// holds is leaked regardless, and a directory per call multiplied that leak
/// by every stub any test asked for. One directory bounds it to one leaked
/// directory per test-binary run, holding at most a handful of symlinks —
/// not one that grows with the number of tests or repeat runs.
static STUB_DIR: std::sync::LazyLock<tempfile::TempDir> =
    std::sync::LazyLock::new(|| tempfile::tempdir().expect("tempdir for stub symlinks"));

/// Builds a symlink to this test binary whose name is
/// `pheme-core-stub-<suffix>`, so the constructor above recognizes it when
/// `Supervisor` spawns it. Several tests ask for the same suffix (`"normal"`)
/// and may run concurrently, so a symlink that is already there — created by
/// another test a moment earlier, pointing at the same target — is fine;
/// only some other failure is not.
fn make_stub(suffix: &str) -> PathBuf {
    let link = STUB_DIR.path().join(format!("pheme-core-stub-{suffix}"));
    let target = std::env::current_exe().expect("current_exe");
    match std::os::unix::fs::symlink(&target, &link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => panic!("symlink the stub at {}: {e}", link.display()),
    }
    link
}

fn stub_exe() -> PathBuf {
    make_stub("normal")
}

fn stub_exe_that_exits(code: i32) -> PathBuf {
    make_stub(&format!("exit-{code}"))
}

fn stub_exe_that_fails(msg: &str) -> PathBuf {
    make_stub(&format!("fail-{msg}"))
}

async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// --- The tests -----------------------------------------------------------

#[tokio::test]
async fn with_no_configuration_nothing_is_spawned() {
    // Review Focus 1: the first run, every time, for every new user.
    let s = Supervisor::start(stub_exe(), None).await.unwrap();
    assert!(matches!(s.state(), CoreState::NoConfig));
}

#[tokio::test]
async fn a_running_child_reports_status() {
    let s = Supervisor::start(stub_exe(), Some(server_config()))
        .await
        .unwrap();
    assert!(
        wait_until(
            || matches!(s.state(), CoreState::Running(_)),
            Duration::from_secs(5)
        )
        .await,
        "no status arrived"
    );
}

#[tokio::test]
async fn a_child_that_dies_is_reported_not_waited_on_forever() {
    // Review Focus 2: a crash, a missing uinput permission, a bound port. The
    // front-end must say so rather than wait for a status that never comes.
    let s = Supervisor::start(stub_exe_that_exits(2), Some(server_config()))
        .await
        .unwrap();
    assert!(
        wait_until(
            || matches!(s.state(), CoreState::Stopped(_)),
            Duration::from_secs(5)
        )
        .await,
        "the supervisor never noticed the child had gone"
    );
}

#[tokio::test]
async fn a_child_that_fails_to_start_reports_why() {
    // Review Focus 3: a second front-end, whose child cannot bind the port.
    // The message must reach the caller as text, not vanish into a log.
    let s = Supervisor::start(
        stub_exe_that_fails("address already in use"),
        Some(server_config()),
    )
    .await
    .unwrap();
    assert!(
        wait_until(
            || matches!(s.state(), CoreState::Stopped(m) if m.contains("address already in use")),
            Duration::from_secs(5)
        )
        .await,
        "the reason never reached the supervisor: {:?}",
        s.state()
    );
}

#[tokio::test]
async fn applying_a_configuration_restarts_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut s = Supervisor::start(stub_exe(), Some(server_config()))
        .await
        .unwrap();
    wait_until(
        || matches!(s.state(), CoreState::Running(_)),
        Duration::from_secs(5),
    )
    .await;
    let mut cfg = server_config();
    cfg.name = "renamed".into();
    s.apply_config(cfg.clone(), &path).await.unwrap();
    assert_eq!(
        Config::load(Some(&path)).unwrap(),
        cfg,
        "the file was not written"
    );
    assert!(
        wait_until(
            || matches!(s.state(), CoreState::Running(_)),
            Duration::from_secs(5)
        )
        .await,
        "the child did not come back"
    );
}

#[tokio::test]
async fn restart_respawns_from_the_held_config_without_a_path() {
    // Starting a child back up is not a configuration change: `restart`
    // takes no `path` at all, so there is structurally nothing it could
    // write to, unlike `apply_config`.
    let mut s = Supervisor::start(stub_exe(), Some(server_config()))
        .await
        .unwrap();
    wait_until(
        || matches!(s.state(), CoreState::Running(_)),
        Duration::from_secs(5),
    )
    .await;
    let first_pid = s.child_pid().expect("a running child");
    s.shutdown().await;
    assert!(
        !process_exists(first_pid),
        "shutdown left the first child behind"
    );

    s.restart().await.unwrap();
    assert!(
        wait_until(
            || matches!(s.state(), CoreState::Running(_)),
            Duration::from_secs(5)
        )
        .await,
        "the child did not come back after restart"
    );
}

#[tokio::test]
async fn restarting_with_no_configuration_is_a_harmless_no_op() {
    let mut s = Supervisor::start(stub_exe(), None).await.unwrap();
    assert!(matches!(s.state(), CoreState::NoConfig));
    s.restart().await.unwrap();
    assert!(matches!(s.state(), CoreState::NoConfig));
}

#[tokio::test]
async fn shutdown_leaves_no_child_behind() {
    let mut s = Supervisor::start(stub_exe(), Some(server_config()))
        .await
        .unwrap();
    wait_until(
        || matches!(s.state(), CoreState::Running(_)),
        Duration::from_secs(5),
    )
    .await;
    let pid = s.child_pid().expect("a running child");
    s.shutdown().await;
    assert!(!process_exists(pid), "pid {pid} survived shutdown");
}
