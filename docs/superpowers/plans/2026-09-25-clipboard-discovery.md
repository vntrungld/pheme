# Sub-project 5 — Clipboard and Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Text copied on one machine can be pasted on the other after the pointer crosses the edge, and a client finds its server by name instead of by IP address.

**Architecture:** A new crate `pheme-clip` holds a `Clipboard` trait, a pure `ClipSync` policy and one `arboard` backend covering Win32, X11 and Wayland data-control. One dedicated OS thread in `pheme-app` owns the clipboard handle and the policy, takes commands on a channel, and never touches the tokio runtime or the input path. The clipboard crosses on its own QUIC unidirectional stream, one per change, so a large payload cannot hold keystrokes behind it. Discovery lives in `pheme-net`: the server advertises `_pheme._udp.local.` and the client resolves its configured name through a new pure `Target` type on every reconnect attempt.

**Tech Stack:** Rust 2021, `arboard` 3.6 (`wayland-data-control`), `mdns-sd` 0.21, `quinn` 0.11, `postcard`, `tokio`, `crossbeam-channel`, `tracing`.

**Spec:** `docs/superpowers/specs/2026-09-25-clipboard-discovery-design.md`

## Global Constraints

- Every file, comment, identifier, log message and commit message in this repository is written in **English**.
- Commit messages: `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`. Title under 72 characters, imperative mood. Blank line. Body wrapped at 72 columns explaining what changed and why. Final trailer exactly `Co-Authored-By: Claude <noreply@anthropic.com>` and nothing else.
- `pheme-core` gains no new code in this plan. No new `Action`, no new state, no new dependency.
- `pheme-net` must **not** depend on `pheme-clip`. Shared constants live in `pheme-proto`.
- `pheme-core` contains no `cfg(target_os)`.
- Nothing added here may run on, block, or delay the input path.
- Exact values, to be used verbatim:
  - `pub const MAX_CLIP_BYTES: usize = 1024 * 1024;`
  - `pub const CLIP_FRAME_SLACK: usize = 256;`
  - `pub const CLIP_MIME: &str = "text/plain;charset=utf-8";`
  - mDNS service type `"_pheme._udp.local."`
  - TXT keys `fp` and `v`, with `v` always `"1"`
  - `arboard = { version = "3.6", default-features = false, features = ["wayland-data-control"] }`
  - `mdns-sd = "0.21"`
- New crates use `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`, `rust-version.workspace = true`, and take every shared dependency through `{ workspace = true }`.
- Every task ends with all three green: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

## Review Focus

Five things the spec implies, that a person will meet, and that no task's happy-path test would catch. Each has a test in the task that owns the code.

1. **A peer sends `Msg::Clipboard` whose `data` is not valid UTF-8.** Expected: ignored with a log line; never a panic and never a broken connection. — Task 4.
2. **A peer sends a `mime` other than `text/plain;charset=utf-8`.** Expected: ignored, logged at `debug`, connection unaffected. — Task 4.
3. **The clipboard backend fails mid-session** (compositor restarts, the selection owner disappears). Expected: the session continues, the first failure logs at `warn` and the rest at `debug` so a persistently broken clipboard does not log on every crossing. — Task 4.
4. **`connect` is empty or only whitespace.** Expected: a clear configuration error, never an mDNS browse for the empty name. — Task 8.
5. **An mDNS instance name that contains a dot** (`name = "my.desk"`). Expected: `resolve` finds what `advertise` registered, because DNS-SD escapes dots inside an instance label. — Task 7.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `crates/pheme-clip/Cargo.toml` | the new crate's manifest |
| `crates/pheme-clip/src/lib.rs` | `Clipboard` trait, `ClipError`, `open()` |
| `crates/pheme-clip/src/sync.rs` | `ClipSync` — the whole policy, no OS, no network |
| `crates/pheme-clip/src/backend.rs` | the `arboard` implementation |
| `crates/pheme-clip/src/mock.rs` | `MockClipboard` + `MockClipboardHandle` for tests |
| `crates/pheme-app/src/clipboard.rs` | `ClipboardService`: the worker thread and the two moments it acts on |
| `crates/pheme-app/src/target.rs` | `Target` — how a `connect` string becomes an address |
| `crates/pheme-net/src/discovery.rs` | advertise, browse, resolve over mDNS |
| `crates/pheme-app/tests/clipboard.rs` | clipboard over real QUIC with mock backends |

**Modified:**

| File | Change |
|---|---|
| `Cargo.toml` | workspace member `crates/pheme-clip`, workspace deps `arboard`, `mdns-sd` |
| `crates/pheme-proto/src/lib.rs` | `MAX_CLIP_BYTES`, `CLIP_FRAME_SLACK`, `CLIP_MIME` |
| `crates/pheme-net/src/transport.rs` | `PeerSender::send_clipboard`, the `accept_uni` reader, `Peer::take_clipboard` |
| `crates/pheme-net/src/lib.rs` | `pub mod discovery;` and its re-exports |
| `crates/pheme-net/Cargo.toml` | `mdns-sd` |
| `crates/pheme-app/Cargo.toml` | `pheme-clip`, `crossbeam-channel` already present |
| `crates/pheme-app/src/lib.rs` | `pub mod clipboard; pub mod target;` |
| `crates/pheme-app/src/server.rs` | `ServerDeps.clipboard`, `Shared.clipboard`, the `Enter` hook, the receive task, advertising |
| `crates/pheme-app/src/client.rs` | `ClientDeps.clipboard`, `ClientDeps.target`, the `Leave`/`Bye` hook, the receive task, re-resolution |
| `crates/pheme-app/src/config.rs` | `discovery` flag, `connect_target()` |
| `crates/pheme-app/src/main.rs` | `pheme discover` |
| `crates/pheme-app/src/setup.rs` | the mDNS firewall note |
| `README.md`, `docs/testing.md`, `docs/superpowers/specs/2026-09-21-pheme-architecture-design.md` | documentation |

---

## Task 1: Clipboard constants and the `ClipSync` policy

The clipboard's entire decision-making, with no OS and no network, so it can be tested exhaustively.

**Files:**
- Modify: `Cargo.toml` (workspace members and dependencies)
- Modify: `crates/pheme-proto/src/lib.rs` (after `PROTOCOL_VERSION`, around line 16)
- Create: `crates/pheme-clip/Cargo.toml`
- Create: `crates/pheme-clip/src/lib.rs`
- Create: `crates/pheme-clip/src/sync.rs`
- Create: `crates/pheme-clip/src/mock.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pheme_proto::{MAX_CLIP_BYTES, CLIP_FRAME_SLACK, CLIP_MIME}`; `pheme_clip::{Clipboard, ClipError, ClipSync}`; `pheme_clip::mock::{MockClipboard, MockClipboardHandle}`. `Clipboard::get_text(&mut self) -> Result<Option<String>, ClipError>`, `Clipboard::set_text(&mut self, text: &str) -> Result<(), ClipError>`. `MockClipboard::new() -> (MockClipboard, MockClipboardHandle)`.

- [ ] **Step 1: Add the constants to `pheme-proto`**

In `crates/pheme-proto/src/lib.rs`, directly after `pub const PROTOCOL_VERSION: u16 = 2;`:

```rust
/// The largest clipboard payload Pheme sends or accepts, in bytes.
///
/// The cap lives here, not in `pheme-clip`, because two crates enforce it: the
/// sender's policy (`ClipSync`) and the receiver's unidirectional-stream reader
/// in `pheme-net`. A receiver with a smaller cap than the sender would silently
/// drop content the sender believed it had delivered. `pheme-net` depends on
/// this crate and must never depend on `pheme-clip`.
pub const MAX_CLIP_BYTES: usize = 1024 * 1024;

/// Room above `MAX_CLIP_BYTES` for the encoding around the payload: the enum
/// tag, the MIME string and the two length prefixes.
pub const CLIP_FRAME_SLACK: usize = 256;

/// The only clipboard format Pheme speaks. A message in any other format is
/// ignored by the receiver rather than guessed at.
pub const CLIP_MIME: &str = "text/plain;charset=utf-8";
```

- [ ] **Step 2: Register the crate in the workspace**

In the root `Cargo.toml`, add `"crates/pheme-clip",` to `members` after `"crates/pheme-audio",`, and add to `[workspace.dependencies]`:

```toml
pheme-clip = { path = "crates/pheme-clip" }
```

- [ ] **Step 3: Create the crate manifest**

`crates/pheme-clip/Cargo.toml`:

```toml
[package]
name = "pheme-clip"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
pheme-proto = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
```

- [ ] **Step 4: Write the failing tests for `ClipSync`**

`crates/pheme-clip/src/sync.rs`, tests only for now (the `ClipSync` body comes in Step 6):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_clipboard_is_not_sent() {
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing(String::new()), None);
    }

    #[test]
    fn the_first_text_is_sent() {
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing("hello".into()), Some("hello".to_string()));
    }

    #[test]
    fn the_same_text_is_not_sent_twice() {
        let mut s = ClipSync::new();
        assert!(s.outgoing("hello".into()).is_some());
        assert_eq!(s.outgoing("hello".into()), None);
    }

    #[test]
    fn new_text_is_sent_after_a_repeat() {
        let mut s = ClipSync::new();
        assert!(s.outgoing("one".into()).is_some());
        assert_eq!(s.outgoing("one".into()), None);
        assert_eq!(s.outgoing("two".into()), Some("two".to_string()));
    }

    #[test]
    fn text_received_from_the_peer_does_not_echo_back() {
        // The whole reason `last` records both directions: without this, crossing
        // back would return the peer's own text to it on every crossing.
        let mut s = ClipSync::new();
        assert!(s.incoming("from the peer"));
        assert_eq!(s.outgoing("from the peer".into()), None);
    }

    #[test]
    fn the_same_text_is_not_applied_twice() {
        let mut s = ClipSync::new();
        assert!(s.incoming("hello"));
        assert!(!s.incoming("hello"));
    }

    #[test]
    fn an_empty_message_does_not_clear_the_local_clipboard() {
        let mut s = ClipSync::new();
        assert!(!s.incoming(""));
    }

    #[test]
    fn oversized_text_is_not_sent() {
        let mut s = ClipSync::new();
        let big = "x".repeat(MAX_CLIP_BYTES + 1);
        assert_eq!(s.outgoing(big), None);
    }

    #[test]
    fn a_refused_oversized_text_does_not_block_the_next_one() {
        // `outgoing` must not record what it refused: otherwise copying something
        // huge and then something small would send neither.
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing("x".repeat(MAX_CLIP_BYTES + 1)), None);
        assert_eq!(s.outgoing("small".into()), Some("small".to_string()));
    }

    #[test]
    fn text_exactly_at_the_limit_is_sent() {
        let mut s = ClipSync::new();
        let at = "x".repeat(MAX_CLIP_BYTES);
        assert_eq!(s.outgoing(at.clone()), Some(at));
    }

    #[test]
    fn oversized_text_from_a_peer_is_refused() {
        let mut s = ClipSync::new();
        assert!(!s.incoming(&"x".repeat(MAX_CLIP_BYTES + 1)));
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p pheme-clip`
Expected: FAIL — the crate does not compile, `ClipSync` is not defined.

- [ ] **Step 6: Write `ClipSync`**

At the top of `crates/pheme-clip/src/sync.rs`, above the test module:

```rust
//! What crosses the network and what does not.
//!
//! This is the clipboard's whole policy, and it touches neither the operating
//! system nor the network so that all of it can be tested. See §3.4 of the
//! sub-project 5 design.

use pheme_proto::MAX_CLIP_BYTES;
use tracing::warn;

/// Remembers the last text this side exchanged, in either direction.
///
/// One field answers three questions. Content equal to `last` is not sent,
/// which stops repeats. Content *received* also becomes `last`, which is what
/// stops an echo: text that arrived from the peer is never sent back to it.
#[derive(Debug, Default)]
pub struct ClipSync {
    last: Option<String>,
}

impl ClipSync {
    pub fn new() -> ClipSync {
        ClipSync::default()
    }

    /// The text to send to the peer, or `None` to send nothing.
    pub fn outgoing(&mut self, text: String) -> Option<String> {
        if text.is_empty() {
            // An empty clipboard carries no intent, and sending it would clear
            // the peer's.
            return None;
        }
        if text.len() > MAX_CLIP_BYTES {
            // Deliberately not recorded in `last`: refusing this must not also
            // refuse whatever the user copies next.
            warn!(
                bytes = text.len(),
                limit = MAX_CLIP_BYTES,
                "clipboard content is too large to share; it stays on this machine"
            );
            return None;
        }
        if self.last.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last = Some(text.clone());
        Some(text)
    }

    /// Whether the caller should write `text` to the local clipboard.
    pub fn incoming(&mut self, text: &str) -> bool {
        if text.is_empty() || text.len() > MAX_CLIP_BYTES {
            return false;
        }
        if self.last.as_deref() == Some(text) {
            return false;
        }
        self.last = Some(text.to_string());
        true
    }
}
```

- [ ] **Step 7: Write the trait and the mock**

`crates/pheme-clip/src/lib.rs`:

```rust
//! The system text clipboard, behind one trait with a mock.
//!
//! Sub-project 5 design: `docs/superpowers/specs/2026-09-25-clipboard-discovery-design.md`.

pub mod mock;
mod sync;

pub use sync::ClipSync;

#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    /// No clipboard exists to talk to. This is a supported state, not a bug:
    /// GNOME's compositor implements no data-control protocol, and a headless
    /// session has no clipboard at all. The caller runs without clipboard
    /// sharing and everything else keeps working.
    #[error("no clipboard is available: {0}")]
    Unavailable(String),
    /// A clipboard exists but this call did not work.
    #[error("clipboard: {0}")]
    Backend(String),
}

/// Read and write the system clipboard's text.
///
/// Deliberately not `Send`: on X11 a clipboard handle owns the `CLIPBOARD`
/// selection on the thread that created it, so the handle is created on the
/// thread that will use it and never moves. `ClipboardService` in `pheme-app`
/// takes a factory rather than a handle for exactly this reason.
pub trait Clipboard {
    /// `Ok(None)` when the clipboard holds no text. An image on the clipboard
    /// is `Ok(None)`, not an error: there is simply nothing to send.
    fn get_text(&mut self) -> Result<Option<String>, ClipError>;
    fn set_text(&mut self, text: &str) -> Result<(), ClipError>;
}
```

`crates/pheme-clip/src/mock.rs`:

```rust
//! An in-memory clipboard, so the clipboard path can be tested without a desktop.

use std::sync::{Arc, Mutex};

use crate::{ClipError, Clipboard};

#[derive(Debug, Default)]
struct State {
    text: Option<String>,
    /// When set, every call fails with this message.
    fail: Option<String>,
    /// How many times `set_text` succeeded, so a test can tell "wrote the same
    /// thing again" from "wrote nothing".
    sets: u32,
}

/// The test's side of a `MockClipboard`. Cheap to clone; every clone sees the
/// same clipboard.
#[derive(Clone, Debug, Default)]
pub struct MockClipboardHandle(Arc<Mutex<State>>);

impl MockClipboardHandle {
    /// What the clipboard holds now.
    pub fn text(&self) -> Option<String> {
        self.0.lock().unwrap().text.clone()
    }

    /// Stand in for the user copying something.
    pub fn copy(&self, text: &str) {
        self.0.lock().unwrap().text = Some(text.to_string());
    }

    /// How many times the clipboard has been written through the trait.
    pub fn sets(&self) -> u32 {
        self.0.lock().unwrap().sets
    }

    /// Make every later call fail, as a compositor restart would.
    pub fn fail_with(&self, message: &str) {
        self.0.lock().unwrap().fail = Some(message.to_string());
    }

    pub fn stop_failing(&self) {
        self.0.lock().unwrap().fail = None;
    }
}

pub struct MockClipboard(MockClipboardHandle);

impl MockClipboard {
    pub fn new() -> (MockClipboard, MockClipboardHandle) {
        let h = MockClipboardHandle::default();
        (MockClipboard(h.clone()), h)
    }
}

impl Clipboard for MockClipboard {
    fn get_text(&mut self) -> Result<Option<String>, ClipError> {
        let s = self.0 .0.lock().unwrap();
        match &s.fail {
            Some(m) => Err(ClipError::Backend(m.clone())),
            None => Ok(s.text.clone()),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipError> {
        let mut s = self.0 .0.lock().unwrap();
        if let Some(m) = &s.fail {
            return Err(ClipError::Backend(m.clone()));
        }
        s.text = Some(text.to_string());
        s.sets += 1;
        Ok(())
    }
}
```

Add `use pheme_proto::MAX_CLIP_BYTES;` is already in `sync.rs`; the test module reaches it through `use super::*`.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p pheme-clip`
Expected: PASS, 11 tests.

- [ ] **Step 9: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml crates/pheme-proto/src/lib.rs crates/pheme-clip
git commit -F - <<'MSG'
Update: add pheme-clip with the clipboard sync policy

The clipboard's whole decision-making is one struct with one field, and
it touches neither the operating system nor the network so that all of
it can be tested. ClipSync remembers the last text this side exchanged
in either direction, which answers three questions at once: an empty
clipboard is not sent, a repeat is not sent, and text received from the
peer is not sent back, so nothing echoes across the edge.

Content over the cap is refused but deliberately not recorded, so
copying something huge and then something small still sends the small
one.

MAX_CLIP_BYTES, CLIP_FRAME_SLACK and CLIP_MIME go in pheme-proto rather
than here: pheme-net enforces the same cap when reading the stream, and
a receiver with a smaller cap than the sender would silently drop
content the sender believed it had delivered. pheme-net must never
depend on pheme-clip.

The Clipboard trait is deliberately not Send. On X11 a clipboard handle
owns the CLIPBOARD selection on the thread that created it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 2: The `arboard` backend

One backend for Windows, X11 and Wayland data-control, and a startup failure that is a supported state rather than a crash.

**Files:**
- Modify: `Cargo.toml` (workspace dependency `arboard`)
- Modify: `crates/pheme-clip/Cargo.toml`
- Create: `crates/pheme-clip/src/backend.rs`
- Modify: `crates/pheme-clip/src/lib.rs` (add `mod backend;` and `open()`)

**Interfaces:**
- Consumes: `pheme_clip::{Clipboard, ClipError}` from Task 1.
- Produces: `pheme_clip::open() -> Result<Box<dyn Clipboard>, ClipError>`.

- [ ] **Step 1: Add the dependency**

Root `Cargo.toml`, `[workspace.dependencies]`:

```toml
arboard = { version = "3.6", default-features = false, features = ["wayland-data-control"] }
```

`default-features = false` drops `image-data`, and with it the `image` crate and the macOS graphics stack. `wayland-data-control` pulls `wl-clipboard-rs`, which speaks `ext-data-control-v1` and falls back to `zwlr_data_control_manager_v1`.

`crates/pheme-clip/Cargo.toml`, under `[dependencies]`:

```toml
arboard = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

At the bottom of `crates/pheme-clip/src/backend.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn opening_the_clipboard_never_panics() {
        // Both outcomes are correct. On CI there is no display and this is
        // `Err(Unavailable)`; on a desktop it is `Ok`. What must never happen is
        // a panic, because `open()` runs during startup on every platform and a
        // panic there would take input and audio down with it.
        let _ = crate::open();
    }

    /// The real round trip, which needs a session no CI runner has.
    /// Run by hand with `cargo test -p pheme-clip -- --ignored`.
    #[test]
    #[ignore]
    fn text_survives_a_round_trip_through_the_system_clipboard() {
        use crate::Clipboard;
        let mut c = crate::open().expect("a desktop session");
        c.set_text("pheme round trip ✅ xin chào").unwrap();
        assert_eq!(
            c.get_text().unwrap().as_deref(),
            Some("pheme round trip ✅ xin chào")
        );
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p pheme-clip`
Expected: FAIL — `crate::open` is not defined.

- [ ] **Step 4: Write the backend**

Top of `crates/pheme-clip/src/backend.rs`:

```rust
//! The platform clipboard, through `arboard`.
//!
//! This is the one backend in this repository that is not written here, and
//! §3.3 of the sub-project 5 design says why: a crate exists that does exactly
//! this job on all three platforms, and the X11 half of the job is not a thin
//! wrapper — X11 has no clipboard, only selections, and a process that sets one
//! must own `CLIPBOARD` and answer every `SelectionRequest` from every other
//! client for as long as it holds it.

use arboard::Clipboard as Arboard;

use crate::{ClipError, Clipboard};

pub(crate) struct SystemClipboard(Arboard);

impl SystemClipboard {
    pub(crate) fn open() -> Result<SystemClipboard, ClipError> {
        Arboard::new()
            .map(SystemClipboard)
            .map_err(|e| ClipError::Unavailable(e.to_string()))
    }
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<Option<String>, ClipError> {
        match self.0.get_text() {
            Ok(t) => Ok(Some(t)),
            // Not an error: this is what an empty clipboard and an image on the
            // clipboard both look like, and in both cases there is nothing to send.
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(ClipError::Backend(e.to_string())),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipError> {
        self.0
            .set_text(text)
            .map_err(|e| ClipError::Backend(e.to_string()))
    }
}
```

In `crates/pheme-clip/src/lib.rs`, add `mod backend;` beside `mod sync;` and append:

```rust
/// Opens the platform clipboard.
///
/// `Err(ClipError::Unavailable)` is a supported outcome, not a failure to
/// handle. GNOME's Wayland compositor implements neither `wlr-data-control` nor
/// `ext-data-control` and has declined to, so there is no route for a
/// window-less process; a headless session has no clipboard at all. The caller
/// logs once and runs without clipboard sharing. §3.3.
pub fn open() -> Result<Box<dyn Clipboard>, ClipError> {
    backend::SystemClipboard::open().map(|c| Box::new(c) as Box<dyn Clipboard>)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-clip`
Expected: PASS, 12 tests, 1 ignored.

If the compiler rejects the `arboard::Error::ContentNotAvailable` arm because the enum is `#[non_exhaustive]`, the trailing `Err(e) =>` arm already covers it — no change needed. If a variant name differs in the resolved version, run `cargo doc -p arboard --open` or read `~/.cargo/registry/src/*/arboard-*/src/common.rs` and match the real name; do not invent one.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/pheme-clip
git commit -F - <<'MSG'
Update: add the arboard clipboard backend

One backend covers Win32, X11 through x11rb, and Wayland through
wl-clipboard-rs speaking ext-data-control with a wlr-data-control
fallback. Default features are off, which drops image-data and with it
the image crate and the macOS graphics stack.

This is the one backend in this repository that is not written here.
The reason is in §3.3 of the design: a crate does this job on all three
platforms, and the X11 half is not a thin wrapper, because X11 has no
clipboard, only selections, and setting one means owning CLIPBOARD and
answering SelectionRequest from every other client.

open() failing is a supported state. GNOME's compositor implements no
data-control protocol, so the caller logs once and runs without
clipboard sharing. The test asserts only that open() never panics,
which is the behaviour that matters on a CI runner with no display.

An empty clipboard and an image on the clipboard both read as Ok(None),
not as errors: in both cases there is nothing to send.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 3: The clipboard's own QUIC stream

**Files:**
- Modify: `crates/pheme-net/src/transport.rs` (`PeerSender` around line 369, `Peer::new` around line 405, `Peer` struct around line 396, accessors around line 503)
- Modify: `crates/pheme-net/tests/transport.rs` (append)

**Interfaces:**
- Consumes: `pheme_proto::{MAX_CLIP_BYTES, CLIP_FRAME_SLACK, CLIP_MIME}` from Task 1.
- Produces: `PeerSender::send_clipboard(&self, m: &Msg) -> Result<()>` (async); `Peer::take_clipboard(&mut self) -> mpsc::Receiver<Msg>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/pheme-net/tests/transport.rs`:

```rust
#[tokio::test]
async fn a_clipboard_message_crosses_on_its_own_stream() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        let m = clip.recv().await.unwrap();
        assert_eq!(
            m,
            Msg::Clipboard {
                mime: pheme_proto::CLIP_MIME.to_string(),
                data: b"hello from the client".to_vec(),
            }
        );
        // The control stream still works afterwards.
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"hello from the client".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}

#[tokio::test]
async fn an_oversized_clipboard_stream_is_dropped_and_the_connection_lives() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        // The oversized stream produces nothing, and the small one that follows
        // still arrives: the reader rejects one stream, not the connection.
        let m = clip.recv().await.unwrap();
        assert_eq!(
            m,
            Msg::Clipboard {
                mime: pheme_proto::CLIP_MIME.to_string(),
                data: b"small".to_vec(),
            }
        );
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: vec![b'x'; pheme_proto::MAX_CLIP_BYTES + pheme_proto::CLIP_FRAME_SLACK + 1],
        })
        .await
        .unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"small".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}

#[tokio::test]
async fn a_unidirectional_stream_carrying_something_else_is_ignored() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        // A `Ping` on a clipboard stream must not reach the clipboard channel,
        // and must not be mistaken for input either.
        let m = clip.recv().await.unwrap();
        assert!(matches!(m, Msg::Clipboard { .. }), "got {m:?}");
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    peer.sender().send_clipboard(&Msg::Ping(1)).await.unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"after the ping".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-net --test transport`
Expected: FAIL — `send_clipboard` and `take_clipboard` do not exist.

- [ ] **Step 3: Add the sender**

In `crates/pheme-net/src/transport.rs`, inside `impl PeerSender`, after `send_datagram`:

```rust
    /// Sends one clipboard message on a unidirectional stream of its own.
    ///
    /// Never the control stream. A one-megabyte payload written there would hold
    /// every `Key` and `Button` behind it until the transfer finished, because
    /// QUIC delivers one stream in order — head-of-line blocking on the path
    /// this project optimises before all others.
    ///
    /// The stream boundary is the message boundary, so nothing is
    /// length-prefixed: a stream that carries exactly one message needs no
    /// framing of its own.
    ///
    /// `async` rather than self-spawning because its caller is the clipboard
    /// worker, an ordinary OS thread with no reactor; that caller spawns this
    /// onto the runtime, so nothing on the input path ever waits for it.
    pub async fn send_clipboard(&self, m: &Msg) -> Result<()> {
        let mut send = self.conn.open_uni().await.map_err(conn_err)?;
        let mut buf = Vec::with_capacity(256);
        encode(m, &mut buf);
        send.write_all(&buf)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        send.finish()
            .map_err(|e| NetError::Connection(e.to_string()))?;
        Ok(())
    }
```

Add `NetError` to the file's imports if it is not already there.

- [ ] **Step 4: Add the reader and the accessor**

Near `CONTROL_BUFFER` and `AUDIO_BUFFER`, add:

```rust
/// Clipboard messages queued for the application. Shallow on purpose: the
/// clipboard is exchanged when the pointer crosses, so more than a couple in
/// flight means something is wrong, and dropping the oldest is correct — only
/// the newest clipboard matters.
const CLIP_BUFFER: usize = 4;
```

In `Peer`, add the field `clipboard: Option<mpsc::Receiver<Msg>>,`.

In `Peer::new`, after the datagram reader task:

```rust
        let (clip_tx, clip_rx) = mpsc::channel(CLIP_BUFFER);
        let clip_conn = conn.clone();
        tokio::spawn(async move {
            loop {
                let mut recv = match clip_conn.accept_uni().await {
                    Ok(r) => r,
                    Err(e) => {
                        debug!("unidirectional stream reader ended: {e}");
                        break;
                    }
                };
                let tx = clip_tx.clone();
                // Each stream is read in its own task so one oversized or slow
                // sender cannot hold up the stream behind it.
                tokio::spawn(async move {
                    let limit = MAX_CLIP_BYTES + CLIP_FRAME_SLACK;
                    let bytes = match recv.read_to_end(limit).await {
                        Ok(b) => b,
                        Err(e) => {
                            debug!("clipboard stream refused: {e}");
                            return;
                        }
                    };
                    match decode(&bytes) {
                        Ok(m @ Msg::Clipboard { .. }) => {
                            // Dropping the oldest is right here: only the newest
                            // clipboard is worth having.
                            if tx.try_send(m).is_err() {
                                debug!("clipboard channel full; dropping a message");
                            }
                        }
                        Ok(other) => {
                            debug!("a unidirectional stream carried {other:?}, not a clipboard")
                        }
                        Err(e) => debug!("undecodable clipboard message: {e}"),
                    }
                });
            }
        });
```

In the `Peer { .. }` literal at the end of `new`, add `clipboard: Some(clip_rx),`.

Add the accessor beside `take_audio`:

```rust
    /// Takes the receiver carrying `Msg::Clipboard` and nothing else. Panics if
    /// called twice.
    pub fn take_clipboard(&mut self) -> mpsc::Receiver<Msg> {
        self.clipboard
            .take()
            .expect("clipboard receiver already taken")
    }
```

Import the constants: `use pheme_proto::{CLIP_FRAME_SLACK, MAX_CLIP_BYTES, ...};` alongside the existing `pheme_proto` imports.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-net --test transport`
Expected: PASS, including the three new tests.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-net
git commit -F - <<'MSG'
Update: carry the clipboard on its own QUIC stream

The clipboard does not travel on the control stream. That stream carries
Key, Button, Enter and Leave, and QUIC delivers one stream in order, so
a one-megabyte clipboard written there would hold every keystroke behind
it until the transfer finished. One unidirectional stream per clipboard
change is what architecture §6 specifies, and this is the reason.

The stream boundary is the message boundary: no length prefix, unlike
the control stream, because a stream carrying exactly one message needs
no framing of its own.

The reader enforces the same cap the sender's policy does, reading each
stream to its end under MAX_CLIP_BYTES + CLIP_FRAME_SLACK. A stream over
the limit, one that decodes to something other than a clipboard, and one
that does not decode at all are each dropped with a debug line while the
connection keeps working: a peer with different ideas about clipboard
content must not be able to take the input link down.

Each stream is read in its own task so one slow sender cannot hold up
the next stream, and a full channel drops the oldest message, because
only the newest clipboard is worth having.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 4: The clipboard worker

The thread that owns the clipboard handle and the policy, and the two moments it acts on. This task also carries the four Review Focus items about malformed and failing input.

**Files:**
- Create: `crates/pheme-app/src/clipboard.rs`
- Modify: `crates/pheme-app/src/lib.rs` (add `pub mod clipboard;`)
- Modify: `crates/pheme-app/Cargo.toml` (add `pheme-clip`)

**Interfaces:**
- Consumes: `pheme_clip::{Clipboard, ClipError, ClipSync}`, `pheme_clip::mock::{MockClipboard, MockClipboardHandle}` (Task 1); `pheme_net::PeerSender::send_clipboard` (Task 3); `pheme_proto::CLIP_MIME` (Task 1).
- Produces: `pheme_app::clipboard::ClipboardService` with `spawn(open: impl FnOnce() -> Result<Box<dyn Clipboard>, ClipError> + Send + 'static) -> Option<ClipboardService>`, `send_to(&self, peer: PeerSender)`, `apply(&self, m: &Msg)`. `ClipboardService` is `Clone`.

- [ ] **Step 1: Add the dependency**

`crates/pheme-app/Cargo.toml`, under `[dependencies]`:

```toml
pheme-clip = { workspace = true }
```

`crossbeam-channel` is already a dependency of this crate.

- [ ] **Step 2: Write the failing tests**

At the bottom of `crates/pheme-app/src/clipboard.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pheme_clip::mock::MockClipboard;
    use pheme_clip::ClipError;
    use std::time::Duration;

    /// The service does its work on another thread, so tests wait for an effect
    /// rather than assuming it has already happened.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[tokio::test]
    async fn an_unavailable_clipboard_yields_no_service() {
        // GNOME Wayland and a headless session both land here, and both must
        // leave the rest of the program running.
        let s = ClipboardService::spawn(|| Err(ClipError::Unavailable("no display".into())));
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn an_incoming_message_reaches_the_clipboard() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"xin ch\xc3\xa0o".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("xin chào")));
    }

    #[tokio::test]
    async fn a_message_that_is_not_utf8_is_ignored() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: vec![0xff, 0xfe, 0xfd],
        });
        // Then something valid, to prove the service is still alive rather than
        // merely slow.
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"after".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("after")));
        assert_eq!(handle.sets(), 1, "the invalid message must not be written");
    }

    #[tokio::test]
    async fn a_message_in_an_unknown_format_is_ignored() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: "image/png".to_string(),
            data: b"not text".to_vec(),
        });
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"text".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("text")));
        assert_eq!(handle.sets(), 1, "the png must not be written");
    }

    #[tokio::test]
    async fn a_failing_clipboard_does_not_stop_the_service() {
        // A compositor restart, or an X11 selection owner that went away: the
        // session must continue, and a later working call must still work.
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        handle.fail_with("the compositor went away");
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"lost".to_vec(),
        });
        assert!(eventually(|| handle.sets() == 0));
        handle.stop_failing();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"found".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("found")));
    }

    #[tokio::test]
    async fn the_same_text_is_written_once() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        for _ in 0..3 {
            s.apply(&Msg::Clipboard {
                mime: CLIP_MIME.to_string(),
                data: b"once".to_vec(),
            });
        }
        assert!(eventually(|| handle.text().as_deref() == Some("once")));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(handle.sets(), 1);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib clipboard`
Expected: FAIL — `ClipboardService` is not defined.

- [ ] **Step 4: Write the service**

Top of `crates/pheme-app/src/clipboard.rs`:

```rust
//! The clipboard worker: one thread that owns the clipboard handle and the
//! policy, and the two moments the clipboard crosses the network.
//!
//! Sub-project 5 design §3.1, §3.4 and §3.6.

use std::thread;

use crossbeam_channel::{unbounded, Sender};
use pheme_clip::{ClipError, ClipSync, Clipboard};
use pheme_net::PeerSender;
use pheme_proto::{Msg, CLIP_MIME};
use tokio::runtime::Handle;
use tracing::{debug, warn};

enum Cmd {
    /// Read the local clipboard and, if the policy allows, send it to this peer.
    SendTo(PeerSender),
    /// Text that arrived from the peer.
    Apply(String),
}

/// A handle to the clipboard worker. Cheap to clone; every clone reaches the
/// same thread, and therefore the same `ClipSync`.
#[derive(Clone)]
pub struct ClipboardService {
    tx: Sender<Cmd>,
}

impl ClipboardService {
    /// Starts the worker, or returns `None` when no clipboard is reachable.
    ///
    /// `open` runs *on the worker thread*, and the handle it produces never
    /// leaves that thread. On X11 a process owns the `CLIPBOARD` selection from
    /// the thread that created the handle, and that thread has to outlive every
    /// use of it. Taking a factory rather than a handle is what makes that true
    /// by construction — and it is also how a test injects a mock.
    ///
    /// `None` is a supported state, not a failure: GNOME's Wayland compositor
    /// implements no data-control protocol. Callers keep running without
    /// clipboard sharing.
    pub fn spawn(
        open: impl FnOnce() -> Result<Box<dyn Clipboard>, ClipError> + Send + 'static,
    ) -> Option<ClipboardService> {
        let handle = Handle::current();
        let (tx, rx) = unbounded();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("pheme-clipboard".into())
            .spawn(move || {
                let mut clip = match open() {
                    Ok(c) => {
                        let _ = ready_tx.send(true);
                        c
                    }
                    Err(e) => {
                        warn!("clipboard sharing is off: {e}");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let mut sync = ClipSync::new();
                // A clipboard that has stopped answering would otherwise log on
                // every single crossing. The first failure is worth a warning;
                // the rest are not.
                let mut reported = false;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::SendTo(peer) => {
                            let text = match clip.get_text() {
                                Ok(Some(t)) => {
                                    reported = false;
                                    t
                                }
                                Ok(None) => continue,
                                Err(e) => {
                                    if reported {
                                        debug!("reading the clipboard failed: {e}");
                                    } else {
                                        warn!("reading the clipboard failed: {e}");
                                        reported = true;
                                    }
                                    continue;
                                }
                            };
                            let Some(text) = sync.outgoing(text) else {
                                continue;
                            };
                            let m = Msg::Clipboard {
                                mime: CLIP_MIME.to_string(),
                                data: text.into_bytes(),
                            };
                            // The send belongs to the runtime, not to this
                            // thread: the pointer handover that triggered it
                            // must not wait for a network round trip.
                            handle.spawn(async move {
                                if let Err(e) = peer.send_clipboard(&m).await {
                                    debug!("clipboard send failed: {e}");
                                }
                            });
                        }
                        Cmd::Apply(text) => {
                            if !sync.incoming(&text) {
                                continue;
                            }
                            if let Err(e) = clip.set_text(&text) {
                                warn!("writing the clipboard failed: {e}");
                            }
                        }
                    }
                }
            })
            .expect("spawning the clipboard thread");
        match ready_rx.recv() {
            Ok(true) => Some(ClipboardService { tx }),
            // `Err` means the thread ended before reporting, which is the same
            // outcome for the caller as an unavailable clipboard.
            _ => None,
        }
    }

    /// Reads the local clipboard and sends it to `peer`, on the worker thread.
    /// Returns immediately; nothing on the input path waits for it.
    pub fn send_to(&self, peer: PeerSender) {
        let _ = self.tx.send(Cmd::SendTo(peer));
    }

    /// Applies a `Msg::Clipboard` that arrived from the peer. Anything else is
    /// ignored.
    pub fn apply(&self, m: &Msg) {
        let Msg::Clipboard { mime, data } = m else {
            return;
        };
        if mime != CLIP_MIME {
            debug!(%mime, "clipboard message in a format pheme does not speak; ignored");
            return;
        }
        match std::str::from_utf8(data) {
            Ok(text) => {
                let _ = self.tx.send(Cmd::Apply(text.to_string()));
            }
            // A peer sending bytes that are not text is not a reason to stop.
            Err(e) => debug!("clipboard message was not valid UTF-8: {e}"),
        }
    }
}
```

Add `pub mod clipboard;` to `crates/pheme-app/src/lib.rs`, in alphabetical order after `pub mod client;`… place it as `pub mod clipboard;` immediately before `pub mod client;` so the list stays sorted.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib clipboard`
Expected: PASS, 6 tests.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-app/Cargo.toml crates/pheme-app/src/lib.rs crates/pheme-app/src/clipboard.rs
git commit -F - <<'MSG'
Update: add the clipboard worker thread

One thread owns the clipboard handle and the ClipSync policy for the
life of the process and takes commands on a channel. It has to be one
thread: on X11 a process owns the CLIPBOARD selection from the thread
that created the handle, and that thread must outlive every use of it.
spawn() therefore takes a factory rather than a handle, so the handle is
created where it will be used and never crosses a thread. A test injects
a mock through the same door.

Because one thread owns both the handle and the policy, two clipboard
messages arriving at once cannot interleave.

Sending is handed to the runtime rather than done here: the pointer
handover that triggered it must not wait for a network round trip.

The awkward inputs are covered. A message whose bytes are not UTF-8, and
one in a format pheme does not speak, are both dropped with a debug line
while the service keeps working. A clipboard that fails mid-session logs
the first failure at warn and the rest at debug, so a compositor that
went away does not log on every crossing, and a later working call still
works.

spawn() returning None is a supported state, not an error path: that is
GNOME Wayland, and everything else keeps running.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 5: Hook the clipboard into the server and the client

**Files:**
- Modify: `crates/pheme-app/src/server.rs` (`ServerDeps` line 33, `Shared` line 80, `run_actions` line 116, `session` line 537)
- Modify: `crates/pheme-app/src/client.rs` (`ClientDeps` line 22, the session loop around line 270)

**Interfaces:**
- Consumes: `pheme_app::clipboard::ClipboardService` (Task 4); `Peer::take_clipboard` (Task 3).
- Produces: `ServerDeps.clipboard: Option<ClipboardService>`, `ClientDeps.clipboard: Option<ClipboardService>`. Both default to `None` in tests that do not care.

- [ ] **Step 1: Add the field to `ServerDeps` and `Shared`**

In `crates/pheme-app/src/server.rs`, add to `ServerDeps` after `mic_counters`:

```rust
    /// The clipboard worker, or `None` where no clipboard is reachable — GNOME
    /// Wayland, or a headless session. `None` disables clipboard sharing and
    /// nothing else.
    pub clipboard: Option<ClipboardService>,
```

Add the same field to `Shared`, and set it where `Shared` is constructed from `ServerDeps`.

- [ ] **Step 2: Send the clipboard when the pointer leaves**

In `run_actions`, replace the `Action::SendControl(m)` arm with:

```rust
                Action::SendControl(m) => {
                    if let Some(l) = &link {
                        // The clipboard crosses with the pointer (§3.1). Reading
                        // it happens on the clipboard thread and the send happens
                        // on the runtime, so this call returns at once and the
                        // handover below is not delayed by either.
                        if matches!(m, Msg::Enter { .. }) {
                            if let Some(c) = &self.clipboard {
                                c.send_to(l.sender.clone());
                            }
                        }
                        let _ = l.control.send(m);
                        self.counters.control_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
```

- [ ] **Step 3: Apply what arrives, on the server**

In `session` (around line 541, beside `let mut audio_rx = peer.take_audio();`):

```rust
    let mut clip_rx = peer.take_clipboard();
```

and add an arm to the session's `tokio::select!` loop, beside the audio arm:

```rust
            m = clip_rx.recv() => match m {
                Some(m) => {
                    if let Some(c) = &shared.clipboard {
                        c.apply(&m);
                    }
                }
                None => break Ok(()),
            },
```

- [ ] **Step 4: Add the field to `ClientDeps` and send on `Leave`**

In `crates/pheme-app/src/client.rs`, add to `ClientDeps`:

```rust
    /// The clipboard worker, or `None` where no clipboard is reachable.
    pub clipboard: Option<ClipboardService>,
```

Thread it into the session function. In the session's message loop, add an arm for `Msg::Leave` *before* the generic `Some(m)` arm, and extend the `Msg::Bye` arm:

```rust
                Some(m @ Msg::Leave { .. }) => {
                    // The pointer is going back to the server, so the client's
                    // clipboard goes with it (§3.1).
                    if let Some(c) = &clipboard {
                        c.send_to(sender.clone());
                    }
                    received += 1;
                    if let Some(seq) = input_seq(&m) {
                        let (gap, next) = count_gap(last_seq, seq);
                        lost += gap;
                        last_seq = next;
                    }
                    for a in core.on_msg(&m) { apply(inject, a); }
                }
```

and in the existing `Some(Msg::Bye { reason })` arm, before the core call:

```rust
                    if let Some(c) = &clipboard {
                        c.send_to(sender.clone());
                    }
```

- [ ] **Step 5: Apply what arrives, on the client**

Beside `let mut audio_rx = peer.take_audio();`:

```rust
    let mut clip_rx = peer.take_clipboard();
```

and an arm in the same `select!`:

```rust
            m = clip_rx.recv() => match m {
                Some(m) => {
                    if let Some(c) = &clipboard {
                        c.apply(&m);
                    }
                }
                None => break Ok(()),
            },
```

- [ ] **Step 6: Build the service in both entry points**

In `server::main`, before constructing `ServerDeps`:

```rust
    let clipboard = ClipboardService::spawn(pheme_clip::open);
```

and pass `clipboard` in the struct literal. Do the same in `client::main`. `ClipboardService::spawn` already logs the reason when it returns `None`; do not log again here.

- [ ] **Step 7: Fix the existing tests**

Every existing construction of `ServerDeps` and `ClientDeps` — in `crates/pheme-app/tests/integration.rs`, `audio.rs` and `mic_e2e.rs` — needs `clipboard: None,`. Add it; change nothing else in those files.

- [ ] **Step 8: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green, with every existing test still passing.

- [ ] **Step 9: Commit**

```bash
git add crates/pheme-app
git commit -F - <<'MSG'
Update: exchange the clipboard when the pointer crosses

The server sends its clipboard with the Enter that hands the pointer
over, and the client sends its own when Leave or Bye brings the pointer
back. Both sides apply whatever arrives on the clipboard stream. That is
the whole of the timing: the clipboard is never read in the background,
which is both what the user means by a copy and the only model Wayland
can support for a window-less process.

pheme-core does not change. It needs no new action and no new state; the
app layer already sees every SendControl the core emits and every
message that arrives, which is enough.

Neither send blocks. run_actions hands the work to the clipboard thread
and moves straight on to the control message that performs the
handover, so reading the clipboard cannot delay the switch.

A None clipboard disables sharing and nothing else, which is how GNOME
Wayland and a headless session run.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 6: Clipboard end to end over real QUIC

**Files:**
- Create: `crates/pheme-app/tests/clipboard.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5. Follow the harness in `crates/pheme-app/tests/integration.rs`: `spawn_pair`-style setup with `MockCapture`/`MockInject`, `wait_until`, `Endpoint::server` on `127.0.0.1:0`.

- [ ] **Step 1: Write the failing tests**

`crates/pheme-app/tests/clipboard.rs`. Copy these helpers from `integration.rs` verbatim — they already exist there and are the only correct way to drive the mock capture: `screens`, `wait_until`, `bind_server_retrying`, `spawn_pair_with_hotkeys` (as the model for `spawn_clip_pair`), `push_edge_crossing` and `wait_connected`.

Two facts about those helpers, so the test drives them correctly:

- **Crossing to the client** is `push_edge_crossing(&cap)`, which pushes `CaptureEvent::MotionAbs { x: 1900, y: 540 }` then `{ x: 1919, y: 540 }` — the mock server screen is 1920 wide with the client on its right edge.
- **Returning to the server** is `cap.push(CaptureEvent::MotionRel { dx: -20_000, dy: 0 })`, which is how every existing test walks the virtual pointer back off the client's left edge.
- `wait_connected(&cap)` must run before any assertion about the clipboard: it waits for capture to start *and* for one crossing to be accepted, which is what proves the client is connected. It leaves the core **remote**, so a test that wants to start from the server side pushes the return motion after it.

Then:

```rust
/// A paired server and client on loopback, each with its own mock clipboard.
struct ClipPair {
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: tokio::task::JoinHandle<anyhow::Result<()>>,
    shutdown_tx: watch::Sender<bool>,
    cap: MockCaptureHandle,
    server_clip: MockClipboardHandle,
    client_clip: MockClipboardHandle,
}

#[tokio::test]
async fn the_server_clipboard_reaches_the_client_when_the_pointer_crosses() {
    let p = spawn_clip_pair().await;
    p.server_clip.copy("copied on the server");
    cross_to_the_client(&p.cap).await;
    assert!(
        wait_until(|| p.client_clip.text().as_deref() == Some("copied on the server"),
                   Duration::from_secs(5)).await,
        "the client's clipboard never received the server's text"
    );
    p.shutdown().await;
}

#[tokio::test]
async fn the_client_clipboard_returns_when_the_pointer_comes_back() {
    let p = spawn_clip_pair().await;
    cross_to_the_client(&p.cap).await;
    p.client_clip.copy("copied on the client");
    cross_back_to_the_server(&p.cap).await;
    assert!(
        wait_until(|| p.server_clip.text().as_deref() == Some("copied on the client"),
                   Duration::from_secs(5)).await,
        "the server's clipboard never received the client's text"
    );
    p.shutdown().await;
}

#[tokio::test]
async fn crossing_twice_without_copying_sends_one_clipboard() {
    let p = spawn_clip_pair().await;
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

#[tokio::test]
async fn a_client_without_a_clipboard_still_crosses_the_edge() {
    // GNOME Wayland on the client: `ClipboardService::spawn` returned None.
    // Input must be completely unaffected.
    let p = spawn_clip_pair_without_client_clipboard().await;
    cross_to_the_client(&p.cap).await;
    assert!(
        wait_until(|| p.injected_enter(), Duration::from_secs(5)).await,
        "the client never received Enter"
    );
    p.shutdown().await;
}
```

Define the two local helpers in terms of the real ones:

```rust
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
        wait_until(|| cap.mode() == CaptureMode::Passive, Duration::from_secs(5)).await,
        "the pointer never came back to the server"
    );
}
```

Check the name of the non-grabbing mode against `pheme_input::CaptureMode` before using it; `integration.rs` already asserts on it in `locking_withdraws_the_edges_and_unlocking_puts_them_back`, so copy the variant from there.

`injected_enter` reads the `MockInjectLog` for the `InjectCall` the client makes when it applies `Msg::Enter`; `integration.rs`'s `server_and_client_exchange_input_over_quic` already asserts on that call, so take the pattern from there rather than guessing the variant.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --test clipboard`
Expected: FAIL — the helpers do not exist yet.

- [ ] **Step 3: Write the harness**

Build `spawn_clip_pair` by copying `spawn_pair` from `integration.rs` and adding, on each side:

```rust
    let (server_clip_mock, server_clip) = MockClipboard::new();
    let server_clipboard = ClipboardService::spawn(move || {
        Ok(Box::new(server_clip_mock) as Box<dyn Clipboard>)
    });
```

passing `clipboard: server_clipboard` into `ServerDeps` and the client's equivalent into `ClientDeps`. `spawn_clip_pair_without_client_clipboard` passes `clipboard: None` on the client side.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --test clipboard`
Expected: PASS, 4 tests.

- [ ] **Step 5: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/pheme-app/tests/clipboard.rs
git commit -F - <<'MSG'
Update: test the clipboard end to end over real QUIC

Server and client run in one process over a real QUIC connection with a
mock clipboard on each side, so the test exercises the transport, the
worker thread and the policy together rather than any one of them alone.

Four things are pinned. Text copied on the server arrives on the client
when the pointer crosses. Text copied on the client comes back when the
pointer returns. Crossing twice without copying writes the far clipboard
once, which is the echo guard doing its job and the assertion that would
have caught a bouncing clipboard. And a client whose clipboard never
opened still crosses the edge normally, because that is GNOME Wayland
and input must not notice.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 7: mDNS advertise, browse and resolve

**Files:**
- Modify: `Cargo.toml` (workspace dependency `mdns-sd`)
- Modify: `crates/pheme-net/Cargo.toml`
- Create: `crates/pheme-net/src/discovery.rs`
- Modify: `crates/pheme-net/src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pheme_net::discovery::{Advertiser, Found, advertise, browse, resolve}`. `advertise(name: &str, port: u16, fingerprint: &str) -> Result<Advertiser>`; `browse(timeout: Duration) -> Result<Vec<Found>>` (async); `resolve(name: &str, timeout: Duration) -> Result<Option<SocketAddr>>` (async); `Found { name: String, addr: SocketAddr, fingerprint: Option<String> }`; `pub const SERVICE_TYPE: &str = "_pheme._udp.local.";`

- [ ] **Step 1: Add the dependency**

Root `Cargo.toml`, `[workspace.dependencies]`: `mdns-sd = "0.21"`.
`crates/pheme-net/Cargo.toml`, `[dependencies]`: `mdns-sd = { workspace = true }`.

- [ ] **Step 2: Write the failing tests**

At the bottom of `crates/pheme-net/src/discovery.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_name_keeps_its_dots() {
        // DNS-SD escapes a dot inside an instance label. `resolve` matches on
        // the unescaped name, so a machine called "my.desk" must still be
        // found — this is the assertion that catches an implementation
        // comparing raw label bytes.
        assert_eq!(instance_name("my.desk"), "my\\.desk._pheme._udp.local.");
        assert_eq!(name_from_instance("my\\.desk._pheme._udp.local."), "my.desk");
    }

    #[test]
    fn a_plain_name_round_trips() {
        assert_eq!(instance_name("desk-linux"), "desk-linux._pheme._udp.local.");
        assert_eq!(
            name_from_instance("desk-linux._pheme._udp.local."),
            "desk-linux"
        );
    }

    /// The real round trip. Ignored because GitHub's runners do not reliably
    /// carry multicast, and a test that passes because nothing listened is
    /// worse than no test. Run by hand:
    /// `cargo test -p pheme-net --lib discovery -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn a_registered_service_can_be_resolved() {
        let _a = advertise("pheme-test-instance", 24800, "deadbeef").unwrap();
        let addr = resolve("pheme-test-instance", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(addr.is_some(), "the service we just registered was not found");
        let found = browse(Duration::from_secs(5)).await.unwrap();
        let ours = found
            .iter()
            .find(|f| f.name == "pheme-test-instance")
            .expect("our own instance");
        assert_eq!(ours.fingerprint.as_deref(), Some("deadbeef"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pheme-net --lib discovery`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Write the module**

`crates/pheme-net/src/discovery.rs`:

```rust
//! Finding a Pheme server on the local network.
//!
//! The server registers `_pheme._udp.local.` under the name from its
//! configuration; the client resolves that name. Discovery never chooses a
//! server: it answers the question "where is the machine called X", and the
//! user says which X. Sub-project 5 design §4.

use std::net::SocketAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::debug;

use crate::{NetError, Result};

/// The DNS-SD service type. QUIC runs over UDP, hence `_udp`.
pub const SERVICE_TYPE: &str = "_pheme._udp.local.";

/// The TXT key carrying the server's certificate fingerprint.
const TXT_FINGERPRINT: &str = "fp";
/// The TXT key carrying the advertisement format version.
const TXT_VERSION: &str = "v";

/// A server Pheme found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub addr: SocketAddr,
    /// The fingerprint the server published. Advisory only: it is shown so a
    /// person can compare it with what `pheme pair` prints. Trust comes from
    /// pairing, never from a TXT record.
    pub fingerprint: Option<String>,
}

/// The full DNS-SD instance name for `name`, with dots escaped as the
/// specification requires inside an instance label.
fn instance_name(name: &str) -> String {
    format!("{}.{SERVICE_TYPE}", name.replace('.', "\\."))
}

/// The instance label back out of a full DNS-SD name, with escapes undone.
fn name_from_instance(full: &str) -> String {
    let label = full.strip_suffix(&format!(".{SERVICE_TYPE}")).unwrap_or(full);
    label.replace("\\.", ".")
}

/// A live advertisement. Dropping it unregisters the service, so a server that
/// exits cleanly stops answering at once instead of leaving a stale record to
/// time out.
pub struct Advertiser {
    daemon: ServiceDaemon,
    full_name: String,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.full_name);
        let _ = self.daemon.shutdown();
    }
}

fn daemon() -> Result<ServiceDaemon> {
    ServiceDaemon::new().map_err(|e| NetError::Connection(format!("mdns: {e}")))
}

/// Publishes this server on the local network.
pub fn advertise(name: &str, port: u16, fingerprint: &str) -> Result<Advertiser> {
    let daemon = daemon()?;
    let props = [(TXT_FINGERPRINT, fingerprint), (TXT_VERSION, "1")];
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        name,
        &format!("{}.local.", name.replace('.', "-")),
        (),
        port,
        &props[..],
    )
    .map_err(|e| NetError::Connection(format!("mdns: {e}")))?
    .enable_addr_auto();
    let full_name = info.get_fullname().to_string();
    daemon
        .register(info)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;
    Ok(Advertiser { daemon, full_name })
}

/// Every Pheme server seen before `timeout` elapses.
pub async fn browse(timeout: Duration) -> Result<Vec<Found>> {
    let daemon = daemon()?;
    let rx = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;
    let found = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + timeout;
        let mut out: Vec<Found> = Vec::new();
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let Some(addr) = info
                        .get_addresses()
                        .iter()
                        .find(|a| a.is_ipv4())
                        .map(|a| SocketAddr::new(**a, info.get_port()))
                    else {
                        continue;
                    };
                    let name = name_from_instance(info.get_fullname());
                    if out.iter().any(|f| f.name == name && f.addr == addr) {
                        continue;
                    }
                    out.push(Found {
                        name,
                        addr,
                        fingerprint: info.get_property_val_str(TXT_FINGERPRINT).map(str::to_string),
                    });
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
        out
    })
    .await
    .map_err(|e| NetError::Connection(format!("mdns task: {e}")))?;
    debug!(count = found.len(), "mdns browse finished");
    Ok(found)
}

/// The address of the server called `name`, or `None` if none answered in time.
///
/// Returns as soon as a match resolves rather than waiting out the timeout, so
/// a reconnect that finds its server costs one round trip, not three seconds.
pub async fn resolve(name: &str, timeout: Duration) -> Result<Option<SocketAddr>> {
    let daemon = daemon()?;
    let rx = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;
    let wanted = name.to_string();
    let addr = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if name_from_instance(info.get_fullname()) != wanted {
                        continue;
                    }
                    if let Some(a) = info.get_addresses().iter().find(|a| a.is_ipv4()) {
                        let _ = daemon.shutdown();
                        return Some(SocketAddr::new(**a, info.get_port()));
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
        None
    })
    .await
    .map_err(|e| NetError::Connection(format!("mdns task: {e}")))?;
    Ok(addr)
}
```

In `crates/pheme-net/src/lib.rs` add `pub mod discovery;` beside the other modules and re-export:

```rust
pub use discovery::{advertise, browse, resolve, Advertiser, Found};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-net --lib discovery`
Expected: PASS, 2 tests, 1 ignored.

If `mdns-sd` 0.21's API differs from the calls above — method names such as `get_property_val_str`, `enable_addr_auto` or the `ServiceInfo::new` signature — read the installed source under `~/.cargo/registry/src/*/mdns-sd-0.21*/src/` and adapt. Do not change what the module promises: the service type, the TXT keys, the `Found` shape and the dot-escaping behaviour are fixed by this plan.

- [ ] **Step 6: Run the ignored test by hand**

Run: `cargo test -p pheme-net --lib discovery -- --ignored`
Expected: PASS on a normal LAN-connected machine. If it fails because the environment blocks multicast, record that in the task report and move on — the test is ignored for exactly this reason.

- [ ] **Step 7: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/pheme-net
git commit -F - <<'MSG'
Update: add mDNS advertising and lookup to pheme-net

A server registers _pheme._udp.local. under the name from its
configuration, with its certificate fingerprint and a format version in
TXT. The advertisement unregisters on drop, so a server that exits
cleanly stops answering at once rather than leaving a stale record to
time out.

The fingerprint is published so a person can compare it with what
pheme pair prints. It is advisory: trust comes from the pairing code and
never from a TXT record.

resolve returns as soon as a matching instance resolves instead of
waiting out its timeout, so a reconnect that finds its server costs one
round trip rather than three seconds.

Instance names escape and unescape dots, because DNS-SD escapes a dot
inside an instance label and a machine called "my.desk" must still be
found. That is what the unit tests pin.

The real round trip is an ignored test. GitHub's runners do not reliably
carry multicast, and a test that passes because nothing was listening is
worse than no test at all.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 8: `Target` — how a `connect` string becomes an address

**Files:**
- Create: `crates/pheme-app/src/target.rs`
- Modify: `crates/pheme-app/src/lib.rs` (`pub mod target;`)
- Modify: `crates/pheme-app/src/config.rs` (add `Config::connect_target`)

**Interfaces:**
- Consumes: `pheme_net::discovery::resolve` (Task 7), `pheme_net::DEFAULT_PORT`.
- Produces: `pheme_app::target::Target` with `Target::parse(s: &str) -> anyhow::Result<Target>` and `async fn Target::resolve(&self) -> anyhow::Result<SocketAddr>`; `Config::connect_target(&self, override_host: Option<&str>) -> anyhow::Result<Target>`.

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/pheme-app/src/target.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_address_is_used_as_written() {
        assert_eq!(
            Target::parse("192.168.1.5:24800").unwrap(),
            Target::Fixed("192.168.1.5:24800".parse().unwrap())
        );
    }

    #[test]
    fn an_ipv6_literal_is_a_socket_address() {
        assert_eq!(
            Target::parse("[::1]:24800").unwrap(),
            Target::Fixed("[::1]:24800".parse().unwrap())
        );
    }

    #[test]
    fn a_bare_ip_goes_to_the_resolver() {
        // It has a dot, so it takes the path that already worked before mDNS
        // existed; `ToSocketAddrs` turns it into an address without a lookup.
        assert_eq!(
            Target::parse("10.0.0.4").unwrap(),
            Target::Dns("10.0.0.4".into())
        );
    }

    #[test]
    fn a_host_and_port_goes_to_the_resolver() {
        assert_eq!(
            Target::parse("laptop:24800").unwrap(),
            Target::Dns("laptop:24800".into())
        );
    }

    #[test]
    fn a_dotted_hostname_goes_to_the_resolver() {
        assert_eq!(
            Target::parse("laptop.lan").unwrap(),
            Target::Dns("laptop.lan".into())
        );
    }

    #[test]
    fn a_bare_label_is_an_mdns_name() {
        assert_eq!(
            Target::parse("laptop-win").unwrap(),
            Target::Mdns("laptop-win".into())
        );
    }

    #[test]
    fn an_empty_target_is_an_error() {
        // Never an mDNS browse for the empty name: that would wait out the
        // timeout on every reconnect and never find anything.
        assert!(Target::parse("").is_err());
    }

    #[test]
    fn a_whitespace_target_is_an_error() {
        assert!(Target::parse("   ").is_err());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            Target::parse("  laptop-win  ").unwrap(),
            Target::Mdns("laptop-win".into())
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib target`
Expected: FAIL — `Target` is not defined.

- [ ] **Step 3: Write `Target`**

Top of `crates/pheme-app/src/target.rs`:

```rust
//! How the `connect` string becomes an address.
//!
//! Sub-project 5 design §4.3. Parsing is separate from resolving so that every
//! rule about which string means what is testable without a network.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context};
use pheme_net::DEFAULT_PORT;
use tracing::debug;

/// How long to wait for an mDNS answer before falling back to the resolver.
const MDNS_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An address written out in full.
    Fixed(SocketAddr),
    /// A name for the system resolver, with or without a port.
    Dns(String),
    /// An mDNS instance name.
    Mdns(String),
}

impl Target {
    /// Classifies `s`. The rules are ordered, and the first that matches wins.
    pub fn parse(s: &str) -> anyhow::Result<Target> {
        let s = s.trim();
        if s.is_empty() {
            bail!("no server address: pass HOST or set `connect` in the config");
        }
        if let Ok(a) = s.parse::<SocketAddr>() {
            return Ok(Target::Fixed(a));
        }
        // A colon means a port, a dot means a hostname or an IP. Either way the
        // system resolver has always handled it, and this keeps doing that.
        if s.contains(':') || s.contains('.') {
            return Ok(Target::Dns(s.to_string()));
        }
        Ok(Target::Mdns(s.to_string()))
    }

    /// The address to connect to, looked up afresh.
    ///
    /// Called on every reconnect attempt rather than once at startup, which is
    /// what makes a server that changed address or restarted on another port
    /// reachable again without restarting the client.
    pub async fn resolve(&self) -> anyhow::Result<SocketAddr> {
        match self {
            Target::Fixed(a) => Ok(*a),
            Target::Dns(host) => resolve_dns(host),
            Target::Mdns(name) => {
                match pheme_net::discovery::resolve(name, MDNS_TIMEOUT).await {
                    Ok(Some(a)) => Ok(a),
                    Ok(None) => {
                        // A bare name the local resolver knows still works.
                        debug!(%name, "no mdns answer; trying the system resolver");
                        resolve_dns(name)
                    }
                    Err(e) => {
                        debug!(%name, "mdns lookup failed: {e}; trying the system resolver");
                        resolve_dns(name)
                    }
                }
            }
        }
    }
}

fn resolve_dns(host: &str) -> anyhow::Result<SocketAddr> {
    let with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };
    let mut addrs = with_port
        .to_socket_addrs()
        .with_context(|| format!("resolving {with_port}"))?;
    match addrs.find(|a| a.is_ipv4()) {
        Some(a) => Ok(a),
        None => bail!("{with_port} did not resolve to an IPv4 address"),
    }
}
```

Add `pub mod target;` to `crates/pheme-app/src/lib.rs`.

- [ ] **Step 4: Add `Config::connect_target`**

In `crates/pheme-app/src/config.rs`, beside `connect_addr` (line 222):

```rust
    /// The server to connect to, as a target that is resolved on every attempt.
    ///
    /// `connect_addr` resolves once and returns an address; this returns the
    /// *question*, so the reconnect loop can ask it again after the answer
    /// changes. §4.3.
    pub fn connect_target(&self, override_host: Option<&str>) -> anyhow::Result<Target> {
        let host = override_host.or(self.connect.as_deref()).unwrap_or("");
        Target::parse(host)
    }
```

Import `crate::target::Target`. Leave `connect_addr` in place: `pheme pair` still uses it and Task 9 replaces its client use.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib target`
Expected: PASS, 9 tests.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-app/src/target.rs crates/pheme-app/src/lib.rs crates/pheme-app/src/config.rs
git commit -F - <<'MSG'
Update: classify the connect string into a resolvable target

Parsing is separated from resolving so that every rule about which
string means what can be tested without a network. A socket address is
used as written; anything with a colon or a dot goes to the system
resolver, which is exactly what happened before mDNS existed; a bare
label is an mDNS instance name.

An empty or whitespace-only target is an error rather than an mDNS
browse for the empty name, which would wait out its timeout on every
reconnect attempt and never find anything.

An mDNS name that nothing answers falls back to the system resolver
before failing, so a bare hostname the local resolver knows keeps
working.

Target::resolve is async and meant to be called on every attempt rather
than once at startup. That is what will make a server that changed
address reachable again without restarting the client.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 9: Resolve the server on every reconnect attempt

**Files:**
- Modify: `crates/pheme-app/src/client.rs` (`ClientDeps.server_addr` line 26, the reconnect loop around line 143, `client::main` line 367)
- Modify: `crates/pheme-app/tests/integration.rs`, `audio.rs`, `mic_e2e.rs` (the `ClientDeps` literals)

**Interfaces:**
- Consumes: `Target` (Task 8).
- Produces: `ClientDeps.target: Target` replacing `ClientDeps.server_addr: SocketAddr`.

- [ ] **Step 1: Change the field**

In `ClientDeps`, replace `pub server_addr: SocketAddr,` with:

```rust
    /// Where the server is, as a question rather than an answer.
    ///
    /// Resolved on every connection attempt, not once at startup: a server that
    /// took a new DHCP lease or restarted on another port is then reachable
    /// again within one backoff interval instead of needing the client
    /// restarted. §4.3.
    pub target: Target,
```

- [ ] **Step 2: Resolve inside the loop**

In `run_client`, replace the destructured `server_addr` with `target`, and inside the reconnect loop, before connecting:

```rust
        let server_addr = match target.resolve().await {
            Ok(a) => a,
            Err(e) => {
                debug!("could not resolve {target:?}: {e}");
                backoff.sleep(&mut shutdown).await;
                continue;
            }
        };
```

Use whatever the existing loop already calls to wait out a failed attempt; do not introduce a second backoff mechanism. If the existing code sleeps inline rather than through a helper, match that shape exactly.

- [ ] **Step 3: Update the entry point**

In `client::main`, replace `let server_addr = cfg.connect_addr(host)?;` with:

```rust
    let target = cfg.connect_target(host)?;
```

and log `target = ?target` in place of `server = %server_addr` in the startup `info!`.

- [ ] **Step 4: Let `pheme pair` take an mDNS name too**

Spec §4.4 requires `pheme pair <host>` to accept an instance name wherever it accepts an address. It currently calls `cfg.connect_addr(Some(host))`, which sends a bare label straight to the system resolver and fails. In `client::pair`, replace that line with:

```rust
    let addr = cfg.connect_target(Some(host))?.resolve().await?;
```

Nothing else in `pair` changes: it already prints the address it used, which is now the resolved one.

- [ ] **Step 5: Update the tests**

In the three test files, replace `server_addr: addr,` with `target: Target::Fixed(addr),` and import `pheme_app::target::Target`. Change nothing else.

- [ ] **Step 6: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green. `integration.rs` already contains a test that restarts the server and expects the client to reconnect; it must still pass, which is what proves the new resolution step did not break the existing path.

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-app
git commit -F - <<'MSG'
Update: resolve the server on every reconnect attempt

The client held a SocketAddr resolved once at startup, so a server that
changed address was unreachable until the client itself was restarted.
It now holds a Target and asks it again before every attempt.

This is the reason discovery is worth building at all. A DHCP lease
change or a server restarting on another port now heals itself within
one backoff interval, and the existing test that restarts a server and
expects the client to come back still passes, which is what shows the
extra step cost the working path nothing.

A target that cannot be resolved waits out the same backoff as a
connection that failed, rather than being treated as fatal: the server
may simply not be up yet.

pheme pair takes the same path, so pairing accepts an mDNS name wherever
it accepted an address. Without this, the first thing a new user is told
to type would be the one command that could not use a discovered name.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 10: Advertise, `pheme discover`, and the config flag

**Files:**
- Modify: `crates/pheme-app/src/config.rs` (`Config`, `Default`)
- Modify: `crates/pheme-app/src/server.rs` (`server::main`)
- Modify: `crates/pheme-app/src/main.rs` (the `Discover` subcommand)
- Modify: `crates/pheme-app/src/setup.rs` (the firewall note)

**Interfaces:**
- Consumes: `pheme_net::discovery::{advertise, browse}` (Task 7).
- Produces: `Config.discovery: bool` (default `true`); the `pheme discover` subcommand.

- [ ] **Step 1: Add the config flag**

In `Config`:

```rust
    /// Whether the server publishes itself on the local network. Clients are
    /// unaffected: they look up whatever `connect` names regardless.
    #[serde(default = "default_discovery")]
    pub discovery: bool,
```

with

```rust
fn default_discovery() -> bool {
    true
}
```

and `discovery: true` in the `Default` impl.

- [ ] **Step 2: Write the failing test**

In `crates/pheme-app/src/config.rs`'s test module:

```rust
    #[test]
    fn discovery_is_on_unless_it_is_turned_off() {
        let c: Config = toml::from_str("role = \"server\"").unwrap();
        assert!(c.discovery);
        let c: Config = toml::from_str("role = \"server\"\ndiscovery = false").unwrap();
        assert!(!c.discovery);
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p pheme-app --lib config`
Expected: FAIL — unknown field `discovery` (the struct has `deny_unknown_fields`).

- [ ] **Step 4: Advertise from the server**

In `server::main`, after the identity and endpoint are built and before the accept loop:

```rust
    // Held for the life of the server: dropping it unregisters the service.
    let _advert = if cfg.discovery {
        match pheme_net::discovery::advertise(&cfg.name, cfg.listen.port(), &identity.fingerprint) {
            Ok(a) => {
                info!(name = %cfg.name, "advertising on the local network");
                Some(a)
            }
            Err(e) => {
                // Not fatal. A client with an address in its config does not
                // need discovery at all.
                warn!("could not advertise on the local network: {e}");
                None
            }
        }
    } else {
        None
    };
```

- [ ] **Step 5: Add the subcommand**

In `crates/pheme-app/src/main.rs`, add to `enum Cmd`:

```rust
    /// List the Pheme servers advertising on this network
    Discover {
        /// How many seconds to listen
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
```

and to the `match cli.cmd`:

```rust
        Cmd::Discover { timeout } => {
            let found =
                pheme_net::discovery::browse(std::time::Duration::from_secs(timeout)).await?;
            if found.is_empty() {
                println!("No Pheme servers found. If one is running, check that UDP port 5353 is not blocked.");
                return Ok(());
            }
            println!("{:<24} {:<22} FINGERPRINT", "NAME", "ADDRESS");
            for f in &found {
                println!(
                    "{:<24} {:<22} {}",
                    f.name,
                    f.addr.to_string(),
                    f.fingerprint.as_deref().unwrap_or("-")
                );
            }
            // Two servers with one name make `connect` ambiguous: whichever
            // answers first wins, and that is not the user's choice.
            for f in &found {
                if found.iter().filter(|o| o.name == f.name).count() > 1 {
                    println!(
                        "\nWarning: more than one server is called {:?}. \
                         `connect = {:?}` will reach whichever answers first; \
                         give them different names, or use an address.",
                        f.name, f.name
                    );
                    break;
                }
            }
            Ok(())
        }
```

- [ ] **Step 6: Mention the firewall in `setup`**

In `crates/pheme-app/src/setup.rs`, add one line to what it prints, on both platforms:

```rust
    println!(
        "Discovery uses mDNS on UDP port 5353. If `pheme discover` finds nothing, \
         allow that port through the firewall, or put the server's address in `connect`."
    );
```

Match the surrounding function's existing output style; if it writes through `tracing` rather than `println!`, follow that instead.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib config`
Expected: PASS.

- [ ] **Step 8: Verify the subcommand by hand**

Run: `cargo run -p pheme-app --bin pheme -- discover --timeout 2`
Expected: the table, or the "No Pheme servers found" line. Neither is a failure; the point is that it runs and exits.

- [ ] **Step 9: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 10: Commit**

```bash
git add crates/pheme-app
git commit -F - <<'MSG'
Update: advertise the server and add pheme discover

The server publishes itself on the local network unless discovery is
set to false, and holds the advertisement for its whole life so that
exiting unregisters it. Failing to advertise is a warning, not a fatal
error: a client with an address in its configuration never needed
discovery.

pheme discover prints name, address and fingerprint for everything it
sees, and warns when two servers share a name, because in that case
connect reaches whichever answers first and that is not the user's
choice. An empty result says to check UDP 5353 rather than leaving the
user with a blank screen, since a default Windows firewall is the most
likely reason.

pheme setup now mentions the same port, so the fix is in the place
people look when discovery finds nothing.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## Task 11: Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/testing.md`
- Modify: `docs/superpowers/specs/2026-09-21-pheme-architecture-design.md` (the crate table, line 73)

**Interfaces:** none.

- [ ] **Step 1: README**

Add a "Clipboard" section stating, in this order: it is text only; it crosses when the pointer crosses, not when you copy; it does not work on GNOME Wayland, because Mutter implements no data-control protocol, and everything else still works there; content over 1 MiB stays local.

Add a "Finding the server" section: the server advertises itself, `pheme discover` lists what is on the network, `connect` takes a name or an address, and the name is looked up again on every reconnect. Mention UDP 5353 and the firewall.

Update the limitations list to match.

- [ ] **Step 2: `docs/testing.md`**

Add the rows exactly as §8 of the spec lists them: C1 through C7, then D1 through D4. Match the table format the existing W-rows use.

- [ ] **Step 3: Amend the architecture document**

Line 73 of `docs/superpowers/specs/2026-09-21-pheme-architecture-design.md` lists "clipboard sync" among `pheme-core`'s responsibilities. Remove those two words from that line and leave the rest untouched. The sub-project 5 spec §3.6 records why.

- [ ] **Step 4: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add README.md docs
git commit -F - <<'MSG'
Update: document the clipboard and discovery

The README says what the clipboard does and, more usefully, when: it
crosses with the pointer rather than when you copy, so copying without
switching machines leaves the other side with what it had. It also says
plainly that GNOME Wayland has no clipboard, because Mutter implements
no data-control protocol and a person who hits that deserves the reason
rather than silence.

docs/testing.md gains C1 to C7 and D1 to D4. Everything about the real
clipboard backends and the real mDNS round trip lives there: no CI
runner has a display, a compositor or reliable multicast, so the manual
matrix is the only place those are actually tested.

The architecture document no longer lists clipboard sync among
pheme-core's responsibilities. It needs no state machine, and the core
is better for staying free of I/O policy.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```
