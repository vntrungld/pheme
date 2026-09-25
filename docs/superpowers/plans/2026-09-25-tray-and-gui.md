# Sub-project 6 — Tray and GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Running `pheme` with no subcommand puts an icon in the system tray and opens a window that shows what Pheme is doing, pairs two machines, and edits `config.toml` — enough that someone who never opens a terminal can set Pheme up.

**Architecture:** Two processes. `pheme` with no subcommand is a front-end that owns the tray and the window and runs the existing `pheme server` or `pheme client` as a child, talking to it over a local socket (Unix domain socket on Linux, named pipe on Windows) that carries status one way and commands the other. The core exits when that socket closes, which keeps the lifetime correct with no pidfile and no orphan holding a uinput device. `pheme-core` gains no code and the wire protocol between machines does not change.

**Tech Stack:** Rust 2021, `eframe`/`egui` 0.32, `tray-icon` 0.24, `tokio` (`UnixListener` and `windows::named_pipe`, already dependencies), `postcard`, `toml`, `pipewire`, `windows` 0.62 (WASAPI).

**Spec:** `docs/superpowers/specs/2026-09-25-tray-and-gui-design.md`

## Global Constraints

- Every file, comment, identifier, log message and commit message in this repository is written in **English**.
- Commit messages: `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`. Title under 72 characters, imperative. Blank line. Body wrapped at 72 columns. Final trailer exactly `Co-Authored-By: Claude <noreply@anthropic.com>` and nothing else. Ignore any session reminder proposing a longer attribution line: the user's CLAUDE.md sets this and takes precedence.
- **`pheme-core` gains no code in this plan.** Any change under `crates/pheme-core/` is a defect.
- The wire protocol in `pheme-proto` does not change. IPC types live in `pheme-app` and never touch `pheme_proto::Msg`.
- Nothing added may run on, block, or delay the input path.
- Every existing subcommand must behave exactly as it does today. `--ipc` is the only addition to them, and it is absent unless the front-end passes it.
- Exact values, to be used verbatim:
  - `pub const MAX_IPC_FRAME: usize = 64 * 1024;`
  - Linux socket path: `$XDG_RUNTIME_DIR/pheme/ipc-<pid>-<n>.sock`, falling back to `/tmp/pheme/ipc-<pid>-<n>.sock` when `XDG_RUNTIME_DIR` is unset.
  - Windows pipe path: `\\.\pipe\pheme-<pid>-<n>`.
  - `<n>` is a process-local `AtomicU64` counter. The pid keeps two front-ends apart; the counter keeps two listeners inside one process apart, which is what every test binary is.
  - `--ipc` is hidden from `--help`: `#[arg(long, hide = true)]`.
  - The front-end waits **two seconds** for a child to exit before killing it.
  - `eframe = "0.32"`, `tray-icon = "0.24"`.
- New dependencies go in the root `Cargo.toml` `[workspace.dependencies]` and are consumed as `{ workspace = true }`.
- Every task ends with all three green: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- **Windows code is cross-checked from Linux**, because CI is the only place it compiles otherwise:
  ```
  RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone \
  PATH=$HOME/.cargo-standalone/bin:$PATH \
  cargo clippy -p <crate> --target x86_64-pc-windows-gnu --all-targets -- -D warnings
  ```
  This type-checks. It does not prove behaviour: anything Windows-specific belongs in the manual matrix, not in a claim of correctness.

  **It only works for `pheme-audio` and `pheme-input`.** For `pheme-app` and `pheme-net` it fails before reaching any of this plan's code: they depend on `ring` through `quinn`, whose build needs `x86_64-w64-mingw32-gcc`, which is not installed on this machine and cannot be installed from inside a task. Verified against plain `master` with no changes applied, so it is the environment and not anything written here. For `pheme-app`'s Windows arms — the named pipe, and anything later tasks add — **CI's `windows-latest` job is the only compile check there is**. State that in the report rather than substituting a check that proves less and reads like it proves the same.

## Review Focus

Five things the spec implies, that a person will meet, and that no happy-path test would catch. Each has a test in the task that owns the code.

1. **The front-end starts with no `config.toml` at all** — the first run, every time, for every new user. Expected: the window opens, the tray appears, and no child is spawned until a configuration exists. Never a crash and never a child started with a guessed role. — Task 8.
2. **The child exits on its own** — a crash, a missing uinput permission, a port already bound. Expected: the front-end notices within a second and shows why, rather than waiting for a status that will never arrive. — Task 8.
3. **A second front-end is started while one is running** — the pid in the socket path stops them colliding, but the second child will fail to bind `listen`. Expected: that failure reaches the window as text, not silence. — Task 8.
4. **`Config::save` when the configuration directory does not exist** — again, the first run. Expected: the directory is created and the file written, not an error about a missing path. — Task 3.
5. **A `Status` arriving from a core of the previous role**, after the role was changed and the child restarted. Expected: the window shows nothing stale — no peer name or RTT from the run before. — Task 10.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `crates/pheme-app/src/ipc/mod.rs` | the IPC module's public surface |
| `crates/pheme-app/src/ipc/proto.rs` | `Status`, `LinkState`, `Command`, `MAX_IPC_FRAME`, encode/decode |
| `crates/pheme-app/src/ipc/transport.rs` | the platform socket: listener on the front-end, connector on the core |
| `crates/pheme-app/src/frontend/mod.rs` | the front-end: supervisor, tray, window |
| `crates/pheme-app/src/frontend/supervisor.rs` | spawning, watching and restarting the child |
| `crates/pheme-app/src/frontend/tray.rs` | the tray icon and its menu |
| `crates/pheme-app/src/frontend/window.rs` | the `eframe` application and its three panels |
| `crates/pheme-audio/src/devices.rs` | `list_devices` and its two platform backends |
| `crates/pheme-app/tests/ipc.rs` | the socket end to end, headless |
| `crates/pheme-app/tests/supervisor.rs` | the supervisor end to end, headless |

**Modified:**

| File | Change |
|---|---|
| `Cargo.toml` | `eframe`, `tray-icon` |
| `crates/pheme-app/Cargo.toml` | the same, plus nothing else |
| `crates/pheme-app/src/lib.rs` | `pub mod frontend; pub mod ipc;` |
| `crates/pheme-app/src/main.rs` | `--ipc` on `server`/`client`, `Cmd::Devices`, and the no-subcommand case |
| `crates/pheme-app/src/server.rs` | `ServerDeps.status`, command handling, exit on socket close |
| `crates/pheme-app/src/client.rs` | the same on the client |
| `crates/pheme-app/src/config.rs` | `Config::save` |
| `crates/pheme-audio/src/lib.rs` | `pub mod devices;` and its re-exports |
| `README.md`, `docs/testing.md` | documentation |

---

## Task 1: The IPC protocol

Pure types and framing, with no socket and no platform code, so all of it is testable.

**Files:**
- Create: `crates/pheme-app/src/ipc/mod.rs`
- Create: `crates/pheme-app/src/ipc/proto.rs`
- Modify: `crates/pheme-app/src/lib.rs`

**Interfaces:**
- Consumes: `pheme_app::config::Role` (already exists, derives `Serialize`/`Deserialize`).
- Produces: `pheme_app::ipc::{Status, LinkState, Command, MAX_IPC_FRAME, encode_frame, decode_frame}`.

- [ ] **Step 1: Write the failing tests**

`crates/pheme-app/src/ipc/proto.rs`, tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> Status {
        Status {
            role: Role::Server,
            state: LinkState::Connected,
            peer: Some("laptop-win".into()),
            rtt_us: 412,
            locked: false,
            events: 91,
            lost: 0,
            audio_depth_ms: 22,
            audio_lost: 3,
            mic_depth_ms: 0,
            mic_lost: 0,
        }
    }

    #[test]
    fn a_status_survives_a_round_trip() {
        let mut buf = Vec::new();
        encode_frame(&sample_status(), &mut buf).unwrap();
        let (m, used) = decode_frame::<Status>(&buf).unwrap().unwrap();
        assert_eq!(m, sample_status());
        assert_eq!(used, buf.len(), "the whole frame was consumed");
    }

    #[test]
    fn every_command_survives_a_round_trip() {
        for c in [Command::Lock, Command::Unlock, Command::Stop] {
            let mut buf = Vec::new();
            encode_frame(&c, &mut buf).unwrap();
            let (got, _) = decode_frame::<Command>(&buf).unwrap().unwrap();
            assert_eq!(got, c);
        }
    }

    #[test]
    fn a_failure_message_survives_a_round_trip() {
        // The reason a link failed is the one thing a person needs and today
        // it exists only in the log, so it must cross intact.
        let s = Status {
            state: LinkState::Failed("address already in use".into()),
            ..sample_status()
        };
        let mut buf = Vec::new();
        encode_frame(&s, &mut buf).unwrap();
        let (got, _) = decode_frame::<Status>(&buf).unwrap().unwrap();
        assert_eq!(got.state, LinkState::Failed("address already in use".into()));
    }

    #[test]
    fn an_incomplete_frame_asks_for_more_rather_than_failing() {
        // A socket read can stop anywhere. Half a frame is not an error.
        let mut buf = Vec::new();
        encode_frame(&sample_status(), &mut buf).unwrap();
        for cut in [0, 1, 3, buf.len() - 1] {
            assert!(
                decode_frame::<Status>(&buf[..cut]).unwrap().is_none(),
                "{cut} bytes should have been treated as incomplete"
            );
        }
    }

    #[test]
    fn two_frames_in_one_buffer_are_read_one_at_a_time() {
        let mut buf = Vec::new();
        encode_frame(&Command::Lock, &mut buf).unwrap();
        let first_len = buf.len();
        encode_frame(&Command::Stop, &mut buf).unwrap();
        let (a, used) = decode_frame::<Command>(&buf).unwrap().unwrap();
        assert_eq!(a, Command::Lock);
        assert_eq!(used, first_len);
        let (b, _) = decode_frame::<Command>(&buf[used..]).unwrap().unwrap();
        assert_eq!(b, Command::Stop);
    }

    #[test]
    fn an_oversized_frame_is_refused_without_being_decoded() {
        // The length prefix is attacker-controlled once the socket is open.
        // Refusing on the prefix alone is what stops a reader allocating on it.
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_IPC_FRAME + 1) as u32).to_le_bytes());
        assert!(decode_frame::<Status>(&buf).is_err());
    }

    #[test]
    fn a_frame_of_rubbish_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert!(decode_frame::<Status>(&buf).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib ipc`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the protocol**

`crates/pheme-app/src/ipc/proto.rs`, above the tests:

```rust
//! What the front-end and the core say to each other.
//!
//! Deliberately not `pheme_proto::Msg`. That enum is the protocol between two
//! machines; these types exist only for a local GUI and must never make the
//! wire protocol carry a field for its benefit. Sub-project 6 design §4.

use serde::{Deserialize, Serialize};

use crate::config::Role;

/// The largest IPC frame either side will send or accept.
///
/// About a thousand times the largest `Status`. It exists to bound a reader
/// against a length prefix the other end controls, not to carry anything.
pub const MAX_IPC_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkState {
    Starting,
    Listening,
    Connecting,
    Connected,
    /// The core is running but the link failed, and this is why.
    Failed(String),
}

/// Pushed by the core once a second, and once immediately on connect.
///
/// Not every field is meaningful to every role, and the spec fixes which:
/// `lost` is counted only by the client, which numbers the gaps in the
/// server's sequence, so a server sends `0`. `audio_*` describes the stream
/// the server plays and `mic_*` the stream the client plays, so each side
/// fills the pair it owns and sends `0` for the other. A zero therefore means
/// "not measured here", and the window labels fields by role rather than
/// showing a misleading nought for something the other end would have counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub role: Role,
    pub state: LinkState,
    pub peer: Option<String>,
    pub rtt_us: u64,
    pub locked: bool,
    pub events: u64,
    pub lost: u64,
    pub audio_depth_ms: u32,
    pub audio_lost: u64,
    pub mic_depth_ms: u32,
    pub mic_lost: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Lock,
    Unlock,
    /// Stop cleanly. The front-end sends this before restarting the child with
    /// a changed configuration.
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("ipc frame of {0} bytes exceeds the {MAX_IPC_FRAME} byte limit")]
    TooLong(usize),
    #[error("ipc encoding: {0}")]
    Codec(#[from] postcard::Error),
    #[error("ipc io: {0}")]
    Io(#[from] std::io::Error),
}

/// Appends one length-prefixed frame to `buf`.
pub fn encode_frame<T: Serialize>(value: &T, buf: &mut Vec<u8>) -> Result<(), IpcError> {
    let body = postcard::to_stdvec(value)?;
    if body.len() > MAX_IPC_FRAME {
        return Err(IpcError::TooLong(body.len()));
    }
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(())
}

/// Reads one frame from the front of `buf`.
///
/// `Ok(None)` means the buffer holds less than a whole frame, which is the
/// normal state of a socket read and not an error. `Ok(Some((value, used)))`
/// returns the value and how many bytes it consumed, so the caller can drain
/// exactly that much and look again.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, IpcError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // Checked before the body is looked at, let alone allocated: the prefix
    // comes from the other end of the socket.
    if len > MAX_IPC_FRAME {
        return Err(IpcError::TooLong(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let value = postcard::from_bytes(&buf[4..4 + len])?;
    Ok(Some((value, 4 + len)))
}
```

`crates/pheme-app/src/ipc/mod.rs`:

```rust
//! The local socket between the front-end and the core it supervises.

mod proto;

pub use proto::{decode_frame, encode_frame, Command, IpcError, LinkState, Status, MAX_IPC_FRAME};
```

Add `pub mod ipc;` to `crates/pheme-app/src/lib.rs`, keeping the module list sorted.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib ipc`
Expected: PASS, 7 tests.

- [ ] **Step 5: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/pheme-app/src/ipc crates/pheme-app/src/lib.rs
git commit -F - <<'MSG'
Update: add the front-end IPC protocol

Status one way, commands the other, length-prefixed and postcard-encoded
like the control stream. The types live in pheme-app and deliberately do
not extend pheme_proto::Msg: that enum is the protocol between two
machines and must not grow a field that exists only for a local GUI.

The length prefix is checked before the body is looked at or allocated,
because once the socket is open that prefix comes from the other end. An
incomplete buffer reports that it needs more rather than failing, since
a socket read can stop anywhere, and decode reports how many bytes it
consumed so a caller holding two frames can take them one at a time.

Status carries fields that only one role measures. Rather than leave
that to be discovered, the doc comment says which: the client counts
lost, the server fills the audio pair, the client fills the mic pair,
and a zero means not measured here.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 2: The IPC socket

The platform transport. One place where `cfg(target_os)` appears, and it is in `pheme-app`, never in `pheme-core`.

**Files:**
- Create: `crates/pheme-app/src/ipc/transport.rs`
- Modify: `crates/pheme-app/src/ipc/mod.rs`
- Create: `crates/pheme-app/tests/ipc.rs`

**Interfaces:**
- Consumes: `Status`, `Command`, `encode_frame`, `decode_frame`, `MAX_IPC_FRAME`, `IpcError` from Task 1.
- Produces:
  - `pheme_app::ipc::IpcListener` with `async fn bind() -> Result<IpcListener, IpcError>`, `fn path(&self) -> &str`, `async fn accept(&mut self) -> Result<IpcConnection, IpcError>`.
  - `pheme_app::ipc::IpcConnection` with `async fn send_command(&mut self, c: Command) -> Result<(), IpcError>`, `async fn recv_status(&mut self) -> Result<Option<Status>, IpcError>`.
  - `pheme_app::ipc::CoreLink` with `async fn connect(path: &str) -> Result<CoreLink, IpcError>`, `async fn send_status(&mut self, s: &Status) -> Result<(), IpcError>`, `async fn recv_command(&mut self) -> Result<Option<Command>, IpcError>`.
  - `Ok(None)` from either `recv_*` means the peer closed the connection.

- [ ] **Step 1: Write the failing integration test**

`crates/pheme-app/tests/ipc.rs`:

```rust
use std::time::Duration;

use pheme_app::config::Role;
use pheme_app::ipc::{Command, CoreLink, IpcListener, LinkState, Status};

fn sample() -> Status {
    Status {
        role: Role::Server,
        state: LinkState::Connected,
        peer: Some("laptop-win".into()),
        rtt_us: 412,
        locked: false,
        events: 91,
        lost: 0,
        audio_depth_ms: 22,
        audio_lost: 3,
        mic_depth_ms: 0,
        mic_lost: 0,
    }
}

#[tokio::test]
async fn status_and_commands_cross_the_socket() {
    let mut listener = IpcListener::bind().await.unwrap();
    let path = listener.path().to_string();

    let core = tokio::spawn(async move {
        let mut link = CoreLink::connect(&path).await.unwrap();
        link.send_status(&sample()).await.unwrap();
        let c = link.recv_command().await.unwrap();
        assert_eq!(c, Some(Command::Lock));
        // A second status after the command, to prove the connection is still
        // usable in both directions rather than one-shot.
        link.send_status(&sample()).await.unwrap();
    });

    let mut conn = listener.accept().await.unwrap();
    assert_eq!(conn.recv_status().await.unwrap(), Some(sample()));
    conn.send_command(Command::Lock).await.unwrap();
    assert_eq!(conn.recv_status().await.unwrap(), Some(sample()));
    core.await.unwrap();
}

#[tokio::test]
async fn the_core_sees_the_front_end_go_away() {
    // This is the whole lifetime rule: the core exits when the socket closes,
    // so it must be able to tell that it did.
    let mut listener = IpcListener::bind().await.unwrap();
    let path = listener.path().to_string();

    let core = tokio::spawn(async move {
        let mut link = CoreLink::connect(&path).await.unwrap();
        // Blocks until the other end is gone, then reports the close rather
        // than an error.
        link.recv_command().await.unwrap()
    });

    let conn = listener.accept().await.unwrap();
    drop(conn);
    drop(listener);
    let got = tokio::time::timeout(Duration::from_secs(5), core)
        .await
        .expect("the core should notice within five seconds")
        .unwrap();
    assert_eq!(got, None, "a closed connection reads as None, not an error");
}

#[tokio::test]
async fn two_listeners_do_not_collide() {
    // The path carries the process id, so a second front-end in the same
    // session must not fail to bind. Within one process they would, which is
    // why bind() must also tolerate a stale socket file at its path.
    let a = IpcListener::bind().await.unwrap();
    let b = IpcListener::bind().await;
    assert!(b.is_ok(), "a second bind failed: {:?}", b.err());
    drop(a);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pheme-app --test ipc`
Expected: FAIL — `IpcListener` does not exist.

- [ ] **Step 3: Write the transport**

`crates/pheme-app/src/ipc/transport.rs`. The two platforms differ only in the listener and stream types; everything above that is shared.

```rust
//! The platform socket under the IPC protocol.
//!
//! A Unix domain socket on Linux and a named pipe on Windows, both from
//! `tokio::net`, which this crate already depends on. The front-end listens
//! and the core connects, so the core never has to guess when the front-end
//! appeared and the front-end owns the lifetime. Sub-project 6 design §4.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::proto::{decode_frame, encode_frame, Command, IpcError, Status, MAX_IPC_FRAME};
```

On Linux, `bind()` builds the path from `XDG_RUNTIME_DIR` (falling back to `/tmp`), creates the `pheme` directory, removes any stale socket file at that exact path, and binds a `tokio::net::UnixListener`. `Drop` removes the socket file. On Windows, `bind()` creates the first `NamedPipeServer` instance at `\\.\pipe\pheme-<pid>` and `accept()` waits for a client, then creates the next instance so a later connection has something to land on.

Both `IpcConnection` and `CoreLink` wrap a stream plus a `Vec<u8>` read buffer and share this loop, which is where the framing from Task 1 is used:

```rust
/// Reads one value, or `None` when the peer has closed the connection.
///
/// The buffer persists across calls because a read can stop mid-frame and a
/// single read can deliver two frames; `decode_frame` reports how much it
/// consumed so the remainder stays for the next call.
async fn recv<T, S>(stream: &mut S, buf: &mut Vec<u8>) -> Result<Option<T>, IpcError>
where
    T: for<'de> serde::Deserialize<'de>,
    S: AsyncReadExt + Unpin,
{
    loop {
        if let Some((value, used)) = decode_frame::<T>(buf)? {
            buf.drain(..used);
            return Ok(Some(value));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // A clean close. Anything already buffered was a partial frame and
            // is discarded with the connection.
            return Ok(None);
        }
        if buf.len() + n > MAX_IPC_FRAME * 2 {
            return Err(IpcError::TooLong(buf.len() + n));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn send<T, S>(stream: &mut S, value: &T) -> Result<(), IpcError>
where
    T: serde::Serialize,
    S: AsyncWriteExt + Unpin,
{
    let mut out = Vec::with_capacity(256);
    encode_frame(value, &mut out)?;
    stream.write_all(&out).await?;
    Ok(())
}
```

`IpcConnection::recv_status` and `CoreLink::recv_command` are `recv` at their own types; `send_command` and `send_status` are `send`.

Export the three types from `crates/pheme-app/src/ipc/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --test ipc`
Expected: PASS, 3 tests.

- [ ] **Step 5: Cross-check the Windows arm**

Run:
```
RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone \
PATH=$HOME/.cargo-standalone/bin:$PATH \
cargo clippy -p pheme-app --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```
Expected: clean. This type-checks the named-pipe arm; it does not run it. Say so in your report.

- [ ] **Step 6: Run the full gate and commit**

```bash
git add crates/pheme-app/src/ipc crates/pheme-app/tests/ipc.rs
git commit -F - <<'MSG'
Update: carry the front-end IPC over a local socket

A Unix domain socket on Linux and a named pipe on Windows, both from
tokio, which this crate already depends on. The front-end listens and
the core connects: the core never has to guess when the front-end
appeared, and the front-end owns the lifetime.

The socket path carries the front-end's process id so two front-ends in
one session do not collide, and bind() removes a stale socket file at
its own path rather than failing on the remains of a crash.

The read loop keeps its buffer across calls, because a socket read can
stop mid-frame and one read can deliver two. A clean close reads as None
rather than an error, which is what lets the core apply the rule that it
exits when the front-end goes away.

The Windows arm is type-checked from Linux against the gnu target. That
is not a claim it works; it belongs in the manual matrix.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 3: Writing the configuration

**Files:**
- Modify: `crates/pheme-app/src/config.rs`

**Interfaces:**
- Produces: `Config::save(&self, path: &Path) -> anyhow::Result<()>`.

- [ ] **Step 1: Write the failing tests**

In `crates/pheme-app/src/config.rs`'s test module:

```rust
    #[test]
    fn a_saved_config_loads_back_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.name = "desk-linux".into();
        c.connect = Some("laptop-win".into());
        c.clients.push(ClientCfg {
            name: "laptop-win".into(),
            side: SideCfg::Right,
            span: Some([0.25, 0.75]),
        });
        c.save(&path).unwrap();
        assert_eq!(Config::load(Some(&path)).unwrap(), c);
    }

    #[test]
    fn saving_creates_a_missing_directory() {
        // The first run has no config directory at all, and that is the one
        // run this path exists for.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("config.toml");
        Config::default().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "stray files: {left:?}");
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_intact() {
        // The rename is the whole point: a crash or a full disk between
        // truncating and finishing must not leave a config that no longer
        // parses, on the one path whose job is to keep the user out of an
        // editor. Simulated by making the destination a directory, so the
        // rename fails after the temporary file is written.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let blocked = dir.path().join("blocked.toml");
        std::fs::create_dir(&blocked).unwrap();
        assert!(Config::default().save(&blocked).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib config`
Expected: FAIL — `Config::save` is not defined.

- [ ] **Step 3: Write `save`**

In `impl Config`, beside `load`:

```rust
    /// Writes this configuration to `path`.
    ///
    /// Atomically: a temporary file in the same directory, then a rename. The
    /// rename is what makes it safe — writing in place means a crash or a full
    /// disk between truncating and finishing leaves a file that no longer
    /// parses, and the next start has no configuration at all. That is the one
    /// failure this path cannot afford, because its whole purpose is to keep
    /// the user out of a text editor.
    ///
    /// Comments in a hand-written file do not survive. The window warns before
    /// its first write; there is nothing to do about it here, since a TOML
    /// serializer has no comments to preserve.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self).context("serializing the configuration")?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Leave nothing behind on the way out: a stray .tmp beside a
                // config file invites someone to wonder which one is real.
                let _ = std::fs::remove_file(&tmp);
                Err(anyhow::Error::from(e)
                    .context(format!("replacing {}", path.display())))
            }
        }
    }
```

`tempfile` is already a dev-dependency of `pheme-app`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib config`
Expected: PASS, including the four new tests.

- [ ] **Step 5: Run the full gate and commit**

```bash
git add crates/pheme-app/src/config.rs
git commit -F - <<'MSG'
Update: write the configuration file atomically

Config::save writes a temporary file beside the destination and renames
it. The rename is the point: writing in place means a crash or a full
disk between truncating and finishing leaves a configuration that no
longer parses, and the next start has none at all — on the one path
whose whole purpose is to keep the user out of a text editor.

It creates the directory, because the first run has none, and that is
the run this exists for. A failed rename removes the temporary file
rather than leaving a stray .tmp beside a real config for someone to
wonder about.

Comments in a hand-written file do not survive a save. A TOML serializer
has none to preserve, so the warning belongs in the window rather than
here.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 4: `--ipc` in the core

The server and client learn to report status and obey commands. This is the task that makes the core supervisable.

**Files:**
- Modify: `crates/pheme-app/src/server.rs` (`ServerDeps`, the stats task around line 377, `main` around line 710)
- Modify: `crates/pheme-app/src/client.rs` (`ClientDeps`, the session's stats arm around line 375, `main` around line 412)
- Modify: `crates/pheme-app/src/main.rs` (the `Server` and `Client` subcommands)

**Interfaces:**
- Consumes: `CoreLink`, `Status`, `LinkState`, `Command` from Tasks 1–2.
- Produces: `ServerDeps.ipc: Option<PathBuf>` and `ClientDeps.ipc: Option<PathBuf>`; `pheme server --ipc <path>` and `pheme client --ipc <path>`.

- [ ] **Step 1: Add the hidden flag**

In `crates/pheme-app/src/main.rs`, add to both `Cmd::Server` and `Cmd::Client`:

```rust
        /// Report status to a front-end over this socket and take commands
        /// from it. Hidden because it is not something a person invokes: it is
        /// how `pheme` with no subcommand talks to the child it started.
        #[arg(long, hide = true)]
        ipc: Option<PathBuf>,
```

and pass it through to `server::main` / `client::main`, whose signatures gain an `ipc: Option<PathBuf>` parameter.

- [ ] **Step 2: Emit status from the server**

The server's per-second task is currently gated on `if stats {`. Change the gate to `if stats || ipc.is_some() {` and, inside, keep the existing `info!` line under `if stats` while building a `Status` and sending it when an IPC link exists. Read the existing block before editing and reuse the figures it already computes — `events`, `connected`, the audio snapshot — rather than recomputing them.

The `Status` a server sends:

```rust
                let status = Status {
                    role: Role::Server,
                    state: if connected { LinkState::Connected } else { LinkState::Listening },
                    peer: s.link.lock().unwrap().as_ref().map(|l| l.name.clone()),
                    rtt_us: 0,
                    locked: s.core.lock().unwrap().locked(),
                    events: now.0 - last.0,
                    // Only the client can count a gap in the other end's
                    // sequence, so a server reports none rather than a figure
                    // that would read as "nothing was lost".
                    lost: 0,
                    audio_depth_ms: astats.depth_ms.load(Ordering::Relaxed),
                    audio_lost: a.lost,
                    mic_depth_ms: 0,
                    mic_lost: 0,
                };
```

If `ServerCore` has no `locked()` accessor, add one — it is a one-line read of existing state and does not count as new behaviour in `pheme-core`. If adding it to `pheme-core` is the only way, **stop and report**: the plan forbids code there, and I will rule on it.

- [ ] **Step 3: Emit status from the client**

The same in the client's per-second arm, with `role: Role::Client`, `rtt_us: peer.rtt().as_micros() as u64`, `lost` from the existing counter, the mic pair filled and the audio pair zero.

- [ ] **Step 4: Handle commands and the socket closing**

Spawn one task per run that owns the `CoreLink`:

```rust
    // The core exits when the front-end goes away. That is the lifetime rule
    // from §3: no pidfile, no adoption, and no orphan left holding a uinput
    // device or a global keyboard hook.
    if let Some(path) = &ipc {
        let mut link = CoreLink::connect(&path.to_string_lossy()).await?;
        let shutdown_tx = shutdown_tx.clone();
        let lock = lock_handle.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Statuses arrive from the per-second task on this channel.
                    Some(s) = status_rx.recv() => {
                        if let Err(e) = link.send_status(&s).await {
                            debug!("status send failed: {e}");
                            break;
                        }
                    }
                    r = link.recv_command() => match r {
                        Ok(Some(Command::Lock)) => lock.set(true),
                        Ok(Some(Command::Unlock)) => lock.set(false),
                        // Stop and a closed socket are the same outcome and
                        // take the same path out: the shutdown signal Ctrl+C
                        // already uses. A second shutdown route would need its
                        // own proof that it releases the capture and the
                        // devices, and there is no reason to have one.
                        Ok(Some(Command::Stop)) | Ok(None) => break,
                        Err(e) => {
                            debug!("ipc read failed: {e}");
                            break;
                        }
                    },
                    else => break,
                }
            }
            let _ = shutdown_tx.send(true);
        });
    }
```

`status_rx` is the receiving half of an `mpsc::channel(4)` whose sender the
per-second task holds; a full channel drops the oldest status, because only
the newest is worth showing. `lock_handle` is whatever the lock hotkey
already calls — find it and reuse it rather than reaching into the core a
second way.

Lock and Unlock reach the same place the lock hotkey does. `Stop` sends the existing shutdown watch signal, exactly as Ctrl+C does — do not invent a second shutdown path.

- [ ] **Step 5: Verify by hand**

In one terminal, run a tiny listener using the test harness from Task 2; in another, `cargo run -p pheme-app --bin pheme -- server --ipc <path>`. Confirm a `Status` arrives each second and that closing the listener makes the server exit. Report exactly what you observed.

- [ ] **Step 6: Run the full gate, cross-check Windows, and commit**

```bash
git add crates/pheme-app/src
git commit -F - <<'MSG'
Update: report status and take commands over the IPC socket

pheme server and pheme client gain a hidden --ipc flag. With it they
connect to the front-end's socket, push a Status every second and obey
Lock, Unlock and Stop. Without it — every existing invocation — nothing
changes.

The status comes from the figures the per-second stats task already
computes, so the window shows the numbers this project already trusts
rather than a second set. The task's gate widens from --stats to
"--stats or --ipc"; the log line stays behind --stats alone.

The core exits when the socket closes. That is the lifetime rule: the
front-end is the supervisor, and this keeps it correct with no pidfile,
no adoption protocol, and no orphan holding a uinput device or a global
keyboard hook. Stop and a closed socket both reach the shutdown signal
Ctrl+C already uses, rather than a second path that would need its own
proof.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 5: Audio device enumeration — shape and Linux

**Files:**
- Create: `crates/pheme-audio/src/devices.rs`
- Modify: `crates/pheme-audio/src/lib.rs`

**Interfaces:**
- Produces: `pheme_audio::devices::{DeviceInfo, DeviceKind, list_devices}`. `list_devices() -> Result<Vec<DeviceInfo>, Error>` where `Error` is `pheme-audio`'s existing error type.

- [ ] **Step 1: Write the failing tests**

`crates/pheme-audio/src/devices.rs`, tests for the pure part:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_sort_playback_first_then_by_name() {
        // The window's menus read better grouped, and a stable order means a
        // menu does not reshuffle between openings.
        let mut v = vec![
            DeviceInfo { name: "Zebra".into(), kind: DeviceKind::Capture, is_default: false },
            DeviceInfo { name: "Alpha".into(), kind: DeviceKind::Capture, is_default: false },
            DeviceInfo { name: "Beta".into(), kind: DeviceKind::Playback, is_default: true },
        ];
        sort_devices(&mut v);
        assert_eq!(
            v.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["Beta", "Alpha", "Zebra"]
        );
    }

    #[test]
    fn a_node_without_a_media_class_is_not_a_device() {
        assert_eq!(kind_from_media_class("Audio/Sink"), Some(DeviceKind::Playback));
        assert_eq!(kind_from_media_class("Audio/Source"), Some(DeviceKind::Capture));
        assert_eq!(kind_from_media_class("Stream/Output/Audio"), None);
        assert_eq!(kind_from_media_class("Video/Source"), None);
        assert_eq!(kind_from_media_class(""), None);
    }

    /// The real enumeration, which needs a session with audio devices.
    /// Run by hand: `cargo test -p pheme-audio --lib devices -- --ignored`
    #[test]
    #[ignore]
    fn the_system_reports_at_least_one_device() {
        let v = list_devices().expect("enumeration");
        assert!(!v.is_empty(), "no audio devices found");
        for d in &v {
            assert!(!d.name.is_empty());
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-audio --lib devices`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the shape and the Linux backend**

```rust
//! Listing the audio devices the operating system offers.
//!
//! Two platform backends with no crate between them and the system, because
//! the configuration window's device menus are unusable without it and
//! `pheme devices` has been in the architecture document since the beginning
//! without ever existing. Sub-project 6 design §7.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Playback,
    Capture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
}

/// Playback first, then capture, each alphabetical.
///
/// A stable order matters more than the particular order: a menu that
/// reshuffles between openings is a menu people mis-click.
fn sort_devices(v: &mut [DeviceInfo]) {
    v.sort_by(|a, b| match (a.kind, b.kind) {
        (DeviceKind::Playback, DeviceKind::Capture) => std::cmp::Ordering::Less,
        (DeviceKind::Capture, DeviceKind::Playback) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
}

/// Not `cfg(target_os = "linux")`, although only the Linux backend calls it:
/// its test runs on every platform, and a `cfg` here would break the Windows
/// build of the test module. It is pure string matching and costs nothing
/// where it is unused.
fn kind_from_media_class(class: &str) -> Option<DeviceKind> {
    match class {
        "Audio/Sink" => Some(DeviceKind::Playback),
        "Audio/Source" => Some(DeviceKind::Capture),
        // Stream/* are applications playing or recording, not devices, and
        // listing them would offer the user something they cannot select.
        _ => None,
    }
}
```

**This step has no code block, deliberately.** Writing PipeWire registry
calls from memory would hand you something that looks authoritative and does
not compile — the same mistake this project already made once with `mdns-sd`.
Read `crates/pheme-audio/src/linux_pipewire.rs` and the installed crate under
`~/.cargo/registry/src/*/pipewire-*/src/`, and write what compiles against
them. What you may not change is the contract: the signature, `DeviceInfo`'s
shape, and which node classes count as devices.

The Linux `list_devices` runs a PipeWire main loop briefly, registers a registry listener, collects `PW_TYPE_INTERFACE_Node` globals whose `media.class` passes `kind_from_media_class`, taking `node.description` as the name and falling back to `node.name`, and quits the loop once the registry has been walked. Follow the connection and main-loop patterns already in `crates/pheme-audio/src/linux_pipewire.rs`; do not invent a second way of talking to PipeWire in this crate. The default device comes from the `default.audio.sink` and `default.audio.source` metadata; when that is unavailable, no entry is marked default, which is correct rather than guessing.

- [ ] **Step 4: Run the tests, including the ignored one by hand**

Run: `cargo test -p pheme-audio --lib devices`
Expected: PASS, 2 tests, 1 ignored.

Run: `cargo test -p pheme-audio --lib devices -- --ignored`
Expected: PASS on this machine, which has PipeWire. Report the device list it found. If it fails, that is a real defect in the backend — fix it rather than weakening the test.

- [ ] **Step 5: Run the full gate and commit**

```bash
git add crates/pheme-audio
git commit -F - <<'MSG'
Update: list the audio devices PipeWire offers

The configuration window's device menus need a list, and pheme devices
has been in the architecture document from the beginning without ever
existing. This is the shape plus the Linux half.

Only Audio/Sink and Audio/Source nodes are devices. Stream/* nodes are
applications playing or recording, and listing them would offer the user
something they cannot select.

The order is playback first, then capture, each alphabetical. A stable
order matters more than the particular one: a menu that reshuffles
between openings is a menu people mis-click.

When the default-device metadata is unavailable nothing is marked
default, which is honest rather than guessing at one.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 6: Audio device enumeration — Windows

**Files:**
- Modify: `crates/pheme-audio/src/devices.rs`
- Modify: `crates/pheme-audio/Cargo.toml` if a `windows` feature is missing

**Interfaces:**
- Consumes: `DeviceInfo`, `DeviceKind`, `sort_devices` from Task 5.
- Produces: the `cfg(target_os = "windows")` arm of `list_devices`, same signature.

- [ ] **Step 1: Write the Windows backend**

**No code block here either, for the same reason**: read
`crates/pheme-audio/src/windows/wasapi.rs` and the `windows` crate's own
definitions, and write what compiles. The contract — the signature and
`DeviceInfo`'s shape — is fixed; the calls are yours.

`IMMDeviceEnumerator::EnumAudioEndpoints` over `eRender` then `eCapture` with `DEVICE_STATE_ACTIVE`, reading `PKEY_Device_FriendlyName` from each endpoint's property store, and `GetDefaultAudioEndpoint(flow, eConsole)` for the default id. Follow the COM initialisation and property-store patterns already in `crates/pheme-audio/src/windows/wasapi.rs` rather than introducing a second style. Add whatever `windows` crate features the calls need to `crates/pheme-audio/Cargo.toml`, matching how the existing feature list is written.

- [ ] **Step 2: Cross-check it compiles**

Run:
```
RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone \
PATH=$HOME/.cargo-standalone/bin:$PATH \
cargo clippy -p pheme-audio --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```
Expected: clean.

**This type-checks; it does not run.** No CI runner and no machine here has a Windows audio session, so this backend's behaviour is established by manual row G9 and nothing else. Say that plainly in your report rather than implying the clean clippy means it works.

- [ ] **Step 3: Run the full gate and commit**

```bash
git add crates/pheme-audio
git commit -F - <<'MSG'
Update: list the audio devices WASAPI offers

The Windows half of device enumeration: active render and capture
endpoints, their friendly names, and the console default for each flow.
It follows the COM and property-store patterns already in wasapi.rs
rather than introducing a second style in one crate.

Type-checked from Linux against the gnu target. That is not a claim it
works: no runner and no machine here has a Windows audio session, so
this backend's behaviour rests on manual row G9 alone.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 7: `pheme devices`

**Files:**
- Modify: `crates/pheme-app/src/main.rs`
- Modify: `crates/pheme-app/Cargo.toml` if `pheme-audio` is not already a dependency (it is)

**Interfaces:**
- Consumes: `pheme_audio::devices::{list_devices, DeviceInfo, DeviceKind}`.

- [ ] **Step 1: Add the subcommand**

```rust
    /// List the audio devices this machine offers
    Devices,
```

and its arm, printing a table with a `*` marking the default, playback and capture grouped by the order `sort_devices` already produces:

```rust
        Cmd::Devices => {
            let devices = pheme_audio::devices::list_devices()?;
            if devices.is_empty() {
                println!("No audio devices found.");
                return Ok(());
            }
            println!("{:<10} {:<40} DEFAULT", "KIND", "NAME");
            for d in &devices {
                let kind = match d.kind {
                    pheme_audio::devices::DeviceKind::Playback => "playback",
                    pheme_audio::devices::DeviceKind::Capture => "capture",
                };
                println!(
                    "{:<10} {:<40} {}",
                    kind,
                    d.name,
                    if d.is_default { "*" } else { "" }
                );
            }
            Ok(())
        }
```

- [ ] **Step 2: Verify by hand**

Run: `cargo run -p pheme-app --bin pheme -- devices`
Expected: a table. Compare it against what the desktop's sound settings show and report both, including any device that appears in one and not the other — that difference is the finding, not a nuisance.

- [ ] **Step 3: Run the full gate and commit**

```bash
git add crates/pheme-app/src/main.rs
git commit -F - <<'MSG'
Update: add the pheme devices subcommand

It prints what list_devices found, with the default marked. The name in
the NAME column is exactly the string the config's device fields accept,
so the output is something to copy rather than something to interpret.

Architecture §12 has listed this command since the first sub-project. It
exists now because the configuration window needs the same list.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 8: The supervisor

The front-end without any user interface: it spawns the child, watches it, restarts it, and stops it. Headless and fully testable, which is why it comes before the tray and the window.

**Files:**
- Create: `crates/pheme-app/src/frontend/mod.rs`
- Create: `crates/pheme-app/src/frontend/supervisor.rs`
- Create: `crates/pheme-app/tests/supervisor.rs`
- Modify: `crates/pheme-app/src/lib.rs`

**Interfaces:**
- Consumes: `IpcListener`, `IpcConnection`, `Status`, `Command`, `LinkState` from Tasks 1–2; `Config`, `Config::save` from Task 3.
- Produces:
  ```rust
  pub struct Supervisor { /* … */ }
  pub enum CoreState {
      /// No configuration yet: nothing to run.
      NoConfig,
      Running(Status),
      /// The child exited. The string is why, as far as we can tell.
      Stopped(String),
  }
  impl Supervisor {
      /// `exe` is the executable to spawn as the child — `std::env::current_exe()`
      /// in production, a stub in tests.
      pub async fn start(exe: PathBuf, cfg: Option<Config>) -> anyhow::Result<Supervisor>;
      pub fn state(&self) -> CoreState;
      pub async fn send(&mut self, c: Command);
      /// Stops the child, writes the configuration, starts it again.
      pub async fn apply_config(&mut self, cfg: Config, path: &Path) -> anyhow::Result<()>;
      /// Stops the child and waits up to two seconds before killing it.
      pub async fn shutdown(&mut self);
      /// The child's process id while one is running. Used by the shutdown
      /// test to prove nothing survives.
      pub fn child_pid(&self) -> Option<u32>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

`crates/pheme-app/tests/supervisor.rs`. The stub child is the test binary itself re-invoked with an environment variable, which avoids compiling a second binary:

```rust
#[tokio::test]
async fn with_no_configuration_nothing_is_spawned() {
    // Review Focus 1: the first run, every time, for every new user.
    let s = Supervisor::start(stub_exe(), None).await.unwrap();
    assert!(matches!(s.state(), CoreState::NoConfig));
}

#[tokio::test]
async fn a_running_child_reports_status() {
    let mut s = Supervisor::start(stub_exe(), Some(server_config())).await.unwrap();
    assert!(
        wait_until(|| matches!(s.state(), CoreState::Running(_)), Duration::from_secs(5)).await,
        "no status arrived"
    );
}

#[tokio::test]
async fn a_child_that_dies_is_reported_not_waited_on_forever() {
    // Review Focus 2: a crash, a missing uinput permission, a bound port. The
    // front-end must say so rather than wait for a status that never comes.
    let mut s = Supervisor::start(stub_exe_that_exits(2), Some(server_config())).await.unwrap();
    assert!(
        wait_until(|| matches!(s.state(), CoreState::Stopped(_)), Duration::from_secs(5)).await,
        "the supervisor never noticed the child had gone"
    );
}

#[tokio::test]
async fn a_child_that_fails_to_start_reports_why() {
    // Review Focus 3: a second front-end, whose child cannot bind the port.
    // The message must reach the caller as text, not vanish into a log.
    let mut s = Supervisor::start(stub_exe_that_fails("address already in use"), Some(server_config()))
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
    let mut s = Supervisor::start(stub_exe(), Some(server_config())).await.unwrap();
    wait_until(|| matches!(s.state(), CoreState::Running(_)), Duration::from_secs(5)).await;
    let mut cfg = server_config();
    cfg.name = "renamed".into();
    s.apply_config(cfg.clone(), &path).await.unwrap();
    assert_eq!(Config::load(Some(&path)).unwrap(), cfg, "the file was not written");
    assert!(
        wait_until(|| matches!(s.state(), CoreState::Running(_)), Duration::from_secs(5)).await,
        "the child did not come back"
    );
}

#[tokio::test]
async fn shutdown_leaves_no_child_behind() {
    let mut s = Supervisor::start(stub_exe(), Some(server_config())).await.unwrap();
    wait_until(|| matches!(s.state(), CoreState::Running(_)), Duration::from_secs(5)).await;
    let pid = s.child_pid().expect("a running child");
    s.shutdown().await;
    assert!(!process_exists(pid), "pid {pid} survived shutdown");
}
```

Write `stub_exe`, `stub_exe_that_exits`, `stub_exe_that_fails`, `wait_until` and `process_exists` as helpers in the same file. The stub connects to the socket in `--ipc`, sends a `Status` every 200 ms, and exits when the socket closes — a small, honest imitation of the real core.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --test supervisor`
Expected: FAIL — `Supervisor` does not exist.

- [ ] **Step 3: Write the supervisor**

`crates/pheme-app/src/frontend/supervisor.rs`. It owns the `IpcListener`, the `tokio::process::Child`, and the latest `Status`. One task reads statuses and updates the shared state; another waits on the child and, when it exits, records `Stopped` with the exit status and the last lines of its stderr, so the reason a core failed reaches the window as text.

Key behaviours, each matching a test above:
- `start` with `None` configuration spawns nothing and reports `NoConfig`.
- The child is spawned with the subcommand its `role` implies, plus `--ipc <path>`, with stderr piped so a failure message can be captured.
- When the child exits, `state()` becomes `Stopped` with whatever explanation is available; the supervisor never blocks waiting for a status from a process that is gone.
- `apply_config` sends `Command::Stop`, waits for the child, writes the file with `Config::save`, and spawns again.
- `shutdown` closes the listener, waits two seconds, then kills.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --test supervisor`
Expected: PASS, 6 tests. Run the file five times to show it is not flaky, and report the runs — these tests spawn processes and are the most likely in the plan to be timing-sensitive.

- [ ] **Step 5: Run the full gate and commit**

```bash
git add crates/pheme-app/src/frontend crates/pheme-app/src/lib.rs crates/pheme-app/tests/supervisor.rs
git commit -F - <<'MSG'
Update: supervise the core process from the front-end

The front-end without any user interface: it spawns the child with the
subcommand its role implies, watches it, restarts it after a
configuration change, and stops it. Headless and fully testable, which
is why it lands before the tray and the window rather than tangled into
them.

Three states, and the two that are not "running" are the ones that
matter. With no configuration file nothing is spawned at all, which is
the first run for every new user. When the child exits, the supervisor
records why — the exit status and what the child wrote to stderr — so a
core that could not bind its port says so in the window instead of
leaving it waiting for a status that will never arrive.

Shutdown closes the socket, waits two seconds, then kills. The core
already exits on a closed socket, so the wait is for the ordinary case
and the kill is for a core with a bug.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 9: The tray

**Files:**
- Create: `crates/pheme-app/src/frontend/tray.rs`
- Modify: `Cargo.toml`, `crates/pheme-app/Cargo.toml`, `crates/pheme-app/src/frontend/mod.rs`

**Interfaces:**
- Consumes: `CoreState`, `Command` from Task 8.
- Produces: `Tray` with `fn new() -> Option<Tray>` (None when no tray is available), `fn set_state(&mut self, s: &CoreState)`, `fn poll(&mut self) -> Option<TrayEvent>`, and `enum TrayEvent { Open, ToggleLock, StartStop, Quit }`.

- [ ] **Step 1: Add the dependency**

Root `Cargo.toml`: `tray-icon = "0.24"`. `crates/pheme-app/Cargo.toml`: `tray-icon = { workspace = true }`.

Note in the commit body that this brings GTK 3 and `libappindicator` as Linux runtime dependencies. That was the accepted trade in design §5, and sub-project 7's packaging depends on knowing it.

- [ ] **Step 2: Write the tray**

Menu items: **Open**, **Lock input** (a checkmark mirroring `Status.locked`), **Stop** or **Start** depending on `CoreState`, **Quit**. Two icons, connected and disconnected, embedded with `include_bytes!` so there is no file to install.

`Tray::new()` returns `None` rather than failing when the platform has no tray — GNOME without the AppIndicator extension is the common case. Log one warning at that point and let the caller carry on: design §5 makes a missing tray a degraded mode, and the window is reachable without it.

- [ ] **Step 3: Verify by hand**

Run the front-end on this machine (KDE). Confirm the icon appears, the menu opens, the lock checkmark follows the core, and Quit exits. Report what you saw, including anything that looked wrong.

- [ ] **Step 4: Run the full gate and commit**

```bash
git add Cargo.toml crates/pheme-app
git commit -F - <<'MSG'
Update: add the tray icon and its menu

Open, a lock checkmark that mirrors the core's own state, start or stop
depending on what is running, and quit. Two icons compiled into the
binary, so there is no file to install alongside it.

tray-icon brings GTK 3 and libappindicator as runtime dependencies on
Linux. That was the accepted trade against writing two backends, and it
binds sub-project 7: the tarball is no longer self-contained and the
packaging has to say which system packages to install.

A tray that cannot be created is a degraded mode, not a failure. GNOME
without the AppIndicator extension is the ordinary case, so Tray::new
returns None, logs once, and the window stays reachable without it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 10: The window, status panel

**Files:**
- Create: `crates/pheme-app/src/frontend/window.rs`
- Modify: `Cargo.toml`, `crates/pheme-app/Cargo.toml`, `crates/pheme-app/src/main.rs` (the no-subcommand case)

**Interfaces:**
- Consumes: `Supervisor`, `CoreState` from Task 8; `Tray` from Task 9.
- Produces: `pheme_app::frontend::run()` — the whole front-end, called by `main.rs` when no subcommand is given. Also `StatusView`, the pure reduction of `CoreState` into what the panel draws, with `Default`, `fn apply(&mut self, s: &CoreState)`, and the public fields `peer: Option<String>` and `rtt_us: u64`. It is a separate type precisely so the reset-on-restart rule can be tested without a window.

- [ ] **Step 1: Add the dependency and the entry point**

Root `Cargo.toml`: `eframe = "0.32"`. In `main.rs`, make `cmd` optional (`#[command(subcommand)] cmd: Option<Cmd>`) and run `pheme_app::frontend::run()` when it is `None`. Every existing subcommand keeps its exact behaviour.

- [ ] **Step 2: Close hides, Quit exits**

Design §6: closing the window hides it and leaves the tray and the core
running; only **Quit** exits. In `eframe` that means handling the close
request rather than letting it end the event loop:

```rust
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            // The window is a view onto a running core, not the application.
            // Closing it must not stop sharing — that is what Quit is for.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
```

Check the exact viewport command names against the `eframe` 0.32 that
resolves; they have moved between versions. If a tray could not be created
(Task 9 returned `None`), closing the window must **exit** instead of hiding
it, because otherwise there is no way left to reach the application and no
way to quit it.

- [ ] **Step 3: Write the status panel**

Role, link state, peer, RTT in milliseconds to one decimal, and the counters. `LinkState::Failed(why)` shows `why` — that string is the whole reason this panel earns its place, since today it exists only in the log.

**Label fields by role rather than showing a nought for what the other end measures.** A server does not display "lost", and a client does not display the audio pair; this is Review Focus 5's other half and the doc comment on `Status` says which is which.

- [ ] **Step 4: Write the failing test for stale status**

Review Focus 5. This is testable without a window, because the state reduction is a pure function — put it there and test it:

```rust
    #[test]
    fn a_restart_does_not_show_the_previous_role_s_peer() {
        // The role changed and the child restarted. Nothing from the run
        // before may still be on screen.
        let mut view = StatusView::default();
        view.apply(&CoreState::Running(server_status_with_peer("laptop-win")));
        assert_eq!(view.peer.as_deref(), Some("laptop-win"));
        view.apply(&CoreState::Stopped("restarting".into()));
        assert_eq!(view.peer, None, "a stale peer survived the restart");
        assert_eq!(view.rtt_us, 0);
    }
```

- [ ] **Step 5: Verify by hand and commit**

Run `pheme` with a real configuration against a real core. Confirm the numbers move and match `pheme server --stats`. Report both, side by side.

```bash
git add Cargo.toml crates/pheme-app
git commit -F - <<'MSG'
Update: show live core status in the window

Role, link state, peer, RTT and the counters, driven by the Status the
core pushes every second. A failed link shows why, which is the string
that until now existed only in the log and is the thing a person
actually needs.

Fields are labelled by role rather than shown as a nought for whatever
the other end measures: a server does not count loss and a client does
not fill the audio pair, so displaying zero there would read as "nothing
went wrong" rather than "not measured here".

The view resets when the core stops, so a peer name or an RTT from
before a restart is never left on screen after the role changed
underneath it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 11: The window, pairing panel

**Files:**
- Modify: `crates/pheme-app/src/frontend/window.rs`

**Interfaces:**
- Consumes: `pheme_net::discovery::browse`, `pheme_net::pairing::client_pair`, `Supervisor`.

- [ ] **Step 1: Write the panel**

On a **server**: a button that restarts the child with `--pair`, and shows the code the core prints. On a **client**: the list from `pheme_net::discovery::browse` — name, address, fingerprint — a code field, and a button that runs `pheme_net::pairing::client_pair` in the front-end process. The front-end already links `pheme-net`; nothing shells out.

The browse runs on a task, never on the repaint thread: it takes three seconds and blocking the UI for that long is the kind of thing this whole two-process design exists to avoid.

Show the fingerprint beside each discovered server, and say in the panel that it is there to be compared with what the other machine displays — it is advisory, and trust still comes from the code.

- [ ] **Step 2: Verify by hand**

Pair two machines from the windows alone, with no terminal. If only one machine is available, pair against a second `pheme` process on the same machine using an explicit address. Report exactly what you did and what happened.

- [ ] **Step 3: Run the full gate and commit**

```bash
git add crates/pheme-app/src/frontend/window.rs
git commit -F - <<'MSG'
Update: pair two machines from the window

A server shows its code; a client picks a server from the mDNS list,
types the code and pairs. The pairing runs in the front-end process,
which already links pheme-net, so nothing shells out to a second
invocation of the binary.

The discovery browse runs on a task rather than on the repaint thread.
It takes three seconds, and freezing the window for that long is exactly
what the two-process design exists to avoid.

Each discovered server shows its fingerprint, with a line saying it is
for comparing against the other machine. It is advisory: trust comes
from the pairing code, never from a record anyone on the network can
publish.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 12: The window, configuration panel

**Files:**
- Modify: `crates/pheme-app/src/frontend/window.rs`

**Interfaces:**
- Consumes: `Config`, `Config::save`, `Supervisor::apply_config`, `pheme_audio::devices::list_devices`.

- [ ] **Step 1: Write the panel**

`role`, `name`, `listen` or `connect` depending on role, the client list with each entry's `side` and `span`, the lock hotkey, and three device menus filled from `list_devices`. An empty selection means "the operating system default", exactly as an empty string does in the file today.

**Validate before writing**, using the same checks `Config` already performs on load, so the window cannot produce a file the CLI would then refuse. A rejected field shows why next to itself rather than in a dialog.

**Warn once before the first save** that comments in a hand-written `config.toml` will not survive. Silently discarding what someone wrote by hand is worse than saying so.

Saving calls `Supervisor::apply_config`, which writes the file and restarts the child.

- [ ] **Step 2: Verify by hand**

Edit the client list and save. Confirm `config.toml` parses with `pheme server --config <path>`, that the child restarts, and that the new edge works. Report the file before and after.

- [ ] **Step 3: Run the full gate and commit**

```bash
git add crates/pheme-app/src/frontend/window.rs
git commit -F - <<'MSG'
Update: edit the configuration from the window

Role, name, address, the client list with each edge and span, the lock
hotkey, and device menus filled from list_devices. An empty device
selection means the operating system default, exactly as an empty string
does in the file.

Validation runs before the file is written, through the same checks
Config performs when loading, so the window cannot produce a file the
command line would then refuse. A rejected field says why beside itself
rather than in a dialog that takes the context away.

The first save warns that comments in a hand-written config.toml will
not survive it. A TOML serializer has no comments to preserve, so the
choice is between saying so and silently discarding what someone typed.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 13: Documentation

**Files:**
- Modify: `README.md`, `docs/testing.md`

- [ ] **Step 1: README**

A section on running `pheme` with no subcommand: what the tray does, what the window does, and that closing the front-end stops sharing — with `pheme server` named as the way to run it without a GUI.

State the Linux runtime dependencies plainly: GTK 3 and `libappindicator`, and that GNOME needs the AppIndicator shell extension for the tray, with the window still reachable without it.

Document `pheme devices`.

**Check every claim against the code before writing it.** A README that is subtly wrong sends someone to configure away something that cannot be configured. If the code does not support a claim, do not write it — report the discrepancy instead.

- [ ] **Step 2: `docs/testing.md`**

Add rows G1–G10 exactly as §13 of the design lists them.

- [ ] **Step 3: Run the full gate and commit**

```bash
git add README.md docs/testing.md
git commit -F - <<'MSG'
Update: document the tray and the configuration window

What `pheme` with no subcommand does, and the part people will meet
first: closing the front-end stops sharing, and `pheme server` is how to
run it without a GUI.

The Linux runtime dependencies are stated where someone will look before
installing rather than after it fails: GTK 3 and libappindicator, plus
the AppIndicator extension on GNOME, with the window still reachable
when the tray is not.

docs/testing.md gains G1 to G10. The tray, the window and both device
backends need a desktop session, so CI proves this sub-project compiles
and that its protocol and file handling are correct, and proves nothing
about whether the application works. That matrix is the only place it
is tested.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```
