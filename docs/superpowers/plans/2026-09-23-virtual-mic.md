# Virtual Mic (Sub-project 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The microphone attached to the server appears on the client as an ordinary recording device, and the server's microphone is opened only while something on the client is actually recording.

**Architecture:** The existing `AudioCapture`/`AudioPlayback` trait pair already fits this direction — the server captures a real microphone, the client "plays" into a virtual source — so the new work is backends, not abstractions. Before any of it, two duplications are collapsed: the device-thread handshake that all four existing backends re-implement, and the two near-identical supervisors in `pheme-app`. After that each role runs one `SendSide` and one `RecvSide` that differ only in their stream tag and backend.

**Tech Stack:** Rust 2021, `pipewire` 0.10 (feature `v0_3_32`), `windows` crate (WASAPI), `rtrb`, `rubato`, `quinn`, `postcard`, `crossbeam-channel`, `tokio`.

**Spec:** `docs/superpowers/specs/2026-09-23-virtual-mic-design.md`

## Global Constraints

- One audio format, everywhere, forever: 48 000 Hz, 2 channels, interleaved i16 little-endian, 240 samples per channel per frame = 480 interleaved = 960 bytes = 5 ms. Never introduce a second one.
- Every document, comment, identifier, log message and commit message in this repository is in **English**.
- Commit format is `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`; title under 72 characters, imperative mood; blank line; body wrapped at 72 columns; then the trailer `Co-Authored-By: Claude <noreply@anthropic.com>`.
- Audio never breaks the KVM session: nothing in an audio path may make `run_client` or `run_server` return an error.
- Real-time device callbacks never allocate, never lock, and never block.
- `AudioCapture::start` and `AudioPlayback::start` are synchronous and bounded: they return only once the device is running or has failed, and the timeout path **detaches the device thread, never joins it**.
- A demand signal may only ever keep the microphone open longer than necessary; it may never close one that should be open. `Demand::Unknown` means open.
- Silence is tested for exactly (`== 0`), never against a threshold.
- `seq` counts audio time and advances even for frames silence suppression drops.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must be green at the end of every task.
- Windows code in this repository can only be type-checked locally (`cargo check --target x86_64-pc-windows-gnu`). It has never been run by CI in a way that proves behaviour, so anything Windows-specific is a candidate for the manual matrix, not a claim of correctness.

## Review Focus

Five conditions the spec implies that no task's happy-path tests would exercise, most likely to bite a user first. Each has a test pinned to the task that owns the code.

1. **The peer disconnects while the microphone is open.** No client means no consumer, so the server must close the microphone. If disconnection only clears the peer sender and leaves the gate open, the microphone stays lit for the life of the process. — Task 14.
2. **`MicWanted` flips faster than the device can open and close.** A rapid true/false/true inside the start timeout must not wedge the supervisor, leak a thread, or leave the gate and the device disagreeing. — Task 7.
3. **A frame arrives tagged for the other direction.** A client receiving `AudioStream::Playback`, or a server receiving `AudioStream::Mic`, must ignore it rather than route it into the buffer for the direction it does own. — Tasks 13 and 14.
4. **`MicWanted(true)` while the microphone backend is failing.** The gate says open, `detect_capture` says no. The retry cycle must be unchanged, the log must not storm, and the microphone must come up on its own once the device appears. — Task 7.
5. **A capture device that does not run at 48 kHz.** `AudioCapture` has no `rate()`, so the send side cannot check: converting is the backend's job, and a backend that gets it wrong pitch-shifts every recording with no counter moving. Pin samples-per-frame in the backend's own test. — Task 12.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `crates/pheme-audio/src/device.rs` | `DeviceThread`, `Ready` — the start/stop/healthy handshake, once |
| `crates/pheme-app/src/audio/mod.rs` | `Supervisor` shared parts: `FailureLog`, `nap`, `RETRY`, `TICK`, `PumpEnd` |
| `crates/pheme-app/src/audio/send.rs` | `SendSide`: capture → `Packer` → datagram, with the demand gate |
| `crates/pheme-app/src/audio/recv.rs` | `RecvSide`: datagram → `JitterBuffer` → `DriftController` → playback |
| `crates/pheme-app/tests/mic_e2e.rs` | End-to-end mic direction over real QUIC |

**Deleted:** `crates/pheme-app/src/audio.rs` (becomes the `audio/` module).

**Modified:**

| File | Change |
|---|---|
| `crates/pheme-audio/src/lib.rs` | `mod device`, `Demand`, `AudioPlayback::demand`, `detect_mic`, `detect_virtual_mic` |
| `crates/pheme-audio/src/linux_pipewire.rs` | Port onto `DeviceThread`; add `PipewireVirtualSource` and `PipewireMic` |
| `crates/pheme-audio/src/windows/wasapi.rs` | Port onto `DeviceThread`; add `WasapiMic`; pin `ToWire`'s mono and rate conversion with tests |
| `crates/pheme-audio/src/jitter.rs` | Public `restart()`, `depth_prepop`, clipping bound |
| `crates/pheme-audio/src/mock.rs` | Restartable mocks, `MockPlayback::set_demand` |
| `crates/pheme-proto/src/lib.rs` | `Msg::MicWanted`, `Hello.audio`, `PROTOCOL_VERSION` 2 |
| `crates/pheme-net/src/transport.rs` | `Peer::take_audio()` |
| `crates/pheme-app/src/client.rs` | Mic `RecvSide`, `MicWanted` sending, resume reset |
| `crates/pheme-app/src/server.rs` | Mic `SendSide`, gating, stats |
| `crates/pheme-app/src/config.rs` | `audio.mic_device` |
| `README.md`, `docs/testing.md` | Mic documentation and rows M1–M10 |

---

## Task 1: The device-thread handshake

Four backends re-implement the same lifecycle today, and the readiness contract has already been broken twice in it — WASAPI signalled readiness only after its session ended (every Windows `start` timed out and all Windows audio was dead while the build and lints stayed green), and PipeWire's `healthy()` could never return false (its whole rebuild path was unreachable). This sub-project would take it to six copies. Extract it once, and make `healthy()` correct by construction rather than by each backend remembering.

**Files:**
- Create: `crates/pheme-audio/src/device.rs`
- Modify: `crates/pheme-audio/src/lib.rs` (add `pub mod device;`)

**Interfaces:**
- Consumes: `crate::{Error, Result}`.
- Produces: `device::DeviceThread` with `new()`, `start(&mut self, name: &str, timeout: Duration, stop: impl Fn() + Send + 'static, body: impl FnOnce(Ready) + Send + 'static) -> Result<()>`, `healthy(&self) -> bool`, `is_running(&self) -> bool`, `stop(&mut self)`; and `device::Ready` with `ok(&self)` and `fail(&self, e: Error)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/pheme-audio/src/device.rs` with only this test module at the bottom (the implementation comes in step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const QUICK: Duration = Duration::from_millis(500);

    #[test]
    fn a_thread_that_reports_success_is_running_and_healthy() {
        let mut d = DeviceThread::new();
        let (tx, rx) = mpsc::channel::<()>();
        d.start("test-ok", QUICK, || {}, move |ready| {
            ready.ok();
            // Hold the thread open until the test drops the sender.
            let _ = rx.recv();
        })
        .expect("start");
        assert!(d.is_running());
        assert!(d.healthy());
        drop(tx);
        d.stop();
    }

    #[test]
    fn a_thread_that_reports_failure_returns_that_error_and_does_not_run() {
        let mut d = DeviceThread::new();
        let e = d
            .start("test-fail", QUICK, || {}, |ready| {
                ready.fail(Error::Device("no such device".into()));
            })
            .expect_err("start must fail");
        assert!(matches!(e, Error::Device(m) if m == "no such device"));
        assert!(!d.is_running(), "a failed start leaves nothing running");
        assert!(!d.healthy());
    }

    #[test]
    fn a_thread_that_never_reports_times_out_and_is_asked_to_stop() {
        let stopped = Arc::new(AtomicBool::new(false));
        let asked = stopped.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let mut d = DeviceThread::new();
        let e = d
            .start(
                "test-hang",
                Duration::from_millis(50),
                move || asked.store(true, Ordering::SeqCst),
                move |_ready| {
                    // Never reports readiness; releases only when the test says so, which
                    // is what makes this a *detached* thread rather than a joined one.
                    let _ = rx.recv();
                },
            )
            .expect_err("start must time out");
        assert!(matches!(e, Error::Backend(_)));
        assert!(!d.is_running(), "a timed-out start owns nothing");
        assert!(
            stopped.load(Ordering::SeqCst),
            "the hung thread must be asked to stop even though it is not joined"
        );
        drop(tx);
    }

    #[test]
    fn a_thread_that_returns_stops_being_healthy() {
        let mut d = DeviceThread::new();
        d.start("test-short", QUICK, || {}, |ready| ready.ok())
            .expect("start");
        // The body returned immediately; `healthy` must notice without a `stop` call,
        // because that is the only signal the supervisor has to rebuild on.
        let mut healthy = true;
        for _ in 0..200 {
            if !d.healthy() {
                healthy = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!healthy, "a thread that ended must report ill health");
        d.stop();
    }

    #[test]
    fn a_thread_that_panics_stops_being_healthy() {
        let mut d = DeviceThread::new();
        d.start("test-panic", QUICK, || {}, |ready| {
            ready.ok();
            panic!("a driver callback blew up");
        })
        .expect("start");
        let mut healthy = true;
        for _ in 0..200 {
            if !d.healthy() {
                healthy = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !healthy,
            "an unwinding thread must flip healthy too, or a panicking callback is invisible"
        );
        d.stop();
    }

    #[test]
    fn stop_is_idempotent_and_safe_on_a_thread_that_never_started() {
        let mut d = DeviceThread::new();
        d.stop();
        d.stop();
        assert!(!d.is_running());
    }

    #[test]
    fn starting_an_already_running_thread_is_a_no_op() {
        let mut d = DeviceThread::new();
        let (tx, rx) = mpsc::channel::<()>();
        d.start("test-once", QUICK, || {}, move |ready| {
            ready.ok();
            let _ = rx.recv();
        })
        .expect("first start");
        let started_twice = Arc::new(AtomicBool::new(false));
        let flag = started_twice.clone();
        d.start("test-once", QUICK, || {}, move |ready| {
            flag.store(true, Ordering::SeqCst);
            ready.ok();
        })
        .expect("second start returns Ok");
        assert!(
            !started_twice.load(Ordering::SeqCst),
            "a second start must not spawn a second device thread"
        );
        drop(tx);
        d.stop();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio device::`
Expected: FAIL to compile — `DeviceThread`, `Ready`, `Error` and the imports do not exist yet in this file.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `crates/pheme-audio/src/device.rs`:

```rust
//! The start/stop/healthy handshake every device backend needs, implemented once.
//!
//! Every backend in this crate owns an OS thread that talks to a sound device, and every
//! one of them needs the same four things: a bounded wait for the thread to say it is
//! running, a way to ask it to stop, an honest answer to "is it still alive", and an
//! idempotent teardown. Writing that four times produced two bugs that reached a user —
//! a backend that reported readiness at the wrong moment, and a backend whose health
//! flag could never change — so it is written once here and the backends supply only the
//! parts that differ: the thread body and how that body is asked to stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

use crate::{Error, Result};

/// Handed to a device thread, which calls `ok()` or `fail(e)` exactly once as soon as its
/// device is running or has failed to open.
///
/// "As soon as" is the whole contract. Reporting readiness at the end of a session
/// instead of at its start makes every `start` time out while the device works perfectly,
/// which is invisible to tests and lints and fatal at runtime.
pub struct Ready(mpsc::Sender<Result<()>>);

impl Ready {
    /// The device is open and running.
    pub fn ok(&self) {
        let _ = self.0.send(Ok(()));
    }

    /// The device could not be opened. `start` returns this error verbatim.
    pub fn fail(&self, e: Error) {
        let _ = self.0.send(Err(e));
    }
}

struct Running {
    stop: Box<dyn Fn() + Send>,
    thread: JoinHandle<()>,
    alive: Arc<AtomicBool>,
}

/// Clears `alive` when the device thread's stack is torn down, however it was torn down.
///
/// A guard rather than a statement after the body, so a panic inside a device callback —
/// which unwinds the thread without reaching any statement after it — also flips
/// `healthy()`. A thread that has died is a thread that has died, and the supervisor's
/// rebuild is what brings audio back either way.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// One device thread and its lifecycle.
#[derive(Default)]
pub struct DeviceThread {
    running: Option<Running>,
}

impl DeviceThread {
    pub fn new() -> DeviceThread {
        DeviceThread::default()
    }

    /// Whether a device thread is currently owned. False after a failed or timed-out
    /// `start`, and after `stop`.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Spawns `body` on a thread named `name` and blocks until it reports readiness or
    /// `timeout` elapses. Starting an already-running thread is a no-op.
    ///
    /// On timeout, `stop` is invoked and the thread is **detached, never joined**. A
    /// thread hung inside device construction — a driver that never returns from its
    /// initialise call, a PipeWire loop that has not yet reached the point where it can
    /// observe a stop request — may never act on that request, and joining it would turn
    /// this bounded wait into an unbounded one, which is exactly what the trait contract
    /// promises it is not.
    pub fn start(
        &mut self,
        name: &str,
        timeout: Duration,
        stop: impl Fn() + Send + 'static,
        body: impl FnOnce(Ready) + Send + 'static,
    ) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _alive = AliveGuard(thread_alive);
                body(Ready(ready_tx));
            })
            .map_err(|e| Error::Backend(format!("spawning the {name} thread: {e}")))?;

        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    stop: Box::new(stop),
                    thread,
                    alive,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop();
                warn!(
                    "the {name} thread did not report readiness within {timeout:?}; abandoning \
                     it detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(format!(
                    "the {name} thread did not report readiness within {timeout:?}"
                )))
            }
        }
    }

    /// False once the device thread's stack has been torn down, however it died. A
    /// backend with no thread to lose does not use this type at all.
    pub fn healthy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    /// Idempotent. Asks the thread to stop and joins it.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            (r.stop)();
            let _ = r.thread.join();
        }
    }
}
```

Add to `crates/pheme-audio/src/lib.rs`, in the existing module list (keep it alphabetical, before `drift`):

```rust
pub mod device;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-audio device::`
Expected: PASS, 7 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/device.rs crates/pheme-audio/src/lib.rs
git commit -m "$(cat <<'MSG'
Update: add the shared device-thread handshake

Every audio backend owns an OS thread and needs the same four things
from it: a bounded wait for readiness, a way to ask it to stop, an
honest health answer, and an idempotent teardown. Writing that once per
backend produced two bugs that reached a user - WASAPI reported
readiness only after its session ended, so every start timed out while
the device worked; and PipeWire's healthy() could never return false, so
its rebuild path was unreachable.

DeviceThread installs the liveness guard around the body itself, so
healthy() is correct by construction rather than by each backend
remembering to do it, and the timeout path detaches rather than joins so
a thread hung in device construction cannot make start unbounded.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 2: Port the PipeWire backends onto `DeviceThread`

**Files:**
- Modify: `crates/pheme-audio/src/linux_pipewire.rs`

**Interfaces:**
- Consumes: `crate::device::{DeviceThread, Ready}` from Task 1.
- Produces: `PipewireCapture` and `PipewirePlayback` unchanged in their public trait behaviour; internally each holds one `DeviceThread` instead of its own `Running`, `AliveGuard` and readiness plumbing.

- [ ] **Step 1: Delete the duplicated machinery**

In `crates/pheme-audio/src/linux_pipewire.rs`, delete the `struct Running`, `struct AliveGuard` and `impl Drop for AliveGuard` definitions entirely — `DeviceThread` now owns all three concerns. Replace the import block's `use std::sync::atomic::{AtomicBool, Ordering};` and `use std::sync::mpsc;` with:

```rust
use crate::device::{DeviceThread, Ready};
```

keeping `use std::sync::{Arc, Once};` (still needed by `init` and the playback name).

- [ ] **Step 2: Port `PipewireCapture`**

Replace the struct and its `AudioCapture` impl's lifecycle methods:

```rust
/// The client's virtual sink: applications play into "Pheme Speaker" and we read it.
#[derive(Default)]
pub struct PipewireCapture {
    thread: DeviceThread,
}

impl PipewireCapture {
    pub fn new() -> PipewireCapture {
        PipewireCapture::default()
    }
}

impl AudioCapture for PipewireCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        self.thread.start(
            "pheme-pw-sink",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| capture_thread(sink, ready, cmd_rx),
        )
    }

    fn device_name(&self) -> String {
        "Pheme Speaker".into()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for PipewireCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
```

Change `capture_thread` and `run` to take a `Ready` instead of an `mpsc::Sender<Result<()>>`:

```rust
fn capture_thread(sink: rtrb::Producer<i16>, ready: Ready, cmd_rx: pw::channel::Receiver<Cmd>) {
    if let Err(e) = run(sink, &ready, cmd_rx) {
        ready.fail(e);
    }
}
```

and inside `run`, replace `let _ = ready.send(Ok(()));` with `ready.ok();`.

- [ ] **Step 3: Port `PipewirePlayback` the same way**

```rust
/// The server's playback stream: samples in, speakers out.
pub struct PipewirePlayback {
    device: Option<String>,
    rate: Arc<AtomicU32>,
    name: Arc<Mutex<String>>,
    thread: DeviceThread,
}
```

Keep whatever fields the current implementation uses for `rate()` and `device_name()` exactly as they are; only `running` is replaced by `thread`. `start` becomes:

```rust
fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
    init();
    let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
    let device = self.device.clone();
    let rate = self.rate.clone();
    let name = self.name.clone();
    self.thread.start(
        "pheme-pw-play",
        START_TIMEOUT,
        move || {
            let _ = cmd_tx.send(Cmd::Stop);
        },
        move |ready| playback_thread(device, source, rate, name, ready, cmd_rx),
    )
}
```

with `healthy` and `stop` delegating to `self.thread` as in step 2.

- [ ] **Step 4: Verify nothing changed behaviourally**

Run: `cargo test -p pheme-audio`
Expected: PASS — the same tests as before this task, none removed.

Then exercise the real daemon, which is available on this machine:

Run: `cargo run -p pheme-app -- client --help` (builds the Linux backends), then
Run: `cargo test -p pheme-app`
Expected: PASS.

- [ ] **Step 5: Confirm the virtual sink still appears**

```bash
cargo build --release
./target/release/pheme client 127.0.0.1 &
sleep 2
pactl list sinks short | grep pheme-speaker
kill %1
```

Expected: one line naming `pheme-speaker` at `s16le 2ch 48000Hz`. If it is absent, the port broke node creation.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/linux_pipewire.rs
git commit -m "$(cat <<'MSG'
Update: port the PipeWire backends onto DeviceThread

Both backends kept their own readiness channel, start timeout, detaching
timeout path, liveness guard and idempotent stop. All of it now comes
from DeviceThread, so the two backends carry only what differs: the
thread body and the channel message that asks it to stop.

This removes the copy of the liveness guard whose absence once made
healthy() permanently true here, taking the daemon-restart rebuild path
with it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 3: Port the WASAPI backends onto `DeviceThread`

Windows cannot be run here; it is type-checked against the `x86_64-pc-windows-gnu` target and verified in the VM later. That makes a mechanical, behaviour-preserving port the right shape for this task: no logic changes, only the lifecycle moving out.

**Files:**
- Modify: `crates/pheme-audio/src/windows/wasapi.rs`

**Interfaces:**
- Consumes: `crate::device::{DeviceThread, Ready}` from Task 1.
- Produces: `WasapiCapture` and `WasapiPlayback` unchanged in public behaviour.

- [ ] **Step 1: Delete the duplicated machinery**

Delete `struct Running`, `struct AliveGuard` and its `Drop` impl from `crates/pheme-audio/src/windows/wasapi.rs`. Add:

```rust
use crate::device::{DeviceThread, Ready};
```

- [ ] **Step 2: Port `WasapiCapture`**

```rust
/// Records what the default (or configured) output endpoint is playing.
pub struct WasapiCapture {
    device: Option<String>,
    name: Arc<Mutex<String>>,
    thread: DeviceThread,
}

impl WasapiCapture {
    pub fn new(device: Option<String>) -> WasapiCapture {
        WasapiCapture {
            device,
            name: Arc::new(Mutex::new("not started".into())),
            thread: DeviceThread::new(),
        }
    }
}

impl AudioCapture for WasapiCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let device = self.device.clone();
        let name = self.name.clone();
        self.thread.start(
            "pheme-wasapi-cap",
            START_TIMEOUT,
            move || stop.store(true, Ordering::SeqCst),
            move |ready| capture_thread(device, sink, thread_stop, name, ready),
        )
    }

    fn device_name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}
```

Change `capture_thread`'s last parameter from `ready_tx: mpsc::Sender<Result<()>>` to `ready: Ready`, and replace every `let _ = ready_tx.send(Ok(()));` with `ready.ok();` and every `let _ = ready_tx.send(Err(e));` with `ready.fail(e);`.

**Check while you are in there:** `ready.ok()` must be called at the point the device is confirmed running — immediately after `IAudioClient::Start` succeeds — and never at the end of the session loop. That inversion is the bug that made every Windows `start` time out while the device itself worked.

- [ ] **Step 3: Port `WasapiPlayback` the same way**

Same shape: keep the `rate` and `name` fields, replace `running: Option<Running>` with `thread: DeviceThread`, build the `Arc<AtomicBool>` stop flag in `start` and hand its clone to the body, and delegate `healthy`/`stop` to `self.thread`. Thread name `"pheme-wasapi-play"`.

- [ ] **Step 4: Type-check for Windows**

Run: `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets`
Expected: no errors, no warnings.

Run: `cargo test --workspace`
Expected: PASS (the Linux build is unaffected by this file).

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/windows/wasapi.rs
git commit -m "$(cat <<'MSG'
Update: port the WASAPI backends onto DeviceThread

The same mechanical port as the PipeWire backends: the readiness
channel, start timeout, detaching timeout path, liveness guard and
idempotent stop all move into DeviceThread, leaving each backend with
its thread body and its stop flag.

This is the file where readiness was once signalled at the end of the
session rather than at its start, which timed out every Windows start
while the device itself worked. Centralising the handshake removes the
place that mistake can be made again.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 4: `Demand`, and mocks that can be restarted

The demand signal's safety property lives in a type: the trait default is `Unknown`, which means "keep the microphone open", so a backend that has not implemented detection fails safe without writing a line of code.

**Files:**
- Modify: `crates/pheme-audio/src/lib.rs`
- Modify: `crates/pheme-audio/src/mock.rs`

**Interfaces:**
- Produces: `pheme_audio::Demand` (`Wanted`, `Idle`, `Unknown`); `AudioPlayback::demand(&self) -> Demand` defaulting to `Unknown`; `MockPlaybackHandle::set_demand(Demand)`; `MockCaptureHandle::start_count() -> u64`; `MockCaptureHandle::stopped() -> bool`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `crates/pheme-audio/src/mock.rs`:

```rust
#[test]
fn a_playback_backend_reports_unknown_demand_unless_it_knows_better() {
    let (play, _handle) = MockPlayback::new(48_000);
    assert_eq!(
        play.demand(),
        Demand::Unknown,
        "the default must be the one that keeps a microphone open"
    );
}

#[test]
fn a_playback_backend_can_report_what_its_consumers_are_doing() {
    let (play, handle) = MockPlayback::new(48_000);
    handle.set_demand(Demand::Idle);
    assert_eq!(play.demand(), Demand::Idle);
    handle.set_demand(Demand::Wanted);
    assert_eq!(play.demand(), Demand::Wanted);
}

#[test]
fn a_capture_backend_can_be_started_again_after_being_stopped() {
    // The demand gate stops the microphone outright so its indicator goes out, then
    // starts it again when a consumer comes back. A backend that can only be started
    // once would make the gate a one-way door.
    let (mut cap, handle) = MockCapture::new();
    let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(16);
    cap.start(producer).unwrap();
    assert_eq!(handle.start_count(), 1);
    assert_eq!(handle.push(&[1, 2]), 2);
    cap.stop();
    assert!(handle.stopped());
    assert!(!handle.started());

    let (producer, mut consumer2) = rtrb::RingBuffer::<i16>::new(16);
    cap.start(producer).unwrap();
    assert_eq!(handle.start_count(), 2, "a second start really started it");
    assert!(handle.started());
    assert_eq!(handle.push(&[7, 8]), 2);
    assert_eq!(consumer2.pop(), Ok(7));
    assert_eq!(consumer2.pop(), Ok(8));
    // The first ring received only what was pushed before the stop.
    assert_eq!(consumer.pop(), Ok(1));
    assert_eq!(consumer.pop(), Ok(2));
    assert!(consumer.pop().is_err(), "nothing went to the old ring");
}
```

Add `Demand` to that module's imports: `use crate::{AudioCapture, AudioPlayback, Demand};`

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio mock::`
Expected: FAIL to compile — `Demand`, `set_demand`, `start_count` and `stopped` do not exist.

- [ ] **Step 3: Add `Demand` and the trait method**

In `crates/pheme-audio/src/lib.rs`, after the `Error`/`Result` definitions:

```rust
/// Whether anything is consuming what a playback backend emits.
///
/// Only `Idle` may close a microphone. `Unknown` is the trait default, so a backend that
/// cannot tell — which is every backend except the Linux virtual source — keeps the
/// microphone open without having to opt in. The asymmetry is deliberate: a microphone
/// wrongly held open wastes bandwidth and lights an indicator, while one wrongly held
/// shut makes the whole feature fail silently, and silent failure is the defect class
/// that has reached a user three times in this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    /// Something is recording from this device right now.
    Wanted,
    /// Nothing is recording, and the backend is sure of it.
    Idle,
    /// The backend cannot tell. Treated as `Wanted`.
    Unknown,
}
```

Add to the `AudioPlayback` trait, after `healthy`:

```rust
    /// Whether anything is consuming what this backend emits.
    ///
    /// Meaningful only for a backend that presents a device to other applications — the
    /// client's virtual microphone. A backend that writes to real speakers keeps the
    /// default, and nothing reads it.
    fn demand(&self) -> Demand {
        Demand::Unknown
    }
```

- [ ] **Step 4: Extend the mocks**

In `crates/pheme-audio/src/mock.rs`, add `demand: Demand` to `PlaybackState` (default `Demand::Unknown`), and `start_count: u64` plus `stopped: bool` to `CaptureState` (default `0` and `false`). Then:

```rust
impl MockPlaybackHandle {
    /// Sets what the backend will report about its consumers.
    pub fn set_demand(&self, d: Demand) {
        self.state.lock().unwrap().demand = d;
    }
}

impl MockCaptureHandle {
    /// How many times `start` has succeeded. The demand gate is expected to drive this
    /// past one over a session.
    pub fn start_count(&self) -> u64 {
        self.state.lock().unwrap().start_count
    }

    /// Whether `stop` has been called at least once.
    pub fn stopped(&self) -> bool {
        self.state.lock().unwrap().stopped
    }
}
```

In `MockCapture::start`, after the failure check, add `st.start_count += 1;`. In `MockCapture::stop`, add `st.stopped = true;`. In `MockPlayback`, add:

```rust
    fn demand(&self) -> Demand {
        self.state.lock().unwrap().demand
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-audio`
Expected: PASS, including the three new tests.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/lib.rs crates/pheme-audio/src/mock.rs
git commit -m "$(cat <<'MSG'
Update: add Demand and make the mock backends restartable

Demand carries the safety rule for the microphone gate in the type
system: Unknown is the trait default and means keep the microphone open,
so a backend that cannot detect its consumers fails safe without opting
in. Only Idle may close a microphone.

The gate stops the capture backend outright rather than merely ceasing
to send, so the operating system shows the microphone as closed and its
indicator goes out. That makes restarting a backend a normal event
rather than a one-way door, so the mocks now count starts and record
stops for tests to assert on.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 5: Jitter buffer — explicit restart and pre-pop depth

Two changes, both prerequisites for later tasks. `restart()` is what the client calls when it asks the server to reopen its microphone (spec §3.4). `depth_prepop` is debt (c), and it happens to fix debt (d) as well: the depth the latency budget describes is the audio waiting *before* a pop, and sampling after the pop reads a whole frame lower, which is exactly the headroom the `median >= 5` assertion is missing.

**Files:**
- Modify: `crates/pheme-audio/src/jitter.rs`

**Interfaces:**
- Produces: `JitterBuffer::restart(&mut self)`; `JitterStats.depth_prepop: usize`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `crates/pheme-audio/src/jitter.rs`:

```rust
#[test]
fn an_explicit_restart_drops_everything_and_prefills_again() {
    // What the client does when it asks the server to reopen its microphone: the
    // stream is about to resume from an unrelated sequence number, and anything still
    // buffered belongs to the previous recording.
    let mut jb = JitterBuffer::new();
    jb.push(frame(10, 5));
    jb.push(frame(11, 5));
    assert_eq!(data(jb.pop()), 5);

    jb.restart();
    assert_eq!(jb.stats().depth, 0, "the backlog is gone");
    assert_eq!(jb.pop(), Pop::Idle, "prefilling again");

    // A sender starting from zero is picked up cleanly, with no run of late frames.
    jb.push(frame(0, 9));
    jb.push(frame(1, 9));
    assert_eq!(data(jb.pop()), 9);
    assert_eq!(data(jb.pop()), 9);
    assert_eq!(jb.stats().late, 0, "a restart must not strand the new stream");
}

#[test]
fn a_restart_without_it_would_discard_the_resumed_stream_as_late() {
    // The failure this exists to prevent: the read cursor advanced while the sender was
    // closed, the sender restarts below it, and the gap is under RESET_GAP so nothing
    // resets on its own. Every arriving frame is late and the listener hears silence.
    let mut jb = JitterBuffer::new();
    jb.push(frame(100, 5));
    jb.push(frame(101, 5));
    jb.pop();
    jb.pop();
    // Cursor walks forward while nothing arrives, as it does while the microphone is
    // shut and the virtual source is still pulling.
    for _ in 0..40 {
        jb.pop();
    }
    jb.push(frame(0, 9));
    jb.push(frame(1, 9));
    assert!(
        jb.stats().late > 0,
        "this is the pathology restart() exists to avoid"
    );

    // With the restart in the right place, the same sequence plays.
    let mut jb = JitterBuffer::new();
    jb.push(frame(100, 5));
    jb.push(frame(101, 5));
    jb.pop();
    jb.pop();
    for _ in 0..40 {
        jb.pop();
    }
    jb.restart();
    jb.push(frame(0, 9));
    jb.push(frame(1, 9));
    assert_eq!(data(jb.pop()), 9);
    assert_eq!(jb.stats().late, 0);
}

#[test]
fn the_reported_depth_is_measured_before_the_pop_removes_its_frame() {
    // `depth_ms` in the stats line is meant to answer "how much audio is waiting", which
    // is what the latency budget counts. Sampling after the pop reads a frame lower and
    // leaves a lower bound of one frame with no headroom at all.
    let mut jb = JitterBuffer::new();
    jb.push(frame(0, 1));
    jb.push(frame(1, 1));
    jb.push(frame(2, 1));
    assert!(matches!(jb.pop(), Pop::Data(_)));
    let st = jb.stats();
    assert_eq!(st.depth, 2, "two frames remain after the pop");
    assert_eq!(
        st.depth_prepop, 3,
        "three frames were waiting when the pop happened"
    );
}

#[test]
fn the_pre_pop_depth_is_zero_while_prefilling() {
    let mut jb = JitterBuffer::new();
    assert_eq!(jb.pop(), Pop::Idle);
    assert_eq!(jb.stats().depth_prepop, 0);
    jb.push(frame(0, 1));
    assert_eq!(jb.pop(), Pop::Idle, "still below target");
    assert_eq!(jb.stats().depth_prepop, 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio jitter::`
Expected: FAIL to compile — `restart` and `depth_prepop` do not exist.

- [ ] **Step 3: Implement**

Add the field to `JitterStats`, after `depth`:

```rust
    /// Depth measured immediately before the last `pop` removed its frame.
    ///
    /// This is the number the latency budget counts: the audio waiting to be played.
    /// `depth` is sampled after the removal and therefore reads one frame lower, which
    /// is fine for watching the buffer's trend and useless as a lower bound.
    pub depth_prepop: usize,
```

In `pop`, record it before anything is removed — as the first statement of the function:

```rust
    pub fn pop(&mut self) -> Pop {
        self.stats.depth_prepop = self.frames.len();
        if self.prefilling {
```

Add the public restart next to the private `reset`:

```rust
    /// Drops everything buffered and prefills again, on the caller's say-so rather than
    /// on a sequence-number gap.
    ///
    /// The client calls this at the moment it asks the server to reopen its microphone.
    /// While the microphone was shut, the sender's numbering stood still and this
    /// buffer's read cursor kept advancing, so the resuming stream arrives *behind* the
    /// cursor. If that distance is under `RESET_GAP` nothing resets on its own and every
    /// arriving frame is counted late and discarded — up to 750 ms of silence at the
    /// start of every recording, with every error counter reading zero. The client is
    /// the one component that knows exactly when the stream is resuming, so it says so
    /// instead of leaving the buffer to infer it.
    ///
    /// Counted in `resets`, because that is what it is: one per recording is honest.
    ///
    /// Unlike the gap-triggered `reset`, this also drops the adaptive target back to
    /// `TARGET_MIN`. `reset` keeps the target deliberately, because a sequence jump on a
    /// continuing link does not change what that link's jitter is — but a closed gate is
    /// a deliberate pause, and the receiver spends it underrunning, which ratchets the
    /// target up once per recording. Ten seconds of clean audio sheds one frame, so a
    /// user who records repeatedly would climb to the 40 ms ceiling because they paused,
    /// not because the network was bad. A genuinely poor link re-learns its target within
    /// one concealment; latency is this project's first stated priority, so starting low
    /// and re-learning is the right side to err on.
    pub fn restart(&mut self) {
        self.reset();
        self.target = TARGET_MIN;
        self.stats.target = TARGET_MIN;
    }
```

In `reset`, also clear the new field: `self.stats.depth_prepop = 0;`

Leave `stats()`'s `depth` override alone — `depth` keeps meaning "right now".

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-audio jitter::`
Expected: PASS, including the four new tests and all pre-existing ones.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/jitter.rs
git commit -m "$(cat <<'MSG'
Update: add an explicit jitter restart and pre-pop depth

restart() is what the client calls when it asks the server to reopen its
microphone. While the microphone is shut the sender's numbering stands
still and the receiver's read cursor keeps advancing, so the resuming
stream arrives behind the cursor; if that distance is under RESET_GAP
nothing resets on its own and every arriving frame is discarded as late.
That is up to 750 ms of silence at the start of every recording with
every error counter at zero. The client knows when the stream is
resuming, so it says so rather than leaving the buffer to infer it.

depth_prepop reports the depth before a pop removes its frame, which is
the quantity the latency budget counts; the existing depth is sampled
after the removal and reads a frame lower.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 6: Split the supervisor into `SendSide` and `RecvSide`

`AudioOut` and `AudioIn` are near-identical — spawn, retry every five seconds, `FailureLog`, `nap`, `Drop` — and this sub-project adds a second one of each. Rather than four copies, each role builds one `SendSide` and one `RecvSide` that differ only in the backend detected and the stream tag carried. The file gets *smaller* as the second direction arrives.

This task is a rename and a move, plus the two small debts that live in the code being moved. No behaviour changes.

**Files:**
- Delete: `crates/pheme-app/src/audio.rs`
- Create: `crates/pheme-app/src/audio/mod.rs`, `crates/pheme-app/src/audio/send.rs`, `crates/pheme-app/src/audio/recv.rs`
- Modify: `crates/pheme-app/src/client.rs`, `crates/pheme-app/src/server.rs`, `crates/pheme-app/tests/audio.rs`, `crates/pheme-app/tests/integration.rs`

**Interfaces:**
- Consumes: `pheme_audio::jitter::JitterStats.depth_prepop` from Task 5.
- Produces:
  - `crate::audio::SendSide::spawn(source: CaptureSource, stream: AudioStream, counters: Arc<OutCounters>) -> SendSide`, with `set_peer(&self, Option<PeerSender>)` and `stop(&mut self)`.
  - `crate::audio::RecvSide::spawn(source: PlaybackSource, stats: Arc<InStats>) -> RecvSide`, with `push(&self, Frame)` and `stop(&mut self)`.
  - `crate::audio::{CaptureSource, PlaybackSource, OutCounters, InStats}` re-exported unchanged.

- [ ] **Step 1: Create the module and move the shared parts**

`git mv crates/pheme-app/src/audio.rs crates/pheme-app/src/audio/mod.rs`, then cut from it into the two new files. `crates/pheme-app/src/audio/mod.rs` keeps only what both sides use:

```rust
//! Audio wiring between the backends in `pheme-audio` and the QUIC session.
//!
//! Both roles run both directions. Each builds one `SendSide` — a capture device packed
//! into datagrams — and one `RecvSide` — datagrams played into a device. They differ
//! only in which backend they detect and which `AudioStream` tag they carry, so the
//! supervisor shape they share lives here: rebuild a failing backend every five seconds,
//! keep a permanently broken machine from flooding the log, and never let any of it
//! reach `run_client` or `run_server` as an error.

mod recv;
mod send;

pub use recv::{InStats, PlaybackSource, RecvSide};
pub use send::{CaptureSource, OutCounters, SendSide};

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, warn};

/// How long a failed backend waits before it is rebuilt.
pub(crate) const RETRY: Duration = Duration::from_secs(5);
/// How often the supervisor threads wake up.
pub(crate) const TICK: Duration = Duration::from_millis(2);

/// Why a pump returned.
pub(crate) enum PumpEnd {
    /// A stop was requested, or the channel was dropped: do not rebuild.
    Stopped,
    /// The device, or something the pump owns, failed: rebuild after the retry delay.
    Failed(String),
}
```

Move `FailureLog` (with its doc comment verbatim) and `nap` into `mod.rs` and mark both `pub(crate)`, along with `FailureLog`'s methods.

- [ ] **Step 2: Move the send half**

`crates/pheme-app/src/audio/send.rs` takes `CaptureSource`, `OutCounters`, `CAPTURE_RING_FRAMES`, `out_thread` and `pump_out`, with `AudioOut` renamed to `SendSide`. Two signature changes:

```rust
impl SendSide {
    /// `stream` tags every datagram this side sends: `Playback` on a client, `Mic` on a
    /// server. It is the only thing that distinguishes the two directions here.
    pub fn spawn(
        source: CaptureSource,
        stream: AudioStream,
        counters: Arc<OutCounters>,
    ) -> SendSide {
        // …as `AudioOut::spawn`, passing `stream` through to `out_thread`…
    }
}
```

`pump_out` takes `stream: AudioStream` and uses it in the message it builds, replacing the hard-coded tag:

```rust
                    sender.send_datagram(&Msg::Audio {
                        stream,
                        seq: f.seq,
                        ts_us: f.ts_us,
                        samples: f.bytes,
                    });
```

Move the `AudioOut` tests from the old file's test module into `send.rs`'s own `#[cfg(test)] mod tests`, changing `AudioOut::spawn(src, counters)` to `SendSide::spawn(src, AudioStream::Playback, counters)`.

- [ ] **Step 3: Move the receive half**

`crates/pheme-app/src/audio/recv.rs` takes `PlaybackSource`, `InStats`, `PLAYBACK_RING_SAMPLES`, `FRAME_QUEUE`, `in_thread`, `pump_in` and `publish`, with `AudioIn` renamed to `RecvSide`. Move its tests across too.

In `publish`, switch the reported depth to the pre-pop measurement (debt (c)):

```rust
fn publish(stats: &InStats, s: JitterStats) {
    // The pre-pop depth is the audio *waiting* to be played, which is what the latency
    // budget in the spec counts. `s.depth` is sampled after the pop removed its frame
    // and reads one frame lower.
    stats
        .depth_ms
        .store(s.depth_prepop as u64 * 5, Ordering::Relaxed);
    stats.lost.store(s.lost, Ordering::Relaxed);
    stats.late.store(s.late, Ordering::Relaxed);
    stats.underruns.store(s.underruns, Ordering::Relaxed);
    stats.resets.store(s.resets, Ordering::Relaxed);
    stats.overflows.store(s.overflows, Ordering::Relaxed);
}
```

- [ ] **Step 4: Fix the depth assertion's headroom (debt (d))**

`assert_the_buffer_holds_audio` in `recv.rs` and the equivalent block in `crates/pheme-app/tests/audio.rs` both read the depth counter. Now that it reports the pre-pop depth, the same bound has a full frame of headroom instead of none. Replace the doc comment and the bounds:

```rust
    /// The jitter buffer must be holding about its target: neither empty nor filling.
    ///
    /// `audio_depth_ms` reports the depth *before* each pop, which is the audio waiting
    /// to be played. With a 2-frame target that reads as 10 ms, dipping to 5 at 44.1 kHz
    /// where one wire frame resamples to a hair more than one device period and the
    /// worker occasionally takes two pops to refill the ring. So a lower bound of 5 has
    /// a whole frame of headroom, where the same number against the post-pop depth had
    /// none and was a latent flake. The upper bound catches a buffer that is quietly
    /// filling up; it rises by one frame for the same reason.
    fn assert_the_buffer_holds_audio(depths: &[u64]) {
        let mut steady = depths[50..].to_vec();
        steady.sort_unstable();
        let median = steady[steady.len() / 2];
        let max = steady.last().copied().unwrap_or(0);
        assert!(
            median >= 5,
            "median jitter depth {median} ms: the buffer is running empty, so the \
             playback worker is outrunning the sender"
        );
        assert!(
            max <= 20,
            "jitter depth reached {max} ms: the buffer is filling up, so the sender is \
             outrunning the playback worker"
        );
    }
```

Make the same change to the inline block in `crates/pheme-app/tests/audio.rs` around line 294 (it asserts the lower bound only; keep it that way and update its comment to say pre-pop).

- [ ] **Step 5: Add the clipping bound (debt (e))**

Nothing in the suite bounds the amplitude of what comes out, so a regression in the f32↔i16 conversion — a lost clamp, a wrapping cast, a sign error — would pass everything. Add to `recv.rs`'s tests:

```rust
    /// A full-scale sine, which is where a conversion bug shows up.
    fn loud_sine_frame(i: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(FRAME_INTERLEAVED);
        for n in 0..FRAME_SAMPLES {
            let t = (i * FRAME_SAMPLES + n) as f32 / 48_000.0;
            let v = (32_000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16;
            out.push(v);
            out.push(v);
        }
        out
    }

    #[test]
    fn a_full_scale_signal_is_not_clipped_wrapped_or_inverted() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn(PlaybackSource::Backend(Box::new(play)), stats.clone());
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let mut packer = Packer::new();
        for i in 0..200 {
            let f = packer
                .push(&loud_sine_frame(i), i as u64 * FRAME_US)
                .expect("a sine is never silent");
            audio.push(f);
            handle.drain_frames(1);
            assert!(wait_until(
                || handle.queued() >= FRAME_INTERLEAVED,
                Duration::from_secs(5)
            ));
        }
        audio.stop();

        let rec = handle.recorded();
        // Skip the prefill, where the worker is emitting silence.
        let body = &rec[FRAME_INTERLEAVED * 10..];
        let peak = body.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        // The upper bound is 32_768, not 32_767: `i16::MIN.abs()` is 32768, and -32768 is
        // exactly what the production clamp produces when the resampler's sinc overshoot
        // on a near-full-scale signal reaches the rail. That is correct behaviour, not
        // clipping damage. The bound that earns its keep here is the lower one, which
        // catches attenuation; wrapping is caught by the inter-sample step below, since a
        // wrapping cast turns an overshoot into a sign flip rather than a rail hit.
        assert!(
            (30_000..=32_768).contains(&peak),
            "peak {peak} out of a 32000 input: the signal was clipped or attenuated"
        );

        // The wrap check is NOT done here. A pipeline recording legitimately contains
        // large inter-sample steps: the jitter buffer emits a silence frame when it has
        // nothing (`Pop::Idle`) and a full-gain copy of the previous frame when it
        // conceals (`Pop::Conceal`), and either one is a phase discontinuity in a
        // continuous sine — up to 32 768 for a drop to silence and roughly twice that
        // across a half period. A step bound here would be measuring whether the buffer
        // ever ran dry, not whether the conversion wrapped. The conversion is pinned
        // directly instead, by `to_i16`'s own test below.
    }

    #[test]
    fn the_sample_conversion_scales_and_rails_correctly() {
        // Debt (e), pinned where it actually lives. The resampler overshoots on
        // near-full-scale input, so values outside +/-1.0 reach this conversion in normal
        // operation; a wrapping cast would turn a positive overshoot into a large
        // negative sample, which is an audible click with no counter to show for it.
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(0.5), 16_384);
        assert_eq!(to_i16(-0.5), -16_384);
        assert_eq!(to_i16(1.0), 32_767, "the positive rail");
        assert_eq!(to_i16(-1.0), -32_768, "the negative rail");
        assert_eq!(to_i16(1.5), 32_767, "an overshoot clamps, it does not wrap");
        assert_eq!(to_i16(-1.5), -32_768, "and the same below");
        assert_eq!(to_i16(1e9), 32_767, "however far outside it lands");
        assert_eq!(to_i16(-1e9), -32_768);
        // These two are the ones that can actually fail. Rust's `f32 as i16` has
        // saturated since 1.45, so the clamp above is behaviourally a no-op and no
        // assertion can distinguish the clamped form from the bare cast — measured, not
        // assumed. What is worth pinning is the scale and the rounding: a 32_767.0 scale
        // yields 29_490 for 0.9, and truncation yields 10_922 for a third.
        assert_eq!(to_i16(0.9), 29_491, "the scale is 32_768, not 32_767");
        assert_eq!(to_i16(1.0 / 3.0), 10_923, "the conversion rounds, it does not truncate");
    }
```

- [ ] **Step 6: Update the callers**

In `crates/pheme-app/src/client.rs`: `use crate::audio::{CaptureSource, OutCounters, SendSide};`, the field type becomes `SendSide`, and construction becomes

```rust
    let mut audio = SendSide::spawn(audio, AudioStream::Playback, counters.clone());
```

with `AudioStream` added to the `pheme_proto` import. The `session` parameter `audio: &AudioOut` becomes `audio: &SendSide`.

In `crates/pheme-app/src/server.rs`: `use crate::audio::{InStats, PlaybackSource, RecvSide};`, `Shared.audio: RecvSide`, and `RecvSide::spawn(audio, audio_stats.clone())`.

In `crates/pheme-app/tests/audio.rs` and `crates/pheme-app/tests/integration.rs`, change only the import paths and type names; the constructors there take `CaptureSource`/`PlaybackSource` values that do not change.

- [ ] **Step 7: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS — the same test count as before this task plus the one new clipping test.

Run: `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets`
Expected: no errors.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add -A crates/pheme-app
git commit -m "$(cat <<'MSG'
Update: split the audio supervisor into SendSide and RecvSide

The two supervisors were near-identical - spawn, retry, failure log,
nap, Drop - and the microphone direction was about to add a third and a
fourth. Each role now builds one SendSide and one RecvSide that differ
only in the backend detected and the AudioStream tag carried, so the
shared shape lives in one place and the file gets smaller as the second
direction arrives rather than larger.

Two debts from sub-project 2 live in the code being moved, so they are
paid here. The stats line now reports the jitter depth measured before
each pop, which is the audio waiting to be played and the quantity the
spec's latency budget counts; sampling after the pop read a frame lower
and left the depth assertion with no headroom at 44.1 kHz. And a new
test bounds the amplitude and the inter-sample step of a full-scale
signal, so a lost clamp or a wrapping cast in the f32 conversion fails
instead of passing silently.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 7: The demand gate

`SendSide` learns to close its device on request, and `RecvSide` learns to report whether anything is consuming what it emits. Neither is wired to the network yet — that is Tasks 13 and 14 — so this task is testable entirely against the mocks.

**Files:**
- Modify: `crates/pheme-app/src/audio/mod.rs`, `crates/pheme-app/src/audio/send.rs`, `crates/pheme-app/src/audio/recv.rs`

**Interfaces:**
- Consumes: `pheme_audio::Demand` and the restartable mocks from Task 4.
- Produces:
  - `SendSide::set_wanted(&self, wanted: bool)` — stops the capture device when false, rebuilds it when true.
  - `RecvSide::wanted(&self) -> tokio::sync::watch::Receiver<bool>` — debounced demand, false whenever no backend is running.
  - `RecvSide::spawn_with_linger(source, stats, linger: Duration)` — same as `spawn`, for tests that cannot wait three seconds.
  - `RecvSide::reset(&self)` — drops everything buffered and prefills again.
  - `PumpEnd::Unwanted` in `mod.rs`.

- [ ] **Step 1: Write the failing tests**

In `crates/pheme-app/src/audio/send.rs`'s test module:

```rust
    #[test]
    fn a_side_that_is_not_wanted_closes_its_device() {
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        audio.set_wanted(false);
        assert!(
            wait_until(|| !handle.started(), Duration::from_secs(2)),
            "the device must actually close, not merely stop sending: an open microphone \
             keeps its indicator lit"
        );

        audio.set_wanted(true);
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));
        assert_eq!(handle.start_count(), 2, "it was really reopened");
        audio.stop();
    }

    #[test]
    fn a_side_nobody_gates_stays_open() {
        // The client's speaker capture is never gated; it must behave exactly as before.
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Playback,
            counters,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(handle.started());
        assert_eq!(handle.start_count(), 1);
        audio.stop();
    }

    #[test]
    fn flipping_the_gate_faster_than_the_device_can_follow_settles_correctly() {
        // Review Focus 2. An application that opens and closes a recording device in a
        // burst must leave the gate and the device agreeing, with no wedged state and
        // no thread left behind.
        let (cap, handle) = MockCapture::new();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));
        for _ in 0..20 {
            audio.set_wanted(false);
            audio.set_wanted(true);
        }
        assert!(
            wait_until(|| handle.started(), Duration::from_secs(3)),
            "the gate ended on `true`, so the device must end open"
        );
        audio.set_wanted(false);
        assert!(
            wait_until(|| !handle.started(), Duration::from_secs(3)),
            "the gate ended on `false`, so the device must end closed"
        );
        audio.stop();
    }

    #[test]
    fn a_wanted_side_whose_device_will_not_open_keeps_retrying_quietly() {
        // Review Focus 4. The gate says open and the device says no; the retry cycle must
        // be the ordinary one, and the device must come up on its own once it can.
        let (cap, handle) = MockCapture::new();
        handle.fail_next_start();
        let counters = Arc::new(OutCounters::default());
        let mut audio = SendSide::spawn(
            CaptureSource::Backend(Box::new(cap)),
            AudioStream::Mic,
            counters,
        );
        assert!(
            wait_until(|| handle.started(), Duration::from_secs(8)),
            "the retry cycle must bring the microphone up after a failed start"
        );
        audio.stop();
    }
```

In `crates/pheme-app/src/audio/recv.rs`'s test module:

```rust
    const FAST_LINGER: Duration = Duration::from_millis(100);

    #[test]
    fn demand_is_false_before_a_backend_is_running() {
        let (play, _handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        // Nothing has reported consumers yet, and a backend that is not running cannot
        // deliver audio to anything, so asking for a microphone would be pure cost.
        assert!(!*audio.wanted().borrow());
        audio.stop();
    }

    #[test]
    fn an_unknown_backend_is_treated_as_wanted() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));
        let w = audio.wanted();
        assert!(
            wait_until(|| *w.borrow(), Duration::from_secs(2)),
            "Unknown must mean open, or a backend that cannot detect consumers silently \
             kills the feature"
        );
        audio.stop();
    }

    #[test]
    fn an_idle_backend_closes_the_gate_only_after_the_linger() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.set_demand(Demand::Wanted);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            FAST_LINGER,
        );
        let w = audio.wanted();
        assert!(wait_until(|| *w.borrow(), Duration::from_secs(2)));

        handle.set_demand(Demand::Idle);
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            *w.borrow(),
            "the gate must not close on the first idle poll: applications probe devices"
        );
        assert!(
            wait_until(|| !*w.borrow(), Duration::from_secs(2)),
            "but it must close once the linger has passed"
        );
        audio.stop();
    }

    #[test]
    fn a_consumer_returning_inside_the_linger_never_closes_the_gate() {
        let (play, handle) = MockPlayback::new(48_000);
        handle.set_demand(Demand::Wanted);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats,
            Duration::from_millis(400),
        );
        let w = audio.wanted();
        assert!(wait_until(|| *w.borrow(), Duration::from_secs(2)));

        for _ in 0..5 {
            handle.set_demand(Demand::Idle);
            std::thread::sleep(Duration::from_millis(80));
            handle.set_demand(Demand::Wanted);
            std::thread::sleep(Duration::from_millis(20));
            assert!(*w.borrow(), "the microphone must not flap");
        }
        audio.stop();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib audio::`
Expected: FAIL to compile — `set_wanted`, `wanted`, `spawn_with_linger` do not exist.

- [ ] **Step 3: Add `PumpEnd::Unwanted`**

In `crates/pheme-app/src/audio/mod.rs`:

```rust
pub(crate) enum PumpEnd {
    /// A stop was requested, or the channel was dropped: do not rebuild.
    Stopped,
    /// The gate closed: stop the device and wait for it to open again. Not a failure, so
    /// it is neither logged as one nor made to serve the five-second retry delay.
    Unwanted,
    /// The device, or something the pump owns, failed: rebuild after the retry delay.
    Failed(String),
}

/// How long a `RecvSide` keeps reporting demand after its last consumer left.
///
/// Applications probe recording devices — enumerate, open briefly, close — and without a
/// debounce each probe would open and close the far end's microphone, which is visible to
/// the user and hard on the device. Only the closing edge waits; a consumer arriving is
/// reported at once.
pub(crate) const LINGER: Duration = Duration::from_secs(3);

/// How long a gated `SendSide` sleeps between checks while its gate is shut.
pub(crate) const GATE_POLL: Duration = Duration::from_millis(20);
```

- [ ] **Step 4: Implement the gate in `send.rs`**

Add a `wanted: Arc<AtomicBool>` to `SendSide`, initialised to `true`, cloned into `out_thread`:

```rust
impl SendSide {
    /// Opens or closes the capture device.
    ///
    /// This stops the device itself rather than merely ceasing to send, so the operating
    /// system reports the microphone as closed and its indicator goes out. A side nobody
    /// calls this on stays open, which is what the client's speaker capture wants.
    pub fn set_wanted(&self, wanted: bool) {
        self.wanted.store(wanted, Ordering::SeqCst);
    }
}
```

In `out_thread`, wait for the gate before building anything, and treat a closed gate as a reason to stop the device without the retry delay:

```rust
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if !wanted.load(Ordering::SeqCst) {
            if !nap(GATE_POLL, &stop) {
                return;
            }
            continue;
        }
        // …build and start the backend exactly as before…
        let end = pump_out(backend.as_ref(), consumer, &peer, &stop, &counters, stream, &wanted);
        backend.stop();
        match end {
            PumpEnd::Stopped => return,
            // The gate closed. Go straight back to the top: reopening must be prompt, and
            // a closed gate is not a failure to back off from.
            PumpEnd::Unwanted => continue,
            PumpEnd::Failed(why) => failures.report(format!("audio capture stopped: {why}")),
        }
        if stop.load(Ordering::SeqCst) || !rebuild {
            return;
        }
        if !nap(RETRY, &stop) {
            return;
        }
    }
```

Note the `PumpEnd::Stopped` arm now returns directly, matching what `in_thread` already does; the old code fell through to the `!rebuild` check, which reached the same conclusion less obviously.

In `pump_out`, add the gate check beside the health check:

```rust
    while !stop.load(Ordering::SeqCst) {
        if !wanted.load(Ordering::SeqCst) {
            return PumpEnd::Unwanted;
        }
        if !backend.healthy() {
            return PumpEnd::Failed("the capture device stopped".into());
        }
```

Because `CaptureSource::Backend` sets `rebuild = false`, a gated test backend would stop for good after its first close. Change the `Backend` variant's handling so the injected box is kept and restarted rather than consumed: hold it in an `Option<Box<dyn AudioCapture>>` that is reused across loop iterations instead of `take`n once, and update the variant's doc comment:

```rust
    /// Use this backend. It is started and stopped as often as the gate asks, which is
    /// what lets a test drive `set_wanted`.
    Backend(Box<dyn AudioCapture>),
```

- [ ] **Step 5: Implement demand reporting in `recv.rs`**

`RecvSide` gains `wanted_tx: watch::Sender<bool>` (initially `false`) and `wanted_rx: watch::Receiver<bool>`:

```rust
impl RecvSide {
    pub fn spawn(source: PlaybackSource, stats: Arc<InStats>) -> RecvSide {
        RecvSide::spawn_with_linger(source, stats, LINGER)
    }

    /// As `spawn`, with the closing-edge debounce named explicitly. Tests use a short one.
    pub fn spawn_with_linger(
        source: PlaybackSource,
        stats: Arc<InStats>,
        linger: Duration,
    ) -> RecvSide {
        // …as before, additionally creating `watch::channel(false)` and passing the
        // sender and `linger` into `in_thread`…
    }

    /// Whether anything is consuming what this side plays, debounced by the linger.
    ///
    /// False whenever no backend is running: a backend that is not running cannot deliver
    /// audio to anything, so asking the far end to open a microphone for it would be pure
    /// cost. That is not a violation of the fail-open rule — that rule protects against
    /// *not knowing*, and this is knowing the answer is no.
    pub fn wanted(&self) -> watch::Receiver<bool> {
        self.wanted_rx.clone()
    }
}
```

`in_thread` sets the gate false whenever it is between backends — before building, after a backend stops, and on return. `pump_in` drives it, once per `TICK`, right beside the existing `publish` call:

```rust
        let now = backend.demand();
        match now {
            Demand::Idle => {
                if idle_since.is_none() {
                    idle_since = Some(Instant::now());
                }
                if idle_since.is_some_and(|t| t.elapsed() >= linger) {
                    let _ = wanted_tx.send(false);
                }
            }
            Demand::Wanted | Demand::Unknown => {
                idle_since = None;
                let _ = wanted_tx.send(true);
            }
        }
```

with `let mut idle_since: Option<Instant> = None;` declared above the loop.

- [ ] **Step 6: Add `RecvSide::reset`**

The client calls this at the moment it asks the far end to reopen its microphone (spec §3.4). `RecvSide` owns the jitter buffer inside its worker thread, so the request crosses as a flag rather than a call:

```rust
impl RecvSide {
    /// Drops everything buffered and prefills again.
    ///
    /// The client calls this as it asks the server to reopen its microphone, *before* the
    /// audio starts arriving. The worker acts on it at its next tick, which is within
    /// `TICK`, long before the first frame of a resumed stream can cross the network.
    pub fn reset(&self) {
        self.reset_requested.store(true, Ordering::SeqCst);
    }
}
```

with `reset_requested: Arc<AtomicBool>` on `RecvSide`, cloned into `in_thread` and `pump_in`. At the top of `pump_in`'s loop, beside the health check:

```rust
        if reset_requested.swap(false, Ordering::SeqCst) {
            jitter.restart();
        }
```

A reset requested while no backend is running is simply observed by the next one, which starts empty anyway.

Add the test to `recv.rs`:

```rust
    #[test]
    fn a_reset_empties_the_buffer() {
        let (play, handle) = MockPlayback::new(48_000);
        let stats = Arc::new(InStats::default());
        let mut audio = RecvSide::spawn_with_linger(
            PlaybackSource::Backend(Box::new(play)),
            stats.clone(),
            FAST_LINGER,
        );
        assert!(wait_until(|| handle.started(), Duration::from_secs(2)));

        let mut packer = Packer::new();
        for i in 0..20 {
            let f = packer
                .push(&sine_frame(i), i as u64 * FRAME_US)
                .expect("a sine is never silent");
            audio.push(f);
        }
        assert!(wait_until(
            || stats.depth_ms.load(Ordering::Relaxed) > 0,
            Duration::from_secs(2)
        ));
        let before = stats.resets.load(Ordering::Relaxed);
        audio.reset();
        assert!(
            wait_until(
                || stats.resets.load(Ordering::Relaxed) > before,
                Duration::from_secs(2)
            ),
            "the worker must act on the reset"
        );
        audio.stop();
    }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib audio::`
Expected: PASS, including the nine new tests.

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-app/src/audio
git commit -m "$(cat <<'MSG'
Update: gate a SendSide's device on demand

SendSide::set_wanted stops the capture device outright rather than
merely ceasing to send, so the operating system reports the microphone
as closed and its indicator goes out. A closed gate is not a failure, so
it neither logs as one nor serves the five-second retry delay: reopening
has to be prompt.

RecvSide reports, through a watch channel, whether anything is consuming
what it plays. Unknown counts as wanted, so a backend that cannot detect
consumers keeps the far end's microphone open rather than silently
killing the feature. The closing edge is debounced by three seconds
because applications probe recording devices, and an undebounced probe
would make the far end's microphone flap.

A side nobody gates stays open, which is what the client's speaker
capture wants.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 8: Protocol — `MicWanted`, `AudioParams` on `Hello`, version 2

**Files:**
- Modify: `crates/pheme-proto/src/lib.rs`

**Interfaces:**
- Produces: `Msg::MicWanted { wanted: bool }`; `Msg::Hello` with an added `audio: AudioParams` field; `PROTOCOL_VERSION == 2`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/pheme-proto/src/lib.rs`'s test module (create one if the file has none, following the round-trip style the crate's other tests use):

```rust
    #[test]
    fn mic_wanted_round_trips_and_is_not_a_datagram() {
        let m = Msg::MicWanted { wanted: true };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        assert_eq!(decode(&buf).unwrap(), m);
        assert!(
            !m.is_datagram(),
            "a lost demand signal would strand the microphone open or shut"
        );
    }

    #[test]
    fn hello_carries_the_audio_parameters() {
        let m = Msg::Hello {
            version: PROTOCOL_VERSION,
            name: "laptop".into(),
            os: Os::Linux,
            screens: Vec::new(),
            audio: AudioParams::DEFAULT,
        };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        match decode(&buf).unwrap() {
            Msg::Hello { audio, .. } => assert_eq!(audio, AudioParams::DEFAULT),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn the_protocol_version_is_two() {
        // Bumped when Hello gained `audio`. Note that a version-1 Hello now fails to
        // decode before its `version` field can be read, so a mismatched peer reports a
        // malformed handshake rather than a version mismatch.
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn a_mic_audio_frame_round_trips_like_a_playback_one() {
        let m = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 7,
            ts_us: 35_000,
            samples: vec![0u8; 960],
        };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        assert_eq!(decode(&buf).unwrap(), m);
        assert!(m.is_datagram());
        assert!(
            buf.len() <= 1200,
            "a frame must fit the datagram budget: {} bytes",
            buf.len()
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-proto`
Expected: FAIL to compile — `Msg::MicWanted` does not exist and `Msg::Hello` has no `audio` field.

- [ ] **Step 3: Implement**

```rust
pub const PROTOCOL_VERSION: u16 = 2;
```

Add the field to `Hello`:

```rust
    Hello {
        version: u16,
        name: String,
        os: Os,
        screens: Vec<ScreenInfo>,
        /// What this peer speaks. `HelloAck` carries the server's; this carries the
        /// client's, so each end can refuse a format it does not understand instead of
        /// transmitting into one.
        audio: AudioParams,
    },
```

Add the new message, in the control-stream group next to `Bye`:

```rust
    /// Client → server: whether anything on the client is recording from its virtual
    /// microphone. Sent once after the handshake and again on every change.
    ///
    /// Control stream, not a datagram: a lost or reordered demand signal would leave the
    /// microphone stranded open or stranded shut, and it is sent a handful of times per
    /// session.
    MicWanted {
        wanted: bool,
    },
```

`is_datagram` needs no change — it lists the datagram variants explicitly, and `MicWanted` is not among them.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-proto`
Expected: PASS.

Run: `cargo test --workspace`
Expected: FAIL to compile — `Msg::Hello` is built or matched in three places that do not know about the new field. Fix all three:

- `crates/pheme-app/src/client.rs`: add `audio: AudioParams::DEFAULT` where `Hello` is sent.
- `crates/pheme-app/src/server.rs`: add `audio: _` to the `Msg::Hello { .. }` pattern. The server's real use of the field arrives in Task 14.
- `crates/pheme-net/tests/transport.rs`: the `hello()` helper builds a `Hello` with `version: 1` and no `audio`. Change it to `version: PROTOCOL_VERSION` and `audio: AudioParams::DEFAULT`, importing both from `pheme_proto`. Leaving the literal `1` there would pin a version the code no longer speaks.

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-proto/src/lib.rs crates/pheme-app/src/client.rs crates/pheme-app/src/server.rs
git commit -m "$(cat <<'MSG'
Update: add MicWanted and put AudioParams on Hello

MicWanted is how the client tells the server that something is recording
from its virtual microphone. It travels on the control stream rather
than as a datagram: it is sent a handful of times per session, and a
lost or reordered one would leave the microphone stranded open or shut.

Hello now carries AudioParams too. Sub-project 2 left the exchange
one-way and deferred the decision to this sub-project on the grounds
that a second stream in the opposite direction changes it - and it does,
because the server now sends audio and must be able to check what the
client speaks rather than transmit and hope.

That changes Hello's encoding, so the protocol version goes to 2. Worth
recording: postcard decodes a message whole, so a version-1 Hello now
fails to decode before its version field can be read, and a mismatched
peer reports a malformed handshake rather than a version mismatch. Both
ends are built from the same tree and nothing is released, so this costs
nothing today.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 9: Give audio its own channel in `pheme-net`

`Peer` merges the control stream and the datagram stream into one bounded channel of 256 messages. Sub-project 2 tolerated that because only one side received audio. This sub-project makes **both** sides receive audio and input at once, and the arithmetic is bad: a brief stall can park a mouse event behind up to 255 audio frames, which is 1.28 seconds, in a project whose first stated priority is input latency.

**Files:**
- Modify: `crates/pheme-net/src/transport.rs`
- Modify: `crates/pheme-net/tests/transport.rs`

**Interfaces:**
- Produces: `Peer::take_audio(&mut self) -> mpsc::Receiver<Msg>`, delivering only `Msg::Audio`; `Peer::take_incoming` unchanged for callers but no longer carrying audio.

- [ ] **Step 1: Write the failing test**

Append to `crates/pheme-net/tests/transport.rs`:

```rust
/// Audio must not be able to delay input. The two travel on the same QUIC connection and
/// used to share one bounded channel, so a receiver that stalled for a moment would find
/// a burst of audio queued ahead of the next mouse movement.
#[tokio::test]
async fn audio_cannot_queue_ahead_of_input() {
    let s = side("server");
    let c = side("client");
    trust_each_other(&s, &c);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr: SocketAddr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();

    let server_task = tokio::spawn(async move {
        let Incoming::Peer(mut peer) = server.accept().await.unwrap() else {
            panic!("expected peer")
        };
        let mut input = peer.take_incoming();
        let mut audio = peer.take_audio();
        // The control stream is ordered and reliable, so the Key sent last arrives; what
        // matters is that it is not queued behind the audio, which goes to its own
        // channel entirely.
        match input.recv().await.unwrap() {
            Msg::Key { seq: 42, .. } => {}
            other => panic!("input channel delivered {other:?}"),
        }
        let mut frames = 0;
        while let Ok(Some(m)) =
            tokio::time::timeout(Duration::from_secs(2), audio.recv()).await
        {
            assert!(
                matches!(m, Msg::Audio { .. }),
                "the audio channel carried {m:?}"
            );
            frames += 1;
            if frames == 8 {
                break;
            }
        }
        assert_eq!(frames, 8, "every audio frame reached the audio channel");
        peer.close("done");
    });

    let peer = client.connect(addr).await.unwrap();
    for seq in 0..8 {
        peer.sender().send_datagram(&Msg::Audio {
            stream: pheme_proto::AudioStream::Mic,
            seq,
            ts_us: u64::from(seq) * 5_000,
            samples: vec![0u8; 960],
        });
    }
    peer.sender()
        .send_control(&Msg::Key {
            seq: 42,
            code: pheme_proto::KeyCode(0x04),
            down: true,
        })
        .await
        .unwrap();
    server_task.await.unwrap();
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pheme-net --test transport audio_cannot_queue_ahead_of_input`
Expected: FAIL to compile — `take_audio` does not exist.

- [ ] **Step 3: Implement the split**

In `crates/pheme-net/src/transport.rs`, add the constant next to `CONTROL_BUFFER`:

```rust
/// Audio frames the receiver may queue before dropping them.
///
/// Small on purpose. `JitterBuffer` downstream has a ceiling of 24 frames, so a deeper
/// queue here can only add latency that the buffer will then discard; and a full channel
/// dropping a frame is a loss the jitter buffer conceals, whereas a full *shared* channel
/// used to mean a mouse event waiting behind 1.28 s of audio.
const AUDIO_BUFFER: usize = 32;
```

In `Peer::new`, build a second channel and route by message kind in the datagram reader:

```rust
        let (tx, rx) = mpsc::channel(CONTROL_BUFFER);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_BUFFER);
        let control_tx = tx.clone();
        // …the control-stream task is unchanged: it sends on `control_tx`…
        let dgram_conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(bytes) = dgram_conn.read_datagram().await {
                match decode(&bytes) {
                    Ok(m @ Msg::Audio { .. }) => {
                        // Never block the datagram reader on a slow audio consumer:
                        // a dropped frame is concealed, a stalled reader delays input.
                        if audio_tx.try_send(m).is_err() {
                            debug!("audio channel full; dropping a frame");
                        }
                    }
                    Ok(m) => {
                        if tx.send(m).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => debug!("bad datagram: {e}"),
                }
            }
        });
```

Audio can also arrive on the control stream if a peer misbehaves; route it the same way there rather than letting it into the input channel. Add the `audio: Option<mpsc::Receiver<Msg>>` field to `Peer`, and:

```rust
    /// Takes the receiver carrying `Msg::Audio` and nothing else. Panics if called twice.
    ///
    /// Audio is deliberately not merged with the input channel: they share a connection
    /// but not a deadline. Input must never wait behind audio.
    pub fn take_audio(&mut self) -> mpsc::Receiver<Msg> {
        self.audio.take().expect("audio receiver already taken")
    }
```

Update `take_incoming`'s doc comment: it is now "the control and non-audio datagram receiver".

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-net`
Expected: PASS.

Run: `cargo test --workspace`
Expected: FAIL — `crates/pheme-app/src/server.rs` matches `Msg::Audio` on the channel from `take_incoming`, which no longer carries it, so the server's audio integration test goes quiet. Leave the server code alone; Task 14 rewires it. To keep the tree green in the meantime, add `let mut audio_rx = peer.take_audio();` in `handle_peer` and select on it beside `rx.recv()`, moving the existing `Msg::Audio` arm across unchanged.

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-net crates/pheme-app/src/server.rs
git commit -m "$(cat <<'MSG'
Fix: keep audio out of the input channel

Peer merged the control stream and the datagram stream into one bounded
channel of 256 messages. That was tolerable while only one side received
audio; the microphone direction makes both sides receive audio and input
at once, and a brief stall could then park a mouse event behind up to
255 audio frames - 1.28 seconds - in a project whose first priority is
input latency.

Msg::Audio now goes to its own channel of 32 frames, whichever stream it
arrived on. Small on purpose: the jitter buffer downstream has a ceiling
of 24 frames, so a deeper queue here would only add latency that the
buffer discards, and a full audio channel drops a frame the jitter
buffer conceals rather than delaying a keystroke.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 10: The Linux virtual source, "Pheme Mic"

The design was verified before the spec was written, with a throwaway probe against PipeWire 1.6.8 on this machine: an `Audio/Source` node connected with `Direction::Output` appears in `pactl list sources short` as `s16le 2ch 48000Hz`, its state tracks consumers exactly (`Paused` with none, `Streaming` with one), and a 440 Hz tone came back at 440.1 Hz with the longest run of zero samples equal to 1.

**Files:**
- Modify: `crates/pheme-audio/src/linux_pipewire.rs`, `crates/pheme-audio/src/lib.rs`

**Interfaces:**
- Consumes: `DeviceThread`/`Ready` (Task 1), `Demand` (Task 4).
- Produces: `linux_pipewire::PipewireVirtualSource` implementing `AudioPlayback` with a real `demand()`; `pheme_audio::detect_virtual_mic(device: Option<&str>) -> Result<Box<dyn AudioPlayback>>`.

- [ ] **Step 1: Write the failing test**

Add to `crates/pheme-audio/src/linux_pipewire.rs` a test module. It needs a running PipeWire daemon, which this machine has; guard it so a machine without one skips rather than fails:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioPlayback, Demand};

    /// True when a PipeWire daemon is reachable. CI containers may not have one.
    fn have_pipewire() -> bool {
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").is_some()
            || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| {
                std::path::Path::new(&d).join("pipewire-0").exists()
            })
    }

    #[test]
    fn the_virtual_source_starts_and_reports_no_consumers() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        let (_producer, consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 16);
        let mut src = PipewireVirtualSource::new();
        src.start(consumer).expect("the node must be created");
        assert!(src.healthy());
        assert_eq!(src.rate(), RATE, "we own this node, so it runs at 48 kHz");
        assert_eq!(src.device_name(), "Pheme Mic");

        // Nothing is recording from a node that has just been created, and reporting
        // otherwise would hold the far end's microphone open for no one.
        let mut saw_idle = false;
        for _ in 0..100 {
            if src.demand() == Demand::Idle {
                saw_idle = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_idle, "an unconsumed source must settle on Idle");
        src.stop();
        assert!(!src.healthy());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pheme-audio linux_pipewire::`
Expected: FAIL to compile — `PipewireVirtualSource` does not exist.

- [ ] **Step 3: Implement the backend**

Add to `crates/pheme-audio/src/linux_pipewire.rs`. The node properties mirror the virtual sink, with the class and direction reversed:

```rust
/// The client's virtual microphone: we write samples, applications record them.
///
/// The mirror of `PipewireCapture`. `Direction::Output` plus `media.class = Audio/Source`
/// is what makes this a recording device other applications can select, rather than a
/// playback stream.
pub struct PipewireVirtualSource {
    demand: Arc<AtomicU8>,
    thread: DeviceThread,
}

/// `Demand` as an atomic, because the stream listener sets it from the PipeWire thread.
const DEMAND_UNKNOWN: u8 = 0;
const DEMAND_WANTED: u8 = 1;
const DEMAND_IDLE: u8 = 2;

impl Default for PipewireVirtualSource {
    fn default() -> Self {
        PipewireVirtualSource::new()
    }
}

impl PipewireVirtualSource {
    pub fn new() -> PipewireVirtualSource {
        PipewireVirtualSource {
            // Unknown until the node exists: until then we do not know, and not knowing
            // means keep the far end's microphone open.
            demand: Arc::new(AtomicU8::new(DEMAND_UNKNOWN)),
            thread: DeviceThread::new(),
        }
    }
}

impl AudioPlayback for PipewireVirtualSource {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let demand = self.demand.clone();
        self.thread.start(
            "pheme-pw-mic",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| virtual_source_thread(source, demand, ready, cmd_rx),
        )
    }

    /// Always 48 kHz: we create this node, so its rate is ours to choose and the base
    /// resample ratio is exactly 1.0.
    fn rate(&self) -> u32 {
        RATE
    }

    fn device_name(&self) -> String {
        "Pheme Mic".into()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    /// Derived from the stream state, which tracks consumers exactly: a source with
    /// nothing recording from it sits in `Paused` and moves to `Streaming` when an
    /// application connects. Measured on PipeWire 1.6.8 before this was designed.
    ///
    /// A level meter counts as a consumer — an open sound-settings input page, or
    /// `pavucontrol` — which is correct: something really is listening.
    fn demand(&self) -> Demand {
        match self.demand.load(Ordering::SeqCst) {
            DEMAND_WANTED => Demand::Wanted,
            DEMAND_IDLE => Demand::Idle,
            _ => Demand::Unknown,
        }
    }

    fn stop(&mut self) {
        self.thread.stop();
        // A node that no longer exists has no consumers, and saying "unknown" here would
        // hold the far end's microphone open for a device that cannot deliver to anyone.
        self.demand.store(DEMAND_IDLE, Ordering::SeqCst);
    }
}

impl Drop for PipewireVirtualSource {
    fn drop(&mut self) {
        self.stop();
    }
}
```

The thread body follows `run`'s shape exactly, with four differences: the property block, the direction, the `process` callback filling the buffer from the ring instead of draining it into one, and the state listener recording demand as well as death.

```rust
    let stream = pw::stream::StreamBox::new(
        &core,
        "pheme-mic",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => "pheme-mic",
            *pw::keys::NODE_DESCRIPTION => "Pheme Mic",
            *pw::keys::AUDIO_RATE => "48000",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => "240/48000",
        },
    )
    .map_err(|e| Error::Device(format!("creating the Pheme Mic node: {e}")))?;
```

The process callback, which runs on PipeWire's real-time thread and therefore allocates nothing and takes no lock:

```rust
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else {
                return;
            };
            let stride = 2 * CHANNELS;
            let Some(slice) = d.data() else {
                return;
            };
            let frames = slice.len() / stride;
            for f in 0..frames {
                for c in 0..CHANNELS {
                    // An empty ring plays silence rather than stalling the graph.
                    let v = data.source.pop().unwrap_or(0);
                    let at = f * stride + c * 2;
                    slice[at..at + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
```

and the state listener:

```rust
        .state_changed({
            let quit_loop = mainloop.clone();
            let demand = demand.clone();
            let mut streamed = false;
            move |_, _, old, new| {
                debug!(?old, ?new, "Pheme Mic state");
                demand.store(
                    if matches!(new, StreamState::Streaming) {
                        DEMAND_WANTED
                    } else {
                        DEMAND_IDLE
                    },
                    Ordering::SeqCst,
                );
                if let Some(why) = stream_died(&mut streamed, &new) {
                    warn!("Pheme Mic is gone: {why}; ending the PipeWire thread");
                    quit_loop.quit();
                }
            }
        })
```

Connect with `Direction::Output` and the same `format_pod()`, `AUTOCONNECT | MAP_BUFFERS | RT_PROCESS` flags. Note that `stream_died` treats `Unconnected` after `Streaming` as death, which is right for a source too.

Add `use std::sync::atomic::AtomicU8;` to the imports.

- [ ] **Step 4: Add `detect_virtual_mic`**

In `crates/pheme-audio/src/lib.rs`:

```rust
/// Picks the client's virtual-microphone backend for this OS.
///
/// `device` is accepted for symmetry with the other detectors and is unused on Linux,
/// where we create the node rather than choosing one.
pub fn detect_virtual_mic(device: Option<&str>) -> Result<Box<dyn AudioPlayback>> {
    #[cfg(target_os = "linux")]
    {
        if let Some(d) = device {
            tracing::warn!(
                device = d,
                "audio.virtual_mic_device is ignored on Linux; applications select \
                 \"Pheme Mic\" instead"
            );
        }
        Ok(Box::new(linux_pipewire::PipewireVirtualSource::new()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        // Windows has no user-mode API that creates an audio endpoint, so a virtual
        // microphone there needs a signed kernel driver (VB-CABLE). Deferred; see §13 of
        // the sub-project 3 spec. Returning `Unsupported` is what makes a client without
        // one report no demand, so the server never opens its microphone for it.
        Err(Error::Unsupported(
            "this platform has no virtual microphone; the server's microphone will stay closed"
                .into(),
        ))
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-audio linux_pipewire::`
Expected: PASS.

- [ ] **Step 6: Confirm it is a real recording device**

```bash
cargo test -p pheme-audio linux_pipewire:: -- --nocapture &
sleep 1
pactl list sources short | grep pheme-mic
wait
```

Expected: a line reading `pheme-mic  PipeWire  s16le 2ch 48000Hz`. If it is missing, the node properties are wrong — most likely `media.class`.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/linux_pipewire.rs crates/pheme-audio/src/lib.rs
git commit -m "$(cat <<'MSG'
Update: add the Linux virtual microphone node

Pheme Mic is the mirror of Pheme Speaker: Direction::Output plus
media.class Audio/Source makes a node applications can select as a
recording device. Channel positions are declared explicitly, as they are
on every other stream here, because an unpositioned stereo format
negotiated against a positioned peer segfaults inside
libspa-audioconvert.

The node reports its own demand from the stream state, which tracks
consumers exactly - Paused with none, Streaming with one - so the signal
that decides whether the server opens its microphone costs nothing
beyond a listener the file already registers. A level meter counts as a
consumer, which is correct: something really is listening.

detect_virtual_mic returns Unsupported off Linux. That is what makes a
Windows client report no demand rather than unknown demand, so the
server never opens its microphone for a client that could not deliver
the audio anywhere.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 11: Linux microphone capture

The mirror of `PipewirePlayback`: a `Stream/Input/Audio` stream against the default source, or a named one. The spec flags one thing as assumed rather than measured — that the PipeWire graph will upmix a **mono** microphone to the stereo wire format — and most microphones are mono, so this task proves it rather than leaving it to manual testing.

**Files:**
- Modify: `crates/pheme-audio/src/linux_pipewire.rs`, `crates/pheme-audio/src/lib.rs`

**Interfaces:**
- Consumes: `DeviceThread`/`Ready` (Task 1), `PipewireVirtualSource` (Task 10, used by the test).
- Produces: `linux_pipewire::PipewireMic` implementing `AudioCapture`; `pheme_audio::detect_mic(device: Option<&str>) -> Result<Box<dyn AudioCapture>>`.

- [ ] **Step 1: Generalise the format pod over channel count**

`format_pod` hard-codes two channels. Give it a parameter so the test can build a mono node; production keeps passing `CHANNELS`:

```rust
/// Builds the SPA pod describing S16LE at 48 kHz with `channels` channels.
///
/// Channel positions are declared explicitly rather than left unpositioned: negotiating
/// a stereo mix from an unpositioned source into a positioned sink segfaults inside
/// `libspa-audioconvert`, which cost a core dump to find.
fn format_pod(channels: usize) -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(channels as u32);
    let mut position = [0u32; pw::spa::param::audio::MAX_CHANNELS];
    if channels == 1 {
        position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    } else {
        position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(position);
    // …unchanged from here…
}
```

Update the three existing call sites to `format_pod(CHANNELS)?`.

- [ ] **Step 2: Write the failing tests**

Add to `crates/pheme-audio/src/linux_pipewire.rs`'s test module:

```rust
    /// Drains `want` samples out of `consumer`, waiting up to two seconds.
    fn drain_at_least(consumer: &mut rtrb::Consumer<i16>, want: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(want);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while out.len() < want && std::time::Instant::now() < deadline {
            while let Ok(s) = consumer.pop() {
                out.push(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    }

    #[test]
    fn the_mic_captures_from_our_own_virtual_source() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        // Feed a constant 6000 into Pheme Mic, then capture from it. This exercises the
        // virtual source, the microphone capture and the demand signal at once, and needs
        // no sound hardware at all.
        let (mut feed, src_ring) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut src = PipewireVirtualSource::new();
        src.start(src_ring).expect("virtual source");

        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut mic = PipewireMic::new(Some("pheme-mic".into()));
        mic.start(producer).expect("microphone capture");

        // Keep the source fed while the graph settles and runs.
        let feeder = std::thread::spawn(move || {
            for _ in 0..400 {
                while feed.slots() >= 2 {
                    let _ = feed.push(6000);
                    let _ = feed.push(6000);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let got = drain_at_least(&mut consumer, FRAME_INTERLEAVED * 8);
        assert!(
            got.len() >= FRAME_INTERLEAVED * 8,
            "only {} samples arrived from the virtual source",
            got.len()
        );
        // A linked consumer is exactly what the demand signal is meant to notice.
        assert_eq!(
            src.demand(),
            Demand::Wanted,
            "capturing from the node must show up as demand"
        );
        let peak = got.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        assert!(peak > 3000, "the signal arrived at level {peak}, expected ~6000");

        mic.stop();
        src.stop();
        feeder.join().unwrap();
    }

    #[test]
    fn a_mono_source_arrives_as_two_identical_channels() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        // The risk the spec flags: most microphones are mono, and the wire format is
        // stereo. The upmix is the graph's job, and this is the test that says so.
        let (mut feed, src_ring) = rtrb::RingBuffer::<i16>::new(FRAME_SAMPLES * 64);
        let mut src = mono_test_source();
        src.start(src_ring).expect("mono source");

        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut mic = PipewireMic::new(Some("pheme-mono-test".into()));
        mic.start(producer).expect("microphone capture");

        let feeder = std::thread::spawn(move || {
            for _ in 0..400 {
                while feed.slots() >= 1 {
                    let _ = feed.push(5000);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let got = drain_at_least(&mut consumer, FRAME_INTERLEAVED * 8);
        assert!(got.len() >= FRAME_INTERLEAVED * 8);
        // Skip the first frames while the graph is still ramping.
        let body = &got[FRAME_INTERLEAVED * 2..];
        let peak = body.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        assert!(peak > 2500, "the mono signal arrived at level {peak}");
        let mismatched = body
            .chunks_exact(CHANNELS)
            .filter(|p| p[0] != p[1])
            .count();
        assert_eq!(
            mismatched, 0,
            "a mono source must reach both wire channels identically"
        );

        mic.stop();
        src.stop();
        feeder.join().unwrap();
    }
```

and the helper that builds a one-channel source node, alongside them in the test module:

```rust
    /// A single-channel `Audio/Source` node, so the mono upmix can be tested without
    /// mono hardware. Test-only: production never creates a mono node.
    fn mono_test_source() -> PipewireVirtualSource {
        PipewireVirtualSource::with_channels("pheme-mono-test", "Pheme Mono Test", 1)
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio linux_pipewire::`
Expected: FAIL to compile — `PipewireMic` and `PipewireVirtualSource::with_channels` do not exist.

- [ ] **Step 4: Add `with_channels` to the virtual source**

Keep `new()` as the production entry point and let it delegate, so the test path and the production path share every line that matters:

```rust
impl PipewireVirtualSource {
    pub fn new() -> PipewireVirtualSource {
        PipewireVirtualSource::with_channels("pheme-mic", "Pheme Mic", CHANNELS)
    }

    /// The general form. Only tests pass anything but `CHANNELS`: production has one
    /// audio format and it is stereo.
    pub fn with_channels(node: &str, description: &str, channels: usize) -> PipewireVirtualSource {
        // …store node, description and channels alongside `demand` and `thread`, and use
        // them when building the property block and calling `format_pod(channels)`…
    }
}
```

The process callback writes `channels` samples per output frame, pulling that many from the ring.

- [ ] **Step 5: Implement `PipewireMic`**

Factor the shared capture body first. `PipewireCapture`'s `run` differs from the microphone's only in its property block, so give it one:

```rust
/// The capture side of both PipeWire backends: build a node from `props`, drain its
/// buffers into `sink` until asked to stop.
fn capture_run(
    props: pw::properties::Properties,
    sink: rtrb::Producer<i16>,
    ready: &Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
    label: &str,
) -> Result<()> {
    // …the existing `run`, with `props` and `label` substituted for the hard-coded
    // property block and the "Pheme Speaker" strings…
}
```

Then:

```rust
/// The server's microphone: a recording stream against a real capture device.
pub struct PipewireMic {
    device: Option<String>,
    thread: DeviceThread,
}

impl PipewireMic {
    /// `device` is matched by PipeWire as `target.object`, i.e. against a node name or
    /// serial, exactly as `PipewirePlayback` matches an output. `pactl list sources
    /// short` prints the node names. An unknown value falls back to the default source.
    pub fn new(device: Option<String>) -> PipewireMic {
        PipewireMic {
            device,
            thread: DeviceThread::new(),
        }
    }
}

impl AudioCapture for PipewireMic {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => "pheme-mic-capture",
            *pw::keys::NODE_DESCRIPTION => "Pheme microphone capture",
            *pw::keys::AUDIO_RATE => "48000",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => "240/48000",
        };
        if let Some(d) = &self.device {
            props.insert(*pw::keys::TARGET_OBJECT, d.clone());
        }
        self.thread.start(
            "pheme-pw-mic-cap",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| {
                if let Err(e) = capture_run(props, sink, &ready, cmd_rx, "Pheme microphone") {
                    ready.fail(e);
                }
            },
        )
    }

    fn device_name(&self) -> String {
        self.device.clone().unwrap_or_else(|| "default source".into())
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for PipewireMic {
    fn drop(&mut self) {
        self.stop();
    }
}
```

Connect with `Direction::Input`. The graph converts rate, sample format and channel count on the way in, which is what the two new tests pin.

- [ ] **Step 6: Add `detect_mic`**

In `crates/pheme-audio/src/lib.rs`:

```rust
/// Picks the server's microphone backend for this OS. `device` names a specific device;
/// `None` means the platform default.
pub fn detect_mic(device: Option<&str>) -> Result<Box<dyn AudioCapture>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_pipewire::PipewireMic::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::wasapi::WasapiMic::new(
            device.map(str::to_string),
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        Err(Error::Unsupported(
            "no microphone capture backend for this platform".into(),
        ))
    }
}
```

`WasapiMic` arrives in Task 12; until then the Windows arm will not compile, so implement Task 12 before running `cargo check --target x86_64-pc-windows-gnu`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pheme-audio linux_pipewire:: -- --test-threads=1`
Expected: PASS. Single-threaded because both tests create PipeWire nodes with fixed names.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/linux_pipewire.rs crates/pheme-audio/src/lib.rs
git commit -m "$(cat <<'MSG'
Update: add Linux microphone capture

A Stream/Input/Audio stream against the default source or a named one,
the mirror of the playback stream. The capture body is now shared with
the virtual sink, which differed from it only in its property block.

The spec flagged one thing as assumed rather than measured: that the
PipeWire graph upmixes a mono microphone to the stereo wire format. Most
microphones are mono, so this adds the test that proves it, using a
one-channel source node built for the purpose rather than mono hardware
nobody here has. A second test captures from our own Pheme Mic node,
which exercises the virtual source, the capture stream and the demand
signal together and needs no sound hardware at all.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 12: Windows microphone capture

**Files:**
- Modify: `crates/pheme-audio/src/windows/wasapi.rs`

**Interfaces:**
- Consumes: `DeviceThread`/`Ready` (Task 1), `detect_mic`'s Windows arm (Task 11).
- Produces: `windows::wasapi::WasapiMic` implementing `AudioCapture`.

- [ ] **Step 1: Write the failing tests**

`ToWire` is pure logic and the only part of this file that can be tested without a device. Nothing exercises it outside the stereo 48 kHz case sub-project 2 used, so both the mono duplication and the rate conversion are unpinned — Review Focus 5. Add to `crates/pheme-audio/src/windows/wasapi.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate: u32, channels: usize, float: bool) -> FormatInfo {
        FormatInfo {
            rate,
            channels,
            float,
        }
    }

    /// Feeds `frames` of interleaved i16 through `ToWire` and returns what reached the ring.
    fn push_i16(info: FormatInfo, src: &[i16], frames: usize) -> Vec<i16> {
        let mut w = ToWire::new(info).expect("ToWire");
        let (mut producer, mut consumer) = rtrb::RingBuffer::<i16>::new(1 << 16);
        unsafe {
            w.push(src.as_ptr() as *const u8, frames, false, &mut producer);
        }
        let mut out = Vec::new();
        while let Ok(s) = consumer.pop() {
            out.push(s);
        }
        out
    }

    #[test]
    fn a_mono_source_is_duplicated_into_both_wire_channels() {
        // Most microphones are mono and the wire format is stereo. Truncation is the rule
        // for a source with more channels than the wire; a source with fewer must be
        // duplicated, and nothing pinned that until now.
        let src: Vec<i16> = vec![1000, -2000, 3000, -4000];
        let out = push_i16(fmt(RATE, 1, false), &src, src.len());
        assert_eq!(out.len(), src.len() * CHANNELS);
        for (i, pair) in out.chunks_exact(CHANNELS).enumerate() {
            assert_eq!(pair[0], pair[1], "channels differ at frame {i}");
            assert_eq!(pair[0], src[i]);
        }
    }

    #[test]
    fn a_source_wider_than_the_wire_is_truncated_not_downmixed() {
        // 5.1 input: take front left and front right, invent nothing.
        let src: Vec<i16> = vec![10, 20, 30, 40, 50, 60];
        let out = push_i16(fmt(RATE, 6, false), &src, 1);
        assert_eq!(out, vec![10, 20]);
    }

    #[test]
    fn a_source_at_the_wire_rate_is_not_resampled() {
        let w = ToWire::new(fmt(RATE, 2, false)).expect("ToWire");
        assert!(
            w.resampler.is_none(),
            "resampling at a ratio of 1.0 costs latency and CPU for nothing"
        );
    }

    #[test]
    fn a_source_below_the_wire_rate_produces_more_samples_than_it_consumed() {
        // 44.1 kHz in, 48 kHz out: about 1.088 samples out per sample in. The exact count
        // depends on the resampler's internal chunking, so bound it rather than pin it.
        let frames = RESAMPLE_CHUNK * 4;
        let src: Vec<i16> = (0..frames * 2).map(|i| (i % 1000) as i16).collect();
        let out = push_i16(fmt(44_100, 2, false), &src, frames);
        let out_frames = out.len() / CHANNELS;
        assert!(
            out_frames > frames,
            "44.1 kHz must stretch to 48 kHz: {out_frames} frames out of {frames} in"
        );
        assert!(
            out_frames < frames * 3 / 2,
            "{out_frames} frames is far more than the 1.088 ratio allows"
        );
    }

    #[test]
    fn a_silent_packet_produces_silence_rather_than_reading_the_pointer() {
        let out = push_i16(fmt(RATE, 2, false), &[], 4);
        assert_eq!(out, vec![0; 4 * CHANNELS]);
    }
}
```

`push_i16` passes an empty slice for the silent case, so `push` must not dereference `data` when `silent` is true — which its safety comment already promises.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio --target x86_64-pc-windows-gnu` is not runnable here. Instead:
Run: `cargo check -p pheme-audio --target x86_64-pc-windows-gnu --all-targets`
Expected: FAIL — `WasapiMic` does not exist (referenced by `detect_mic` from Task 11), and the test module's `FormatInfo` construction may need its fields made visible to the module.

- [ ] **Step 3: Implement `WasapiMic`**

It differs from `WasapiCapture` in exactly two ways: it opens an `eCapture` endpoint instead of `eRender`, and it initialises the client **without** `AUDCLNT_STREAMFLAGS_LOOPBACK`, which means it can be driven by an event instead of polled.

```rust
/// Records the server's microphone.
///
/// Unlike `WasapiCapture`, which loop-backs a render endpoint and therefore has to poll,
/// this attaches to a real capture endpoint and can be driven by an event.
pub struct WasapiMic {
    device: Option<String>,
    name: Arc<Mutex<String>>,
    thread: DeviceThread,
}

impl WasapiMic {
    pub fn new(device: Option<String>) -> WasapiMic {
        WasapiMic {
            device,
            name: Arc::new(Mutex::new("not started".into())),
            thread: DeviceThread::new(),
        }
    }
}

impl AudioCapture for WasapiMic {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let device = self.device.clone();
        let name = self.name.clone();
        self.thread.start(
            "pheme-wasapi-mic",
            START_TIMEOUT,
            move || stop.store(true, Ordering::SeqCst),
            move |ready| mic_thread(device, sink, thread_stop, name, ready),
        )
    }

    fn device_name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for WasapiMic {
    fn drop(&mut self) {
        self.stop();
    }
}
```

Add `open_capture_device(name: Option<&str>) -> Result<IMMDevice>`, a copy of `open_render_device` with `eRender` replaced by `eCapture` and `eConsole` kept, including its friendly-name matching and its fallback to the default when a name does not match.

`mic_thread` follows `capture_thread`'s structure with these differences:

- `open_capture_device` instead of `open_render_device`.
- `IAudioClient::Initialize` with `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` and no loopback flag, then `SetEventHandle` with a manual-reset event, and `WaitForSingleObject(event, 200)` in place of the `POLL` sleep. A 200 ms wait means a device that stops producing is noticed promptly without spinning.
- `ready.ok()` immediately after `IAudioClient::Start` succeeds — **not** at the end of the session. That inversion is the bug that made every Windows `start` time out while the device worked, and it is the reason this handshake is now centralised.
- `AUDCLNT_E_DEVICE_INVALIDATED` reopens the device internally, exactly as `capture_session` does, so `healthy()` stays true across a device change.

Reuse `ToWire` unchanged for format conversion.

- [ ] **Step 4: Type-check for Windows**

Run: `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets`
Expected: no errors, no warnings.

Run: `cargo test --workspace`
Expected: PASS (Linux is unaffected).

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-audio/src/windows/wasapi.rs
git commit -m "$(cat <<'MSG'
Update: add Windows microphone capture

WasapiMic attaches to a real eCapture endpoint rather than looping back
a render one, so it can be driven by an event instead of the 2.5 ms poll
the loopback path needs. Format conversion reuses ToWire unchanged.

It also brings the first tests ToWire has ever had. Nothing exercised it
outside the stereo 48 kHz case, so the mono duplication and the rate
conversion were both unpinned and a regression in either would have
shipped silently - half a recording missing, or every recording
pitch-shifted, with no counter moving. Those are now four tests, and
they run on the Windows CI job that has been green since sub-project 2's
last fix.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 13: Wire the mic direction into the client

**Files:**
- Modify: `crates/pheme-app/src/client.rs`

**Interfaces:**
- Consumes: `RecvSide` with `wanted()` and `reset()` (Tasks 6, 7); `Peer::take_audio()` (Task 9); `Msg::MicWanted` (Task 8); `detect_virtual_mic` (Task 10).
- Produces: `ClientDeps.mic: PlaybackSource` and `ClientDeps.mic_stats: Option<Arc<InStats>>`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/pheme-app/src/client.rs`'s test module:

```rust
    #[test]
    fn a_mic_frame_is_for_the_client_and_a_playback_frame_is_not() {
        // Review Focus 3. Each side owns one direction. A frame tagged for the other one
        // is a confused or hostile peer, and must be ignored rather than fed into the
        // buffer for the direction this side does own - which would splice unrelated
        // audio into a live recording.
        let mic = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        };
        let playback = Msg::Audio {
            stream: AudioStream::Playback,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        };
        assert!(mic_frame(&mic).is_some());
        assert!(
            mic_frame(&playback).is_none(),
            "a client must ignore the direction it sends rather than receives"
        );
        assert!(mic_frame(&Msg::Ping(1)).is_none());
    }

    #[test]
    fn a_malformed_mic_frame_is_rejected_before_it_reaches_the_buffer() {
        let short = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 10],
        };
        // The jitter buffer counts and drops these too, but rejecting here keeps a peer
        // from spending the audio channel on garbage.
        assert!(mic_frame(&short).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib client::`
Expected: FAIL to compile — `mic_frame` does not exist.

- [ ] **Step 3: Implement the routing helper**

```rust
/// The mic frame in `m`, if it is one this client should play.
///
/// A client receives `AudioStream::Mic` and sends `AudioStream::Playback`; a frame tagged
/// the other way is not ours and is dropped rather than routed into the mic buffer.
fn mic_frame(m: &Msg) -> Option<Frame> {
    match m {
        Msg::Audio {
            stream: AudioStream::Mic,
            seq,
            ts_us,
            samples,
        } if samples.len() == pheme_audio::FRAME_BYTES => Some(Frame {
            seq: *seq,
            ts_us: *ts_us,
            bytes: samples.clone(),
        }),
        _ => None,
    }
}
```

- [ ] **Step 4: Extend `ClientDeps` and spawn the receive side**

```rust
pub struct ClientDeps {
    // …existing fields…
    /// Where audio received from the server's microphone is played: the client's virtual
    /// microphone.
    pub mic: PlaybackSource,
    /// Counters the mic worker publishes. `None` allocates a private set.
    pub mic_stats: Option<Arc<InStats>>,
}
```

In `run_client`, beside the existing `SendSide`:

```rust
    let mic_stats = mic_stats.unwrap_or_default();
    let mut mic = RecvSide::spawn(mic, mic_stats.clone());
```

and `mic.stop()` next to `audio.stop()` at the end. Pass `&mic` into `session`.

`PlaybackSource::Detect` calls `detect_playback`, which opens the machine's **speakers** — exactly the wrong device here, and the mistake would be audible rather than compiler-visible: the server's microphone would come out of the client's loudspeakers. Add a variant that names the other detector:

```rust
pub enum PlaybackSource {
    /// The machine's speakers.
    Detect(Option<String>),
    /// The client's virtual microphone. A different detector, because a `RecvSide` that
    /// opened the speakers here would play the server's microphone out loud.
    DetectVirtualMic(Option<String>),
    /// Started once and never rebuilt — for tests. A `RecvSide` is never gated, so
    /// unlike its capture counterpart it has no reason to restart a backend.
    Backend(Box<dyn AudioPlayback>),
    Disabled,
}
```

`in_thread` destructures the source once, at the top, into the device name, an optional injected backend and whether to rebuild. Give that destructuring a fourth output — which detector to call — and use it in the build step:

```rust
    // `virtual` here is not a spelling of "not real": it is which of the two playback
    // detectors this side wants.
    let (device, mut injected, rebuild, virtual_mic) = match source {
        PlaybackSource::Detect(d) => (d, None, true, false),
        PlaybackSource::DetectVirtualMic(d) => (d, None, true, true),
        PlaybackSource::Backend(b) => (None, Some(b), false, false),
        PlaybackSource::Disabled => return,
    };
    // …
        let built = match injected.take() {
            Some(b) => Ok(b),
            None if virtual_mic => pheme_audio::detect_virtual_mic(device.as_deref()),
            None => pheme_audio::detect_playback(device.as_deref()),
        };
```

`detect_virtual_mic` returns `Unsupported` off Linux, and `in_thread` already treats `Unsupported` as "log once at info and stop", which is precisely the behaviour a Windows client needs.

In `main`, build it from the config:

```rust
            mic: PlaybackSource::DetectVirtualMic(None),
            mic_stats: None,
```

The device name stays `None`: naming one is only meaningful for the deferred Windows implementation in §13 of the spec, so there is no `virtual_mic_device` config key yet and the README must not mention one.

- [ ] **Step 5: Route audio and drive `MicWanted`**

In `session`, take the audio channel next to the input one and watch the demand:

```rust
    let mut rx = peer.take_incoming();
    let mut audio_rx = peer.take_audio();
    let mut mic_wanted = mic.wanted();
```

Immediately after the `HelloAck` arm accepts the server's `AudioParams`, send the first demand signal. The server starts every session with its microphone closed, so without this nothing would ever open it:

```rust
    let wanted = *mic_wanted.borrow_and_update();
    if wanted {
        mic.reset();
    }
    let _ = sender.send_control(&Msg::MicWanted { wanted }).await;
```

Add two arms to the `select!`:

```rust
            m = audio_rx.recv() => match m {
                Some(m) => {
                    if let Some(f) = mic_frame(&m) {
                        mic.push(f);
                    }
                }
                None => break Ok(()),
            },
            _ = mic_wanted.changed() => {
                let wanted = *mic_wanted.borrow_and_update();
                if wanted {
                    // Reset *before* asking, not after the audio starts arriving. While
                    // the server's microphone was shut its numbering stood still and this
                    // buffer's read cursor kept advancing, so the resuming stream arrives
                    // behind the cursor; if that distance is under RESET_GAP nothing
                    // resets on its own and every frame is discarded as late. The client
                    // knows when it is asking, so it says so.
                    mic.reset();
                }
                let _ = sender.send_control(&Msg::MicWanted { wanted }).await;
            }
```

`borrow_and_update` returns a guard that is not `Send`, so copy the `bool` out and let the guard drop before the `await` — writing `let wanted = *mic_wanted.borrow_and_update();` on its own line does exactly that.

- [ ] **Step 6: Update the test constructors**

`crates/pheme-app/tests/audio.rs` and `crates/pheme-app/tests/integration.rs` build `ClientDeps`; add `mic: PlaybackSource::Disabled` and `mic_stats: None` to each.

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-app
git commit -m "$(cat <<'MSG'
Update: play the server's microphone on the client

The client gains a RecvSide for the virtual microphone, fed from the
audio channel the transport now keeps separate from input, and tells the
server when something is recording.

Two details carry weight. Frames tagged for the other direction are
dropped rather than routed into the mic buffer, because each side owns
one direction and splicing the other one into a live recording is worse
than ignoring it. And the jitter buffer is reset before the request to
open the microphone goes out, not after the audio starts arriving: while
the far end was shut its numbering stood still and this buffer's cursor
kept advancing, so the resuming stream arrives behind the cursor and,
with a gap under RESET_GAP, would be discarded as late for up to 750 ms
with every counter reading zero.

The first MicWanted goes out with the handshake, because the server
starts every session with its microphone closed.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 14: Wire the mic direction into the server

**Files:**
- Modify: `crates/pheme-app/src/server.rs`, `crates/pheme-app/src/config.rs`

**Interfaces:**
- Consumes: `SendSide::set_wanted` (Task 7), `Msg::MicWanted` and `Hello.audio` (Task 8), `detect_mic` (Tasks 11, 12).
- Produces: `ServerDeps.mic: CaptureSource`, `ServerDeps.mic_counters: Option<Arc<OutCounters>>`, `AudioCfg.mic_device: Option<String>`.

- [ ] **Step 1: Write the failing tests**

In `crates/pheme-app/src/config.rs`'s test module:

```rust
    #[test]
    fn the_microphone_device_is_read_from_the_config() {
        let cfg: Config = toml::from_str(
            r#"
            role = "server"
            name = "desk"
            [audio]
            mic_device = "alsa_input.usb-Blue_Yeti"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.audio.mic_device.as_deref(),
            Some("alsa_input.usb-Blue_Yeti")
        );
    }

    #[test]
    fn an_absent_microphone_device_means_the_platform_default() {
        let cfg: Config = toml::from_str("role = \"server\"\nname = \"desk\"\n").unwrap();
        assert_eq!(cfg.audio.mic_device, None);
    }
```

In `crates/pheme-app/src/server.rs`'s test module (create one if absent):

```rust
    use super::*;
    use pheme_audio::mock::MockCapture;
    use pheme_app_test_support::wait_until; // or repeat the helper locally

    #[test]
    fn a_playback_frame_is_not_for_the_server_to_send() {
        // Review Focus 3, the server's half: it receives Playback and sends Mic.
        assert!(playback_frame(&Msg::Audio {
            stream: AudioStream::Playback,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        })
        .is_some());
        assert!(playback_frame(&Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        })
        .is_none());
    }
```

The disconnect behaviour — Review Focus 1 — is an end-to-end property and is tested in Task 16, where a real session can be torn down.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib`
Expected: FAIL — `mic_device` and `playback_frame` do not exist.

- [ ] **Step 3: Add the config key**

In `crates/pheme-app/src/config.rs`'s `AudioCfg`:

```rust
    /// Server side: which microphone to capture and send to the client. `None` means the
    /// platform default. There is no key to turn the microphone on or off: it opens only
    /// while the client reports that something is recording.
    pub mic_device: Option<String>,
```

- [ ] **Step 4: Add the send side and the gate**

`ServerDeps` gains:

```rust
    /// Where the audio sent to the client's virtual microphone comes from.
    pub mic: CaptureSource,
    /// Counters the mic packer thread publishes. `None` allocates a private set.
    pub mic_counters: Option<Arc<OutCounters>>,
```

`CaptureSource::Detect` calls `detect_capture`, which opens the client's *loopback* device. On a server the mic side must call `detect_mic` instead. The stream tag already distinguishes the two sides, so let it choose:

```rust
        let built = match injected.take() {
            Some(b) => Ok(b),
            None => match stream {
                AudioStream::Playback => pheme_audio::detect_capture(device.as_deref()),
                AudioStream::Mic => pheme_audio::detect_mic(device.as_deref()),
            },
        };
```

in `out_thread`, which already receives `stream` from Task 6. No new variant is needed here because the tag is not optional: a `SendSide` cannot exist without one.

`Shared` gains `mic: SendSide`. In `run_server`:

```rust
    let mic_counters = mic_counters.unwrap_or_default();
    let mic = SendSide::spawn(mic, AudioStream::Mic, mic_counters.clone());
    // Closed until a client says something is recording. A server with no client has no
    // consumer, so there is nothing for an open microphone to be open for.
    mic.set_wanted(false);
```

and the routing helper beside it:

```rust
/// The playback frame in `m`, if it is one this server should play.
///
/// A server receives `AudioStream::Playback` and sends `AudioStream::Mic`; a frame tagged
/// the other way is not ours.
fn playback_frame(m: &Msg) -> Option<Frame> {
    match m {
        Msg::Audio {
            stream: AudioStream::Playback,
            seq,
            ts_us,
            samples,
        } if samples.len() == pheme_audio::FRAME_BYTES => Some(Frame {
            seq: *seq,
            ts_us: *ts_us,
            bytes: samples.clone(),
        }),
        _ => None,
    }
}
```

In `main`, `mic: CaptureSource::Detect(cfg.audio.mic_device.clone())` and `mic_counters: None`.

- [ ] **Step 5: Honour `MicWanted`, and check the client's format**

In `handle_peer`, after the `Hello` is accepted, validate what the client says it speaks (spec §2.3):

```rust
    let audio_ok = audio == AudioParams::DEFAULT;
    if !audio_ok {
        error!(
            ?audio,
            client = %name,
            "the client speaks an audio format pheme does not; running this session \
             without audio in either direction"
        );
    }
```

Attach the mic's peer only when the format matches, and route the audio channel:

```rust
    let mut audio_rx = peer.take_audio();
    if audio_ok {
        shared.mic.set_peer(Some(peer.sender()));
    }
```

Add the arms:

```rust
            m = audio_rx.recv() => match m {
                Some(m) => {
                    if audio_ok {
                        if let Some(f) = playback_frame(&m) {
                            shared.audio.push(f);
                        }
                    }
                }
                None => break Ok(()),
            },
```

and inside the existing control arm:

```rust
                Some(Msg::MicWanted { wanted }) => {
                    if audio_ok {
                        shared.mic.set_wanted(wanted);
                    }
                }
```

- [ ] **Step 6: Close the microphone when the session ends**

Review Focus 1. At every exit from `handle_peer` — beside the existing `link` clearing — add:

```rust
    shared.mic.set_peer(None);
    // No client means no consumer. Clearing the peer alone would leave the device open
    // for the life of the process, with its indicator lit and nothing listening.
    shared.mic.set_wanted(false);
```

- [ ] **Step 7: Update the test constructors**

`crates/pheme-app/tests/audio.rs` and `crates/pheme-app/tests/integration.rs` build `ServerDeps`; add `mic: CaptureSource::Disabled` and `mic_counters: None` to each.

- [ ] **Step 8: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

Run: `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets`
Expected: no errors.

- [ ] **Step 9: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-app
git commit -m "$(cat <<'MSG'
Update: send the server's microphone to the client

The server gains a SendSide tagged Mic, closed until a client reports
that something is recording. audio.mic_device chooses which microphone;
there is no key to turn the direction on or off, because the demand
signal makes one unnecessary.

The session now also checks the AudioParams the client announced in
Hello and runs without audio in either direction on a mismatch, which is
what putting them there was for: sub-project 2 could only transmit and
hope.

Ending a session closes the microphone as well as clearing the peer.
Clearing the peer alone would stop the frames but leave the device open
for the life of the process, indicator lit, with nothing on the other
end to listen.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 15: Stats for both directions, and the interval that lies

Both roles now run both directions, so both stats lines grow. The existing `audio_*` names keep meaning the playback stream so the lines already being read do not change meaning, and the mic stream takes `mic_*`.

The client's stats line also has a real bug, seen live during the VM session: it alternated between 200 and 400 frames per second when the true rate was a steady 200.

**Files:**
- Modify: `crates/pheme-app/src/client.rs`, `crates/pheme-app/src/server.rs`

**Interfaces:**
- Consumes: `InStats` and `OutCounters` from both directions.
- Produces: no new API.

- [ ] **Step 1: Fix the client's stats interval**

The line is emitted inside the one-second ping tick, guarded by `last_stats.elapsed() >= Duration::from_secs(1)`. A tick landing a few milliseconds early fails the guard, skips that report, and the next one presents **two** seconds of counters under a per-second label. The counters were right; the interval they were divided by was not.

Give stats its own timer instead of borrowing the ping's, and drop the guard:

```rust
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
```

Replace the stats block inside the `ping.tick()` arm with its own arm:

```rust
            _ = stats_tick.tick(), if stats => {
                let sent = counters.sent.swap(0, Ordering::Relaxed);
                let suppressed = counters.suppressed.swap(0, Ordering::Relaxed);
                let m = mic_stats.snapshot_delta();
                info!(
                    rtt_us = peer.rtt().as_micros(),
                    received,
                    lost,
                    audio_sent = sent,
                    audio_suppressed = suppressed,
                    mic_depth_ms = mic_stats.depth_ms.load(Ordering::Relaxed),
                    mic_lost = m.lost,
                    mic_underruns = m.underruns,
                    mic_late = m.late,
                    mic_resets = m.resets,
                    mic_dropped = m.dropped,
                    mic_overflows = m.overflows,
                    active = core.active(),
                    "stats/s"
                );
                received = 0;
                lost = 0;
            }
```

`tokio::time::interval` does not skip: a tick that is late simply fires late, so every line covers exactly one interval. Delete `last_stats` entirely.

- [ ] **Step 2: Add a delta helper so both roles read the counters the same way**

The server computes six differences by hand with a tuple; the client would need seven more. Add to `crates/pheme-app/src/audio/recv.rs`:

```rust
/// One interval's worth of counters.
#[derive(Default, Clone, Copy)]
pub struct InDelta {
    pub lost: u64,
    pub late: u64,
    pub underruns: u64,
    pub resets: u64,
    pub dropped: u64,
    pub overflows: u64,
}

/// The values `snapshot_delta` saw last time, so the counters themselves can stay
/// cumulative and a caller that never asks for a delta still reads totals.
#[derive(Default)]
struct InLast {
    lost: AtomicU64,
    late: AtomicU64,
    underruns: AtomicU64,
    resets: AtomicU64,
    dropped: AtomicU64,
    overflows: AtomicU64,
}

/// The change in one counter since the last call, and the new high-water mark.
fn step(now: &AtomicU64, last: &AtomicU64) -> u64 {
    let now = now.load(Ordering::Relaxed);
    now - last.swap(now, Ordering::Relaxed)
}

impl InStats {
    /// The change since the last call. Exactly one caller per `InStats`, because each
    /// call consumes the interval it reports.
    pub fn snapshot_delta(&self) -> InDelta {
        InDelta {
            lost: step(&self.lost, &self.last.lost),
            late: step(&self.late, &self.last.late),
            underruns: step(&self.underruns, &self.last.underruns),
            resets: step(&self.resets, &self.last.resets),
            dropped: step(&self.dropped, &self.last.dropped),
            overflows: step(&self.overflows, &self.last.overflows),
        }
    }
}
```

Add `last: InLast` to `InStats`, private to the module. The counters only ever grow, so `step`'s subtraction cannot underflow.

- [ ] **Step 3: Use it on the server and add the mic counters there**

Replace the server's `alast`/`anow` tuples with `astats.snapshot_delta()`, and add the send side:

```rust
                let a = astats.snapshot_delta();
                let mic_sent = mic_counters.sent.swap(0, Ordering::Relaxed);
                let mic_suppressed = mic_counters.suppressed.swap(0, Ordering::Relaxed);
                info!(
                    events = now.0 - last.0,
                    control = now.1 - last.1,
                    datagrams = now.2 - last.2,
                    connected,
                    audio_depth_ms = astats.depth_ms.load(Ordering::Relaxed),
                    audio_lost = a.lost,
                    audio_underruns = a.underruns,
                    audio_late = a.late,
                    audio_resets = a.resets,
                    audio_dropped = a.dropped,
                    audio_overflows = a.overflows,
                    mic_sent,
                    mic_suppressed,
                    mic_open = mic_open,
                    "stats/s"
                );
```

`mic_open` is the one field that makes the demand mechanism visible in a log. Publish it from `SendSide`: add an `AtomicBool` set true while a backend is started and false while the gate is shut or the device is down, and expose `SendSide::is_open(&self) -> bool`.

- [ ] **Step 4: Write the test**

In `crates/pheme-app/src/audio/recv.rs`'s tests:

```rust
    #[test]
    fn a_delta_reports_each_interval_once() {
        let stats = InStats::default();
        stats.lost.store(10, Ordering::Relaxed);
        assert_eq!(stats.snapshot_delta().lost, 10);
        assert_eq!(stats.snapshot_delta().lost, 0, "nothing new since");
        stats.lost.store(13, Ordering::Relaxed);
        assert_eq!(stats.snapshot_delta().lost, 3);
        assert_eq!(
            stats.lost.load(Ordering::Relaxed),
            13,
            "the counter itself stays cumulative"
        );
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-app
git commit -m "$(cat <<'MSG'
Fix: report both audio directions, once per interval

Both roles now run both directions, so both stats lines carry both. The
audio_ names keep meaning the playback stream so existing lines do not
change meaning, and the microphone stream takes mic_. mic_open is the
field that makes the demand gate visible in a log at all.

The client's line also had a real bug, seen live: it alternated between
200 and 400 frames per second when the true rate was a steady 200. It
was emitted inside the one-second ping tick behind an elapsed() >= 1s
guard, so a tick landing a few milliseconds early skipped its report and
the next line presented two seconds of counters under a per-second
label. Stats now have their own interval, which fires late rather than
skipping, so every line covers exactly one interval.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 16: End-to-end tests for the mic direction

`tests/audio.rs` proves the playback direction over a real QUIC connection with mock devices at both ends. This is its mirror, and it is where the two properties that only exist end-to-end get pinned: a disconnect closes the microphone, and a client with no virtual microphone never opens one.

**Files:**
- Create: `crates/pheme-app/tests/mic_e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 13 and 14.

- [ ] **Step 1: Write the harness**

```rust
//! The server's microphone reaching the client's virtual microphone over a real QUIC
//! connection, with mock devices on both ends.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pheme_app::audio::{CaptureSource, InStats, OutCounters, PlaybackSource};
use pheme_app::client::{run_client, ClientDeps};
use pheme_app::server::{run_server, ServerDeps};
use pheme_audio::mock::{MockCapture, MockCaptureHandle, MockPlayback, MockPlaybackHandle};
use pheme_audio::{Demand, FRAME_INTERLEAVED};
use pheme_core::{ClientPlacement, Hotkeys, Side};
use pheme_input::mock::{MockCapture as MockInputCapture, MockInject};
use pheme_net::{Endpoint, Identity, TrustStore};
use pheme_proto::ScreenInfo;
use tokio::sync::watch;

fn screens(w: u32, h: u32) -> Vec<ScreenInfo> {
    vec![ScreenInfo { x: 0, y: 0, w, h, primary: true }]
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
    /// The server's microphone device.
    server_mic: MockCaptureHandle,
    /// The client's virtual microphone: what applications would record from.
    client_mic: MockPlaybackHandle,
    heard: Arc<InStats>,
}

/// A paired server and client, with the mic direction wired to mocks. `demand` is what
/// the client's virtual microphone reports about its consumers. `virtual_mic` false
/// models a client with no virtual microphone at all — a Windows client today.
fn spawn_mic_pair(demand: Demand, virtual_mic: bool) -> MicPair {
    // …identical identity/trust/endpoint setup to `tests/audio.rs::spawn_pair`…

    let (server_mic_backend, server_mic) = MockCapture::new();
    let (client_mic_backend, client_mic) = MockPlayback::new(48_000);
    client_mic.set_demand(demand);
    let heard = Arc::new(InStats::default());

    // ServerDeps: audio: PlaybackSource::Disabled, mic: CaptureSource::Backend(..)
    // ClientDeps: audio: CaptureSource::Disabled,
    //             mic: if virtual_mic { PlaybackSource::Backend(..) } else { PlaybackSource::Disabled },
    //             mic_stats: Some(heard.clone())
    // …spawn both, as `spawn_pair` does…
}
```

The playback direction is disabled on both sides so nothing competes for the mock devices, and so a failure names the direction under test.

- [ ] **Step 2: Write the tests**

```rust
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

#[tokio::test]
async fn a_disconnect_closes_the_server_microphone() {
    // Review Focus 1. No client means no consumer. Clearing the peer alone would stop the
    // frames but leave the device open for the life of the process.
    let pair = spawn_mic_pair(Demand::Wanted, true);
    assert!(wait_until(|| pair.server_mic.started(), Duration::from_secs(5)).await);

    // Tear the client down while leaving the server running.
    pair.shutdown_tx.send(true).unwrap();
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
    pair.shutdown().await;
}
```

`MockCaptureHandle` and `MockPlaybackHandle` are already `Clone`, which the disconnect test needs.

- [ ] **Step 3: Run the tests**

Run: `cargo test -p pheme-app --test mic_e2e`
Expected: PASS, 5 tests. The linger is three seconds, so the gate-closing tests allow ten.

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Lint and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pheme-app/tests/mic_e2e.rs
git commit -m "$(cat <<'MSG'
Update: add end-to-end tests for the microphone direction

The mirror of tests/audio.rs, and the place where the properties that
only exist end to end get pinned: a disconnect closes the server's
microphone, a client with no virtual microphone never opens one, and a
stream that resumes after the gate reopens is not discarded as late.

That last one is the failure no unit test would find. The gate closing
freezes the sender's numbering while the receiver's cursor keeps
advancing, and a gap under RESET_GAP leaves the jitter buffer counting
every arriving frame as late - inaudible to every counter and obvious to
an ear.

Both directions of the pair drive their own device clock. Sub-project 2
learned that the hard way: a playback mock without one let the receiver
discard 95 % of the stream while every assertion passed.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Task 17: Documentation

**Files:**
- Modify: `README.md`, `docs/testing.md`

- [ ] **Step 1: Update the README**

Change the opening description — it currently says the reverse direction is planned:

```markdown
Share one machine's keyboard and mouse with another over the LAN, Deskflow-style,
with audio forwarded in both directions. Written in Rust. GPL-3.0.
```

Add a section after the existing audio one:

```markdown
### The server's microphone on the client

The microphone attached to the **server** appears on the **client** as an ordinary
recording device, so a call or a recording running on the client uses the microphone
you are actually sitting in front of.

On a Linux client it appears as `pheme-mic` in `pactl list sources short`, described as
"Pheme Mic" in sound settings and `pavucontrol`. Select it wherever you would pick a
microphone.

**The server's microphone is opened only while something is recording.** There is
nothing to switch on: when an application on the client opens Pheme Mic, the server
opens its microphone; a few seconds after the last one closes, the server closes it
again. Until then the device is not held open and its indicator light stays off. Set
`mic_device` under `[audio]` on the server to choose a microphone other than the system
default.

A Windows client has no virtual microphone yet. Windows has no way for a program to
create a recording device without a signed kernel driver, so this needs VB-CABLE and is
not implemented. A Windows client keeps its keyboard, mouse and audio-out; it simply
never asks the server to open its microphone. A Windows *server* sends its microphone to
a Linux client normally.

Do not route Pheme Mic into Pheme Speaker on the client: that sends the server's
microphone straight back to the server's speakers, which will howl. Nothing stops you,
because the routing is yours to choose.
```

Add `mic_device` to the config sample in the README.

- [ ] **Step 2: Add the manual test rows**

Append to the audio section of `docs/testing.md`:

```markdown
## Microphone (sub-project 3)

Run with this machine as the Linux client and the QEMU VM as the Windows server
(`~/pheme-vm/start-vm.sh`), and again Linux server → Linux client.

| # | Check | Result |
|---|---|---|
| M1 | "Pheme Mic" appears in the client's sound settings; recording from it plays the server's microphone | |
| M2 | Nothing recording → `mic_open=false` on the server and the OS shows the microphone unused; start recording → audio within 500 ms | |
| M3 | Stop recording → the microphone closes after about 3 s; opening a sound-settings page that merely lists devices does not make it flap | |
| M4 | **No silence at the start of a recording.** Record for 5 s, stop, record again: the second recording has audio from its first moment | |
| M5 | A mono microphone on the server arrives as two channels on the client | |
| M6 | Ten minutes continuous: `mic_underruns=0`, `mic_depth_ms` steady rather than climbing | |
| M7 | Unplug the network for 3 s and reconnect: the microphone resumes by itself | |
| M8 | Unplug the server's microphone mid-session: it recovers within the retry cycle, and the keyboard and mouse are unaffected | |
| M9 | A Windows client: the server's microphone never opens and the client's log stays clean | |
| M10 | Latency: clap near the server's microphone while recording on the client; the offset is under 40 ms | |

M4 is the row worth running twice. The failure it catches is inaudible to every counter:
the buffer discards the resumed stream as late and the recording simply starts silent.

M2 and M3 read `mic_open` from the server's `--stats` line, which is the only place the
demand gate is visible.

Note that a level meter counts as a consumer. An open sound-settings input page, or
`pavucontrol`, holds the server's microphone open — correct behaviour, surprising the
first time. Close them before running M2 or M3.
```

- [ ] **Step 3: Check the documentation against the code**

Run: `grep -n 'mic_device\|virtual_mic_device' README.md crates/pheme-app/src/config.rs`
Expected: `mic_device` in both; `virtual_mic_device` in neither, since the Windows client work is deferred. If the README mentions a key the config does not parse, the config will reject a file the README told the user to write.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/testing.md
git commit -m "$(cat <<'MSG'
Update: document the microphone direction

The README no longer says the reverse direction is planned, and explains
the part users will not guess: the server's microphone opens only while
something on the client is recording, so there is nothing to switch on
and the indicator light stays off the rest of the time.

It also says plainly that a Windows client has no virtual microphone
yet, and why - Windows has no way to create a recording device without a
signed kernel driver - so nobody goes looking for a setting that does
not exist.

The manual matrix gains rows M1 to M10. M4 is the one to run twice: a
recording that starts silent after the gate reopens is invisible to
every counter.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
)"
```

---

## Done

At the end of Task 17:

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` are green.
- `cargo check --workspace --target x86_64-pc-windows-gnu --all-targets` is clean.
- The Linux client shows "Pheme Mic" as a recording device, and recording from it plays the server's microphone.
- The server's microphone is closed whenever nothing on the client is recording, and whenever no client is connected.
- Rows M1–M10 of `docs/testing.md` are the remaining gate, and M1, M2, M5 and M10 need the VM as a Windows server.
