# Wayland Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a machine running a Wayland session act as a Pheme server, by adding a capture backend built on the `org.freedesktop.portal.InputCapture` portal and libei.

**Architecture:** The portal inverts the X11 model — the application declares pointer barriers up front and receives nothing until the compositor says a barrier was crossed. A new `linux_portal` backend translates that model into the `CaptureEvent` stream `ServerCore` already consumes, which needs three small changes to the shared trait and core: `Action::Ungrab` absorbs the `WarpCursor` that always followed it, the trait gains `set_edges` so barriers can be placed where clients actually are, and `ServerCore` gains `release_remote` for a capture the compositor ends on its own.

**Tech Stack:** Rust 2021, `ashpd` 0.13 (async-io, input_capture, global_shortcuts), `reis` 0.7 (async-io), `futures-lite` 2, `async-channel` 2.

**Spec:** `docs/superpowers/specs/2026-09-24-wayland-capture-design.md`

## Global Constraints

- **Everything in this repository is English** — code, comments, commit messages, documentation. (Vietnamese is for conversation only.)
- **Commit format** is `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`. Title under 72 characters, imperative. Blank line, then a body wrapped at 72 columns explaining what changed and why. Trailer, exactly:
  `Co-Authored-By: Claude <noreply@anthropic.com>`
- **Workspace MSRV becomes 1.87** (`ashpd` requires it). Set in Task 4.
- **Gate before every commit:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. All three green.
- **Windows cross-compilation cannot be checked locally** — `ring` (via quinn/rustls) needs `x86_64-w64-mingw32-gcc`, which is not installed and needs root. For tasks that touch Windows code, the local gate is:
  `RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone PATH=$HOME/.cargo-standalone/bin:$PATH cargo clippy -p pheme-audio -p pheme-input --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
  CI is the real gate.
- **No new dependency beyond the four named above.** All four are already in `ashpd`'s own dependency graph, so declaring them adds nothing to the build. In particular, do not add `xkbcommon`; §6 of the spec explains why the modifier mask uses a fixed layout.
- **The portal is never mocked.** Logic that can be a pure function must be one, and must be unit-tested. Session lifecycle is verified by the manual matrix and by the ignored live test in Task 12.
- **Every constant taken from the spec's §3 is a measurement, not a guess.** Do not "simplify" them. `x0 + W` for a far edge, evdev key codes with no `- 8`, and the `flush()` after binding capabilities are each load-bearing, and each fails silently when wrong.

## File Structure

| File | Responsibility |
|---|---|
| `crates/pheme-core/src/server.rs` | `Action::Ungrab { x, y }`, `release_remote()`, `capture_edges()` |
| `crates/pheme-input/src/lib.rs` | `CaptureEdge`, `InputCapture::release`, `InputCapture::set_edges`, `detect_capture()` |
| `crates/pheme-input/src/linux_x11.rs` | `release()`; stops rejecting Wayland itself |
| `crates/pheme-input/src/windows/capture.rs` | `release()` |
| `crates/pheme-input/src/mock.rs` | `release()`, `set_edges()` recording, handle accessors |
| `crates/pheme-input/src/portal/mod.rs` | Backend struct, trait impl, thread handle |
| `crates/pheme-input/src/portal/geometry.rs` | Zones and edges to portal barriers (pure) |
| `crates/pheme-input/src/portal/translate.rs` | libei events to `CaptureEvent`, modifier mask, held-key tracking (pure) |
| `crates/pheme-input/src/portal/session.rs` | The portal session thread: D-Bus, libei, lifecycle |
| `crates/pheme-input/src/portal/shortcuts.rs` | Lock hotkey via GlobalShortcuts |
| `crates/pheme-input/tests/portal_live.rs` | `#[ignore]`d test against the real portal |
| `crates/pheme-app/src/server.rs` | New action arm, `set_edges` on client connect/disconnect |
| `README.md`, `docs/testing.md` | Wayland requirements, rows W1–W11 |

The portal backend is a directory rather than one file: the pure parts (`geometry`, `translate`) hold every defect the design probe uncovered, and they must be readable and testable without the D-Bus machinery next to them.

---

### Task 1: `Action::Ungrab` carries the position

**Files:**
- Modify: `crates/pheme-core/src/server.rs`
- Modify: `crates/pheme-input/src/lib.rs`
- Modify: `crates/pheme-input/src/linux_x11.rs`
- Modify: `crates/pheme-input/src/windows/capture.rs`
- Modify: `crates/pheme-input/src/mock.rs`
- Modify: `crates/pheme-app/src/server.rs`

**Interfaces:**
- Produces: `Action::Ungrab { x: i32, y: i32 }` replacing the unit variant; `InputCapture::release(&mut self, x: i32, y: i32) -> Result<()>`.
- Consumes: nothing from earlier tasks.

This is a behaviour-preserving refactor. `ServerCore` already emits `Action::Ungrab` immediately followed by `Action::WarpCursor { x, y }` on both paths that produce it, always with the same coordinates. Merging them removes an ordering contract that the portal backend would otherwise have to depend on silently.

**The trap:** `pheme-app`'s `execute` documents that *"A failed `Ungrab` is logged and the list continues (the pointer is still warped back)"*. A natural `release` implementation — `self.set_mode(Observe)?; self.warp_cursor(x, y)` — changes that: a failed ungrab now also skips the warp, stranding the pointer. The X11 and Windows implementations must attempt both and report the first error.

- [ ] **Step 1: Write the failing test**

In `crates/pheme-core/src/server.rs`, in the `tests` module:

```rust
#[test]
fn leaving_a_client_ungrabs_at_the_reentry_point() {
    let mut core = two_screen_core();
    core.client_connected("right", client_screens());
    // Cross out to the right, then walk back past the client's left edge.
    core.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
    let actions = core.on_event(CaptureEvent::MotionRel { dx: -5000, dy: 0 });
    let ungrab = actions
        .iter()
        .find_map(|a| match a {
            Action::Ungrab { x, y } => Some((*x, *y)),
            _ => None,
        })
        .expect("leaving a client must ungrab");
    // The ungrab carries the re-entry point itself, not a separate WarpCursor.
    assert_eq!(ungrab, (1918, 540));
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::WarpCursor { .. })),
        "the position travels on Ungrab now: {actions:?}"
    );
}
```

`two_screen_core` and `client_screens` are the existing test helpers in that module; if they are named differently, use the ones the neighbouring tests use and keep the same 1920x1080 geometry, so `1918, 540` stays correct.

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p pheme-core leaving_a_client_ungrabs_at_the_reentry_point`
Expected: FAIL to compile — `Action::Ungrab` is a unit variant and has no fields.

- [ ] **Step 3: Change the enum**

In `crates/pheme-core/src/server.rs`:

```rust
pub enum Action {
    SendControl(Msg),
    SendDatagram(Msg),
    Grab,
    /// Stop capturing and put the pointer at (x, y). The position is part of the
    /// action because it always was: every `Ungrab` was followed by a `WarpCursor`
    /// with these exact coordinates, and a backend that releases by naming a
    /// position (the InputCapture portal) cannot depend on that pairing silently.
    Ungrab { x: i32, y: i32 },
    WarpCursor { x: i32, y: i32 },
    SetLocked(bool),
}
```

- [ ] **Step 4: Update the two producers**

In `client_disconnected` (around line 118):

```rust
            Some(r) if r.name == name => {
                self.remote = None;
                let (x, y) = self.server.center();
                self.last_pos = Some((x, y));
                vec![Action::Ungrab { x, y }]
            }
```

In `on_remote_event`'s leaving branch (around line 244), replace the two pushes:

```rust
                    actions.insert(0, Action::SendControl(Msg::Leave { seq }));
                    actions.truncate(1);
                    actions.push(Action::Ungrab { x, y });
```

- [ ] **Step 5: Update the three assertions in this file**

Around line 408: `assert!(matches!(a[1], Action::Ungrab));` becomes
`assert!(matches!(a[1], Action::Ungrab { .. }));` — check what index the action now sits at, because the list is one element shorter.

Around line 506: `[Action::Ungrab, Action::WarpCursor { x: 960, y: 540 }]` becomes
`[Action::Ungrab { x: 960, y: 540 }]`.

Around line 577 in the proptest: `Action::Ungrab => { prop_assert!(grabbed); grabbed = false; }` becomes
`Action::Ungrab { .. } => { prop_assert!(grabbed); grabbed = false; }`.

- [ ] **Step 6: Run the core tests**

Run: `cargo test -p pheme-core`
Expected: PASS.

- [ ] **Step 7: Add `release` to the trait**

In `crates/pheme-input/src/lib.rs`, inside `pub trait InputCapture`:

```rust
    /// Stops capturing and places the pointer at (x, y).
    ///
    /// **Synchronous**, under the same contract as `set_mode`: returns only once the
    /// backend has applied the change or failed to, with a 1 s internal timeout mapped
    /// to `Error::Backend("mode change timed out")`.
    ///
    /// A backend that implements this as an ungrab followed by a warp must attempt the
    /// warp **even when the ungrab fails**, and report the first error. The caller logs
    /// the error and continues, and a pointer left outside the screen because an ungrab
    /// failed is worse than a pointer that came back under a stale grab.
    fn release(&mut self, x: i32, y: i32) -> Result<()>;
```

- [ ] **Step 8: Write the failing backend test**

In `crates/pheme-input/src/mock.rs`, in its `tests` module:

```rust
#[test]
fn release_observes_and_warps() {
    let (mut cap, handle) = MockCapture::new(default_screens());
    cap.set_mode(CaptureMode::Grab).unwrap();
    cap.release(7, 8).unwrap();
    assert_eq!(handle.mode(), CaptureMode::Observe);
    assert_eq!(
        handle.warps_with_mode(),
        vec![((7, 8), CaptureMode::Observe)],
        "the warp must be recorded after the mode drops, or a real backend would \
         still be clipping the pointer when it warps"
    );
}
```

Match `MockCapture::new`'s real signature and the existing helper for screens.

- [ ] **Step 9: Run it to make sure it fails**

Run: `cargo test -p pheme-input release_observes_and_warps`
Expected: FAIL — `release` is not implemented for `MockCapture`.

- [ ] **Step 10: Implement `release` on the three backends**

`crates/pheme-input/src/mock.rs`:

```rust
    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        let mode = self.set_mode(CaptureMode::Observe);
        let warp = self.warp_cursor(x, y);
        mode.and(warp)
    }
```

`crates/pheme-input/src/linux_x11.rs`:

```rust
    /// Ungrab, then warp. Both are attempted even if the first fails: the previous
    /// `Ungrab` + `WarpCursor` pair kept going after a failed ungrab, and dropping the
    /// warp would leave the pointer wherever the grab had parked it.
    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        let ungrab = self.set_mode(CaptureMode::Observe);
        let warp = self.warp_cursor(x, y);
        ungrab.and(warp)
    }
```

`crates/pheme-input/src/windows/capture.rs`: identical body to the X11 one.

- [ ] **Step 11: Write the test that pins the failure behaviour**

Still in `crates/pheme-input/src/mock.rs`. `MockCapture` has `fail_next_grab`; add the same for the ungrab path so this is testable. In `MockState` add `fail_next_ungrab: bool`, on the handle add `pub fn fail_next_ungrab(&self)` mirroring the existing `fail_next_grab` setter, and in `set_mode` return an error for `CaptureMode::Observe` when it is set.

```rust
#[test]
fn a_failed_ungrab_still_warps() {
    let (mut cap, handle) = MockCapture::new(default_screens());
    cap.set_mode(CaptureMode::Grab).unwrap();
    handle.fail_next_ungrab();
    let r = cap.release(7, 8);
    assert!(r.is_err(), "the error must still be reported");
    assert_eq!(
        handle.warps(),
        vec![(7, 8)],
        "dropping the warp when the ungrab fails strands the pointer off-screen"
    );
}
```

- [ ] **Step 12: Run both mock tests**

Run: `cargo test -p pheme-input release_observes_and_warps a_failed_ungrab_still_warps`
Expected: PASS.

- [ ] **Step 13: Update the app**

In `crates/pheme-app/src/server.rs`, replace the `Ungrab` arm and keep `WarpCursor` (still used by `abort_switch`):

```rust
                Action::Ungrab { x, y } => self.capture_call(|c| c.release(x, y)),
                Action::WarpCursor { x, y } => self.capture_call(|c| c.warp_cursor(x, y)),
```

Update the `execute` doc comment: the sentence about a failed `Ungrab` now belongs to `release`, whose contract guarantees the warp is attempted regardless.

- [ ] **Step 14: Run the full gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green. The integration test asserting `warps_with_mode().last() == Some(((1918, 540), CaptureMode::Observe))` must still pass unchanged — if it does not, `release` is recording the warp before dropping the mode.

- [ ] **Step 15: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Update: fold the ungrab position into the Ungrab action

Action::Ungrab was always emitted immediately before a WarpCursor with
the same coordinates, on both paths that produce it. The InputCapture
portal releases a capture by naming a cursor position rather than by
warping, so a portal backend would have had to reconstruct the pair by
remembering that an Ungrab is always followed by a WarpCursor -- an
ordering contract no test would notice being broken.

Ungrab now carries the position and the trait gains release(x, y).

The X11, Windows and mock implementations attempt the warp even when
the ungrab fails, preserving what pheme-app documented: a failed ungrab
is logged and the pointer still comes back. Writing release as
`set_mode(Observe)?; warp_cursor(..)` would have silently dropped the
warp and stranded the pointer off-screen.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 2: `ServerCore::release_remote()`

**Files:**
- Modify: `crates/pheme-core/src/server.rs`

**Interfaces:**
- Consumes: `Action::Ungrab { x, y }` from Task 1.
- Produces: `ServerCore::release_remote(&mut self) -> Vec<Action>`.

The InputCapture portal lets the compositor end a capture on its own — KDE binds an escape combination for it. When that happens `ServerCore` still believes it is `Remote` and the client still believes it holds the input, so any key held at that moment stays held on the client forever.

This differs from `client_disconnected`, which sends no `Msg::Leave` because the client is gone. Here the client is still connected and must be told.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn release_remote_tells_the_client_to_let_go() {
    let mut core = two_screen_core();
    core.client_connected("right", client_screens());
    core.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
    assert_eq!(core.active(), Active::Remote("right".into()));

    let actions = core.release_remote();

    assert!(
        matches!(actions.first(), Some(Action::SendControl(Msg::Leave { .. }))),
        "without Leave the client keeps whatever it is holding: {actions:?}"
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Ungrab { .. })),
        "{actions:?}"
    );
    assert_eq!(core.active(), Active::Local);
}

#[test]
fn release_remote_is_a_no_op_when_already_local() {
    let mut core = two_screen_core();
    assert!(core.release_remote().is_empty());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pheme-core release_remote`
Expected: FAIL — no method `release_remote`.

- [ ] **Step 3: Implement**

```rust
    /// Returns to Local because the capture ended without the pointer leaving the
    /// client — the compositor ended it on its own, which the InputCapture portal
    /// permits at any time.
    ///
    /// Unlike `client_disconnected` this sends `Msg::Leave`: the client is still
    /// connected, and without it every key held at that instant stays held there.
    /// Unlike `abort_switch` the switch did happen, so `Enter` was already sent.
    pub fn release_remote(&mut self) -> Vec<Action> {
        if self.remote.take().is_none() {
            return Vec::new();
        }
        let (x, y) = self.server.center();
        self.last_pos = Some((x, y));
        let seq = self.next_seq();
        debug!("capture ended by the compositor; back to local");
        vec![
            Action::SendControl(Msg::Leave { seq }),
            Action::Ungrab { x, y },
        ]
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p pheme-core release_remote`
Expected: PASS.

- [ ] **Step 5: Add the event that reaches it**

`release_remote` needs a way to be called. The portal backend cannot call it — it
produces `CaptureEvent`s and knows nothing about `ServerCore`. Add the variant here,
in the same task as the method it triggers, so no later task has to reach across
crates unannounced.

In `crates/pheme-core/src/server.rs`:

```rust
pub enum CaptureEvent {
    MotionAbs { x: i32, y: i32 },
    MotionRel { dx: i32, dy: i32 },
    Button { btn: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
    /// The backend stopped capturing without the pointer leaving the client — the
    /// compositor ended it. Only the Wayland backend produces this; the X11 and
    /// Windows backends keep capturing until they are told to stop.
    CaptureEnded,
}
```

and route it at the top of `on_event`, before the key-tracking block, because it is
not a key and must work whether or not a remote is active:

```rust
    pub fn on_event(&mut self, ev: CaptureEvent) -> Vec<Action> {
        if let CaptureEvent::CaptureEnded = ev {
            return self.release_remote();
        }
        // … existing body unchanged …
```

- [ ] **Step 6: Test the routing**

```rust
#[test]
fn capture_ended_returns_to_local_with_a_leave() {
    let mut core = two_screen_core();
    core.client_connected("right", client_screens());
    core.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
    let actions = core.on_event(CaptureEvent::CaptureEnded);
    assert!(
        matches!(actions.first(), Some(Action::SendControl(Msg::Leave { .. }))),
        "{actions:?}"
    );
    assert_eq!(core.active(), Active::Local);
}

#[test]
fn capture_ended_while_local_does_nothing() {
    let mut core = two_screen_core();
    assert!(core.on_event(CaptureEvent::CaptureEnded).is_empty());
}
```

Run: `cargo test -p pheme-core capture_ended`
Expected: PASS. Adding the variant makes every `match` on `CaptureEvent` fail to
compile until it is handled — follow the compiler through `pheme-app` and the mock.

- [ ] **Step 7: Write the test that catches a stuck modifier**

The whole point of `Leave` is that the client releases what it holds. Pin that the held set on the server is also consistent afterwards:

```rust
#[test]
fn a_key_held_across_release_remote_does_not_leak_into_the_next_enter() {
    let mut core = two_screen_core();
    core.client_connected("right", client_screens());
    core.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
    core.on_event(CaptureEvent::Key { code: KeyCode::LEFT_SHIFT, down: true });
    core.release_remote();
    // The key-up arrives while local, as it would from any backend that still
    // observes the keyboard.
    core.on_event(CaptureEvent::Key { code: KeyCode::LEFT_SHIFT, down: false });

    core.on_event(CaptureEvent::MotionAbs { x: 960, y: 540 });
    let actions = core.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
    let mods = actions.iter().find_map(|a| match a {
        Action::SendControl(Msg::Enter { mods, .. }) => Some(*mods),
        _ => None,
    });
    assert_eq!(mods, Some(Modifiers::default()), "{actions:?}");
}
```

This test passes with the implementation above; it exists because the portal backend in Task 7 has to do extra work to make the same statement true when the key-up is *not* observed.

- [ ] **Step 8: Full gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: add ServerCore::release_remote for compositor-ended capture

The InputCapture portal lets the compositor end a capture at any time,
and KDE binds an escape combination that does exactly that. The core
would still believe it was Remote and the client would still believe it
held the input, so any key held at that instant would stay held on the
client with nothing left to release it.

release_remote sends Msg::Leave and ungrabs to the server centre. It
differs from client_disconnected, which sends no Leave because there is
nobody left to receive one, and from abort_switch, which runs before
Enter was ever sent.

CaptureEvent::CaptureEnded is how a backend reaches it, and it lands in
the same commit as the method it triggers rather than appearing later
from a task that has no other business in this crate.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 3: `CaptureEdge` and `set_edges`

**Files:**
- Modify: `crates/pheme-core/src/server.rs`
- Modify: `crates/pheme-input/src/lib.rs`
- Modify: `crates/pheme-input/src/mock.rs`
- Modify: `crates/pheme-app/src/server.rs`

**Interfaces:**
- Consumes: `Side` (re-exported from `pheme_core`).
- Produces: `pheme_input::CaptureEdge { side: Side, span: (f32, f32) }`; `InputCapture::set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()>` with a no-op default; `ServerCore::capture_edges(&self) -> Vec<(Side, (f32, f32))>`.

`pheme-input` has never needed to know where clients are, because the X11 backend detects crossings by watching the pointer. The portal needs barriers declared up front, so the information has to travel.

It is named `CaptureEdge`, not `Barrier`, because the portal backend imports `ashpd::desktop::input_capture::Barrier` in the same file.

- [ ] **Step 1: Write the failing core test**

In `crates/pheme-core/src/server.rs` tests:

```rust
#[test]
fn capture_edges_covers_only_connected_clients() {
    let layout = Layout {
        server_screens: server_screens(),
        clients: vec![
            ClientPlacement { name: "right".into(), side: Side::Right, span: (0.0, 1.0) },
            ClientPlacement { name: "left".into(), side: Side::Left, span: (0.25, 0.75) },
        ],
    };
    let mut core = ServerCore::new(layout, Hotkeys::default());
    assert!(
        core.capture_edges().is_empty(),
        "an edge with nobody behind it would stop the pointer for nothing"
    );

    core.client_connected("left", client_screens());
    assert_eq!(core.capture_edges(), vec![(Side::Left, (0.25, 0.75))]);

    core.client_connected("right", client_screens());
    assert_eq!(
        core.capture_edges(),
        vec![(Side::Right, (0.0, 1.0)), (Side::Left, (0.25, 0.75))],
        "order follows the layout, not the connection order"
    );

    core.client_disconnected("left");
    assert_eq!(core.capture_edges(), vec![(Side::Right, (0.0, 1.0))]);
}
```

Use the existing helpers for `server_screens()` / `client_screens()`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pheme-core capture_edges_covers_only_connected_clients`
Expected: FAIL — no method `capture_edges`.

- [ ] **Step 3: Implement it**

```rust
    /// The edges a crossing may start a capture on: one per placement whose client is
    /// currently connected.
    ///
    /// Edges without a connected client are deliberately excluded. A backend that
    /// declares them (the InputCapture portal) would have the compositor stop the
    /// pointer at that edge, and the core would then decline the switch — so the
    /// pointer would snag on a screen edge that leads nowhere.
    pub fn capture_edges(&self) -> Vec<(Side, (f32, f32))> {
        self.layout
            .clients
            .iter()
            .filter(|p| self.connected.contains_key(&p.name))
            .map(|p| (p.side, p.span))
            .collect()
    }
```

- [ ] **Step 4: Run it**

Run: `cargo test -p pheme-core capture_edges_covers_only_connected_clients`
Expected: PASS.

- [ ] **Step 5: Add the type and trait method**

In `crates/pheme-input/src/lib.rs`:

```rust
/// An edge on which a crossing should start a capture.
///
/// Named `CaptureEdge` rather than `Barrier` because the portal backend imports
/// ashpd's `Barrier` in the same file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureEdge {
    pub side: Side,
    /// Fractions of the edge's length, as in `ClientPlacement::span`.
    pub span: (f32, f32),
}
```

with `use pheme_core::Side;` added to the imports, and inside the trait:

```rust
    /// Declares the edges on which a crossing should start a capture.
    ///
    /// Backends that detect crossings by watching the pointer (X11, Windows) learn
    /// nothing from this and ignore it. The InputCapture portal cannot work without
    /// it: the compositor watches the barriers, and an edge that was never declared
    /// never produces an event.
    ///
    /// Called whenever the set of connected clients changes.
    fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        let _ = edges;
        Ok(())
    }
```

- [ ] **Step 6: Record the calls in the mock**

In `CaptureState` add `edges: Vec<Vec<CaptureEdge>>` — a log of every call, not just the last, so a test can tell "set once" from "set repeatedly". On `MockCaptureHandle`:

```rust
    /// Every `set_edges` call in order.
    pub fn edge_calls(&self) -> Vec<Vec<CaptureEdge>> {
        self.state.lock().unwrap().edges.clone()
    }
```

and on `MockCapture`:

```rust
    fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        self.state.lock().unwrap().edges.push(edges.to_vec());
        Ok(())
    }
```

- [ ] **Step 7: Write the failing app test**

In `crates/pheme-app/tests/integration.rs`, alongside the existing tests that use `MockCaptureHandle`:

```rust
#[tokio::test]
async fn the_capture_backend_learns_which_edges_have_clients() {
    let h = harness().await;
    wait_connected(&h.cap).await;

    let calls = h.cap.edge_calls();
    let last = calls.last().expect("connecting a client must declare its edge");
    assert_eq!(
        last.len(),
        1,
        "exactly the connected client's edge, nothing else: {calls:?}"
    );
    assert_eq!(last[0].side, Side::Right);

    h.drop_client_connection();
    wait_until(|| h.cap.edge_calls().last().is_some_and(|c| c.is_empty()), Duration::from_secs(5))
        .await;
}
```

Adapt to the harness helpers that already exist in that file (`harness`, `wait_connected`, `wait_until`, and whatever the file uses to drop a client connection). Keep the shape: assert the edge set after connect, and that it empties after disconnect.

- [ ] **Step 8: Run to verify it fails**

Run: `cargo test -p pheme-app the_capture_backend_learns_which_edges_have_clients`
Expected: FAIL — `edge_calls()` is empty; nothing calls `set_edges` yet.

- [ ] **Step 9: Wire the app**

In `crates/pheme-app/src/server.rs`, add to `Shared`:

```rust
    /// Pushes the current edge set to the capture backend. Called whenever the set of
    /// connected clients changes: barriers are declared only for edges that lead
    /// somewhere (see `ServerCore::capture_edges`).
    fn publish_edges(&self) {
        let edges: Vec<CaptureEdge> = self
            .core
            .lock()
            .unwrap()
            .capture_edges()
            .into_iter()
            .map(|(side, span)| CaptureEdge { side, span })
            .collect();
        self.capture_call(|c| c.set_edges(&edges));
    }
```

Call it immediately after both `client_connected` and `client_disconnected`, after `execute`:

```rust
    let actions = shared.core.lock().unwrap().client_connected(&name, screens);
    shared.execute(actions);
    shared.publish_edges();
```

```rust
    let actions = shared.core.lock().unwrap().client_disconnected(&name);
    shared.execute(actions);
    shared.publish_edges();
```

Take the core lock only inside `publish_edges`, never while one is already held — `execute` takes it too.

- [ ] **Step 10: Run the test and the gate**

Run: `cargo test -p pheme-app the_capture_backend_learns_which_edges_have_clients`
Expected: PASS.

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Update: tell the capture backend which edges have clients

The X11 backend finds edge crossings by watching the pointer, so
pheme-input never needed to know where clients sit. The InputCapture
portal inverts that: the compositor watches barriers the application
declares in advance, and an edge that was never declared produces no
event at all.

InputCapture gains set_edges with a no-op default, so X11, Windows and
mock are unaffected, and ServerCore gains capture_edges. The server
publishes the edge set whenever a client connects or disconnects.

Only edges with a connected client are declared. Declaring the rest
would have the compositor stop the pointer at an edge the core then
refuses to cross, so the pointer would snag on edges that lead nowhere.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 4: Dependencies, MSRV, and barrier geometry

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/pheme-input/Cargo.toml`
- Create: `crates/pheme-input/src/portal/mod.rs`
- Create: `crates/pheme-input/src/portal/geometry.rs`
- Modify: `crates/pheme-input/src/lib.rs`

**Interfaces:**
- Consumes: `CaptureEdge` from Task 3.
- Produces: `portal::geometry::{Zone, PortalBarrier, barriers}`.

This task is entirely pure functions plus a dependency change. It is where the spec's §3.1 measurement lands, and that measurement is unintuitive enough that it must be pinned by tests before any D-Bus code exists.

**The two rules, from the portal specification:**

1. A barrier lies on the **outside boundary of the union of all zones**. A barrier sits on the top (horizontal) or left (vertical) edge of its pixels, so the far edge of a zone of width `W` at `x0` is `x0 + W`, **not** `x0 + W - 1`. The extent *along* the edge stops at the last pixel, `x0 + W - 1`.
2. A barrier must be **fully contained within one zone**. A union edge that spans two monitors therefore needs one barrier per zone, not one barrier across both.

Rule 2 is invisible on a single-monitor machine — which is the only Wayland machine this project has — so the tests below carry it.

- [ ] **Step 1: Bump the MSRV and add the dependencies**

In the workspace `Cargo.toml`:

```toml
rust-version = "1.87"
```

and in `[workspace.dependencies]`:

```toml
ashpd = { version = "0.13", default-features = false, features = ["async-io", "input_capture", "global_shortcuts"] }
reis = { version = "0.7", features = ["async-io"] }
futures-lite = "2"
async-channel = "2"
```

`futures-lite` supplies `block_on` and the select combinators; `async-channel` carries commands into the backend thread's executor, because the thread must wait on portal signals, libei events and commands in one place. Both are already in `ashpd`'s graph.

`ashpd`'s default features pull tokio and every portal; the three named features are all that is used. In `crates/pheme-input/Cargo.toml`, under the existing Linux target block:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
evdev = "0.13"
x11rb = { version = "0.14", features = ["xinput", "xtest", "xfixes", "randr"] }
wayland-client = "0.31"
ashpd = { workspace = true }
reis = { workspace = true }
futures-lite = { workspace = true }
async-channel = { workspace = true }
```

- [ ] **Step 2: Verify the workspace still builds**

Run: `cargo build --workspace`
Expected: success. If the toolchain refuses the MSRV, stop — the installed toolchain is 1.98, so this should not happen.

- [ ] **Step 3: Write the failing geometry tests**

Create `crates/pheme-input/src/portal/geometry.rs` with only the tests and the type declarations:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::Side;

    fn one_screen() -> Vec<Zone> {
        vec![Zone { x: 0, y: 0, w: 2560, h: 1440 }]
    }

    /// Two 1920x1080 monitors side by side, as the portal specification's own example.
    fn two_screens() -> Vec<Zone> {
        vec![
            Zone { x: 0, y: 0, w: 1920, h: 1080 },
            Zone { x: 1920, y: 0, w: 1920, h: 1080 },
        ]
    }

    fn edge(side: Side) -> CaptureEdge {
        CaptureEdge { side, span: (0.0, 1.0) }
    }

    #[test]
    fn the_far_edge_is_the_width_not_the_last_pixel() {
        let b = barriers(&one_screen(), &[edge(Side::Right)]);
        assert_eq!(b.len(), 1);
        // Measured: a barrier at x = 2559 is rejected by the compositor, and the
        // rejection is reported only in failed_barriers -- Enable still succeeds and
        // the session then never activates.
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (2560, 0, 2560, 1439));
    }

    #[test]
    fn a_horizontal_edge_stops_at_the_last_pixel_along_it() {
        let b = barriers(&one_screen(), &[edge(Side::Bottom)]);
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (0, 1440, 2559, 1440));
    }

    #[test]
    fn the_near_edges_sit_at_the_origin() {
        let l = barriers(&one_screen(), &[edge(Side::Left)]);
        assert_eq!((l[0].x1, l[0].y1, l[0].x2, l[0].y2), (0, 0, 0, 1439));
        let t = barriers(&one_screen(), &[edge(Side::Top)]);
        assert_eq!((t[0].x1, t[0].y1, t[0].x2, t[0].y2), (0, 0, 2559, 0));
    }

    #[test]
    fn a_union_edge_spanning_two_zones_is_split_per_zone() {
        // The portal requires a barrier to be fully contained within one zone. The top
        // edge of this union crosses both monitors, so one barrier across it is
        // rejected. This is invisible on a single-monitor machine.
        let b = barriers(&two_screens(), &[edge(Side::Top)]);
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (0, 0, 1919, 0));
        assert_eq!((b[1].x1, b[1].y1, b[1].x2, b[1].y2), (1920, 0, 3839, 0));
    }

    #[test]
    fn only_zones_touching_that_side_of_the_union_get_a_barrier() {
        // The right edge of this union belongs to the right monitor alone.
        let b = barriers(&two_screens(), &[edge(Side::Right)]);
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (3840, 0, 3840, 1079));
    }

    #[test]
    fn a_span_narrows_the_barrier_along_the_edge() {
        let e = CaptureEdge { side: Side::Right, span: (0.25, 0.75) };
        let b = barriers(&one_screen(), &[e]);
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (2560, 360, 2560, 1079));
    }

    #[test]
    fn ids_are_unique_and_map_back_to_their_side() {
        let b = barriers(&two_screens(), &[edge(Side::Top), edge(Side::Right)]);
        let ids: std::collections::BTreeSet<u32> = b.iter().map(|b| b.id.get()).collect();
        assert_eq!(ids.len(), b.len(), "ids must be unique: {b:?}");
        assert!(b.iter().all(|b| b.id.get() != 0), "zero is not a valid barrier id");
        let top = b.iter().filter(|b| b.side == Side::Top).count();
        assert_eq!(top, 2);
    }

    #[test]
    fn no_edges_means_no_barriers() {
        assert!(barriers(&one_screen(), &[]).is_empty());
    }

    #[test]
    fn a_degenerate_span_produces_nothing_rather_than_an_invalid_barrier() {
        let e = CaptureEdge { side: Side::Right, span: (0.5, 0.5) };
        assert!(barriers(&one_screen(), &[e]).is_empty());
    }
}
```

- [ ] **Step 4: Run them to verify they fail**

Run: `cargo test -p pheme-input portal::geometry`
Expected: FAIL to compile — `Zone`, `PortalBarrier` and `barriers` do not exist.

- [ ] **Step 5: Implement**

Above the test module in the same file:

```rust
//! Turning the core's capture edges into portal pointer barriers.
//!
//! Two rules from the portal specification drive everything here, and both fail
//! silently when broken — the compositor reports a rejected barrier only in
//! `failed_barriers`, while `SetPointerBarriers` and `Enable` both still succeed:
//!
//! 1. A barrier sits on the top (horizontal) or left (vertical) edge of its pixels,
//!    so the far boundary of a zone of width `W` at `x0` is `x0 + W`, while the
//!    extent along the edge stops at the last pixel, `x0 + W - 1`.
//! 2. A barrier must lie on the outside boundary of the union of all zones **and**
//!    be fully contained within a single zone.

use std::num::NonZeroU32;

use pheme_core::Side;

use crate::CaptureEdge;

/// One of the compositor's input zones, as reported by `GetZones`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Zone {
    fn x1(&self) -> i32 {
        self.x + self.w as i32
    }
    fn y1(&self) -> i32 {
        self.y + self.h as i32
    }
}

/// A barrier ready to hand to `SetPointerBarriers`, with the side it came from so an
/// `Activated` signal can be traced back to a screen edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalBarrier {
    pub id: NonZeroU32,
    pub side: Side,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

fn union(zones: &[Zone]) -> Option<(i32, i32, i32, i32)> {
    let x0 = zones.iter().map(|z| z.x).min()?;
    let y0 = zones.iter().map(|z| z.y).min()?;
    let x1 = zones.iter().map(Zone::x1).max()?;
    let y1 = zones.iter().map(Zone::y1).max()?;
    Some((x0, y0, x1, y1))
}

/// Maps capture edges onto portal barriers, splitting each edge across the zones that
/// touch it. Returns barriers with ids starting at 1; the order is stable so tests can
/// name positions.
pub fn barriers(zones: &[Zone], edges: &[CaptureEdge]) -> Vec<PortalBarrier> {
    let Some((ux0, uy0, ux1, uy1)) = union(zones) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut next_id = 1u32;
    for e in edges {
        let vertical = matches!(e.side, Side::Left | Side::Right);
        // The fixed coordinate of this edge, and the span of the union along it.
        let (fixed, u_lo, u_hi) = match e.side {
            Side::Left => (ux0, uy0, uy1),
            Side::Right => (ux1, uy0, uy1),
            Side::Top => (uy0, ux0, ux1),
            Side::Bottom => (uy1, ux0, ux1),
        };
        let len = (u_hi - u_lo) as f32;
        let a = e.span.0.clamp(0.0, 1.0);
        let b = e.span.1.clamp(0.0, 1.0);
        // Half-open along the edge, exactly like `EdgeSegment`.
        let want_lo = u_lo + (a * len).round() as i32;
        let want_hi = u_lo + (b * len).round() as i32;
        for z in zones {
            // Rule 2: only a zone whose own boundary is the union's boundary here.
            let on_this_edge = match e.side {
                Side::Left => z.x == ux0,
                Side::Right => z.x1() == ux1,
                Side::Top => z.y == uy0,
                Side::Bottom => z.y1() == uy1,
            };
            if !on_this_edge {
                continue;
            }
            let (z_lo, z_hi) = if vertical {
                (z.y, z.y1())
            } else {
                (z.x, z.x1())
            };
            let lo = want_lo.max(z_lo);
            let hi = want_hi.min(z_hi);
            if hi <= lo {
                continue;
            }
            let Some(id) = NonZeroU32::new(next_id) else {
                continue;
            };
            next_id += 1;
            // `hi` is exclusive; the barrier's far end is the last pixel (rule 1).
            out.push(if vertical {
                PortalBarrier { id, side: e.side, x1: fixed, y1: lo, x2: fixed, y2: hi - 1 }
            } else {
                PortalBarrier { id, side: e.side, x1: lo, y1: fixed, x2: hi - 1, y2: fixed }
            });
        }
    }
    out
}
```

Create `crates/pheme-input/src/portal/mod.rs`:

```rust
//! Input capture on Wayland, through the `org.freedesktop.portal.InputCapture` portal
//! and libei.

pub mod geometry;
```

and in `crates/pheme-input/src/lib.rs`, beside the other Linux modules:

```rust
#[cfg(target_os = "linux")]
pub mod portal;
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p pheme-input portal::geometry`
Expected: PASS, all nine.

- [ ] **Step 7: Full gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: map capture edges onto portal pointer barriers

Adds the pure geometry the Wayland backend needs, with the portal's two
barrier rules pinned by tests before any D-Bus code exists. Both rules
fail silently: a rejected barrier appears only in failed_barriers, while
SetPointerBarriers and Enable both still report success and the session
then simply never activates.

The far boundary of a zone of width W at x0 is x0 + W, not x0 + W - 1.
A barrier at the intuitive last pixel is rejected; this was measured on
KDE, not inferred.

A barrier must also be fully contained within one zone, so a union edge
crossing two monitors is split into one barrier per zone. This cannot
show up on the single-monitor machine this project develops on, which
is why the test carries the portal specification's own two-monitor
example.

Also raises the workspace MSRV to 1.87 for ashpd, and takes ashpd with
only the two portal features that are used rather than its default set.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 5: Activation translation

**Files:**
- Create: `crates/pheme-input/src/portal/translate.rs`
- Modify: `crates/pheme-input/src/portal/mod.rs`

**Interfaces:**
- Produces: `portal::translate::{clamp_into, modifier_keys}`.

`Activated` carries the position the pointer *would* have reached had the barrier not stopped it, and the modifier mask in force at that moment. Both have to be translated before `ServerCore` can use them.

**The measurement that makes this task exist:** on a 2560-wide screen the reported x was **2577, 2564 and 2560** across three runs — always at or beyond the width, never inside. `on_edge()` accepts `0..=2559`. An unclamped position matches no edge, so the core returns no actions, so the pointer simply stops at the screen edge: no switch, no error, no log line. This is the single most likely way for the finished backend to appear "done" and do nothing.

- [ ] **Step 1: Write the failing tests**

Create `crates/pheme-input/src/portal/translate.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::{Rect, Side};
    use pheme_core::geometry::on_edge;

    fn screen() -> Rect {
        Rect { x: 0, y: 0, w: 2560, h: 1440 }
    }

    #[test]
    fn a_position_past_the_edge_lands_on_the_edge() {
        // The three values the portal actually reported on this machine.
        for reported in [2577.0_f32, 2564.0, 2560.0] {
            let (x, y) = clamp_into(&screen(), reported, 737.7);
            assert_eq!(x, 2559, "reported {reported}");
            assert_eq!(
                on_edge(&screen(), Side::Right, x, y),
                Some(737),
                "an unclamped position matches no edge, and the switch never happens"
            );
        }
    }

    #[test]
    fn a_position_before_the_origin_lands_on_the_origin() {
        let (x, y) = clamp_into(&screen(), -13.0, -4.0);
        assert_eq!((x, y), (0, 0));
        assert_eq!(on_edge(&screen(), Side::Left, x, y), Some(0));
    }

    #[test]
    fn an_offset_rect_clamps_to_its_own_bounds() {
        let r = Rect { x: 100, y: 50, w: 800, h: 600 };
        assert_eq!(clamp_into(&r, 5000.0, 5000.0), (899, 649));
        assert_eq!(clamp_into(&r, 0.0, 0.0), (100, 50));
    }

    #[test]
    fn a_fractional_position_truncates_toward_the_screen() {
        let (_, y) = clamp_into(&screen(), 2577.0, 737.9);
        assert_eq!(y, 737, "rounding up could push the value off the opposite edge");
    }

    #[test]
    fn the_shift_bit_becomes_the_shift_key() {
        // Measured: holding Shift across the barrier reported depressed = 0x1.
        assert_eq!(modifier_keys(0x1), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn every_modifier_bit_maps_to_its_key() {
        assert_eq!(modifier_keys(0x04), vec![KeyCode::LEFT_CTRL]);
        assert_eq!(modifier_keys(0x08), vec![KeyCode::LEFT_ALT]);
        assert_eq!(modifier_keys(0x40), vec![KeyCode::LEFT_GUI]);
    }

    #[test]
    fn several_modifiers_all_come_through() {
        let keys = modifier_keys(0x01 | 0x04 | 0x40);
        assert_eq!(
            keys,
            vec![KeyCode::LEFT_SHIFT, KeyCode::LEFT_CTRL, KeyCode::LEFT_GUI]
        );
    }

    #[test]
    fn bits_that_are_not_modifiers_are_ignored() {
        // 0x02 is Lock (caps), 0x10 is Mod2 (num lock): neither is a Pheme modifier,
        // and neither must produce a stray key press.
        assert!(modifier_keys(0x02 | 0x10).is_empty());
        // Asserting only the line above would still pass against a `modifier_keys`
        // that returned nothing for every input, so the name would outrun the test.
        // Mixing a real modifier in pins selective exclusion, which is the property.
        assert_eq!(modifier_keys(0x02 | 0x10 | 0x01), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn the_keys_produce_the_modifiers_the_protocol_expects() {
        use pheme_proto::Modifiers;
        let m = Modifiers::from_held(modifier_keys(0x01 | 0x08));
        assert_eq!(m, Modifiers(Modifiers::SHIFT | Modifiers::ALT));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p pheme-input portal::translate`
Expected: FAIL to compile — `clamp_into` and `modifier_keys` do not exist.

- [ ] **Step 3: Implement**

At the top of the same file:

```rust
//! Translating portal and libei events into `CaptureEvent`.

use pheme_core::Rect;
use pheme_proto::KeyCode;

/// Brings an `Activated` cursor position inside `rect`.
///
/// The portal reports where the pointer *would* have gone if the barrier had not
/// stopped it, not where it is. Measured on a 2560-wide screen across three runs:
/// 2577, 2564, 2560 — at or beyond the width every time, and the overshoot grows
/// with pointer speed.
///
/// `on_edge` accepts `0..=w-1`, so an unclamped position matches no edge at all:
/// `ServerCore` returns no actions, the pointer stops at the screen edge, and
/// nothing anywhere reports a problem. Clamping is what makes the backend work,
/// not a guard against a value that "should not" occur.
pub fn clamp_into(rect: &Rect, x: f32, y: f32) -> (i32, i32) {
    // Truncate rather than round: rounding 2559.6 up would leave the value one past
    // the edge again, which is the bug this function exists to prevent.
    let xi = (x as i32).clamp(rect.x, rect.x + rect.w - 1);
    let yi = (y as i32).clamp(rect.y, rect.y + rect.h - 1);
    (xi, yi)
}

/// The modifier keys held at the moment capture started, from libei's `depressed` mask.
///
/// The bit positions are the X11 core modifier layout, which every XKB keymap a
/// compositor generates uses in practice; `0x1` for Shift is the one that was
/// measured. XKB resolves virtual modifiers per keymap, so a keymap that puts Alt
/// somewhere other than Mod1 would produce a wrong modifier here — the spec accepts
/// that rather than linking xkbcommon to look the names up, because the cost is one
/// incorrect modifier in `Enter`, not a crash or a stuck key.
pub fn modifier_keys(depressed: u32) -> Vec<KeyCode> {
    const SHIFT: u32 = 0x01;
    const CONTROL: u32 = 0x04;
    const MOD1_ALT: u32 = 0x08;
    const MOD4_SUPER: u32 = 0x40;
    let mut out = Vec::new();
    for (bit, key) in [
        (SHIFT, KeyCode::LEFT_SHIFT),
        (CONTROL, KeyCode::LEFT_CTRL),
        (MOD1_ALT, KeyCode::LEFT_ALT),
        (MOD4_SUPER, KeyCode::LEFT_GUI),
    ] {
        if depressed & bit != 0 {
            out.push(key);
        }
    }
    out
}
```

Add `pub mod translate;` to `crates/pheme-input/src/portal/mod.rs`.

If `pheme_core::geometry::on_edge` is not public, make it so — the test uses it deliberately, because asserting `x == 2559` alone would still pass if `on_edge`'s own convention ever changed, and the property that matters is that the clamped point is one the core accepts.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p pheme-input portal::translate`
Expected: PASS, all nine.

- [ ] **Step 5: Full gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: translate a portal activation into core coordinates

Activated reports where the pointer would have gone had the barrier not
stopped it, not where it is. Measured on a 2560-wide screen across three
runs: 2577, 2564 and 2560, with the overshoot growing with pointer
speed.

on_edge accepts 0..=2559, so feeding the reported position straight to
the core matches no edge: no actions, no switch, no error and no log
line. The pointer just stops at the screen edge and the backend looks
finished while doing nothing. Clamping is what makes it work, not a
guard against an impossible value.

The test asserts through on_edge rather than against a literal, so the
property under test stays "the core accepts this point".

modifier_keys turns libei's depressed mask into the keys held at the
moment of crossing, which is what lets a Shift held across the edge
reach the client. 0x1 for Shift was measured; the remaining bit
positions are the X11 core modifier layout.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 6: Event translation and held-key tracking

**Files:**
- Modify: `crates/pheme-input/src/portal/translate.rs`
- Modify: `crates/pheme-input/src/linux_uinput.rs`

**Interfaces:**
- Consumes: `translate::modifier_keys` from Task 5.
- Produces: `translate::{Motion, button_from_evdev, wheel_from_discrete, HeldKeys}`.

Four separate translations, each with a measured fact behind it:

- **Key codes are raw evdev.** Measured twice: `42` while Shift was held (`KEY_LEFTSHIFT`) and `30` for the letter A (`KEY_A`). The X11 path does `keycode - 8` because X11 offsets evdev by 8; copying that here shifts every key by eight positions. The libei keyboard device *does* carry an XKB keymap, which makes the wrong assumption look justified.
- **Button codes are raw evdev too** — `272` is `BTN_LEFT`. `linux_uinput.rs` already owns this mapping in the other direction, so this is its inverse and the two are tested against each other rather than written twice.
- **Discrete scroll is 120 per notch**, which is already the unit `CaptureEvent::Wheel` uses — the X11 backend emits `dy: 120` per notch.
- **Relative motion arrives as `f32`.** Truncating each event loses slow movement entirely: a steady 0.4 px per event would never move the pointer at all.

- [ ] **Step 1: Make the uinput button mapping shared**

In `crates/pheme-input/src/linux_uinput.rs` change `fn button_code` to `pub(crate) fn button_code`, and the five `const BTN_*` to `pub(crate) const`.

- [ ] **Step 2: Write the failing tests**

Append to `crates/pheme-input/src/portal/translate.rs`'s test module:

```rust
    #[test]
    fn key_codes_are_evdev_and_are_not_shifted_by_eight() {
        // Measured: 42 while Shift was held, 30 for the letter A. Under the X11
        // convention these would be KEY_G and KEY_Y.
        assert_eq!(key_from_evdev(42), Some(KeyCode::LEFT_SHIFT));
        assert_eq!(key_from_evdev(30), crate::keymap::evdev_to_hid(30));
        assert_ne!(
            key_from_evdev(30),
            crate::keymap::evdev_to_hid(30 - 8),
            "subtracting 8 is the X11 convention and is wrong here"
        );
    }

    #[test]
    fn an_unknown_key_code_is_dropped_rather_than_guessed() {
        assert_eq!(key_from_evdev(0xFFFF), None);
    }

    #[test]
    fn button_codes_round_trip_against_the_uinput_mapping() {
        use pheme_proto::Button;
        for b in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            let code = crate::linux_uinput::button_code(b);
            assert_eq!(
                button_from_evdev(code as u32),
                Some(b),
                "{b:?} does not survive the round trip"
            );
        }
        // The measured value, named explicitly so the round trip cannot pass by
        // agreeing with itself on a wrong constant.
        assert_eq!(button_from_evdev(272), Some(Button::Left));
    }

    #[test]
    fn an_unknown_button_is_dropped() {
        assert_eq!(button_from_evdev(999), None);
    }

    #[test]
    fn one_scroll_notch_is_one_hundred_and_twenty() {
        assert_eq!(wheel_from_discrete(0, 120), (0, 120));
        assert_eq!(wheel_from_discrete(0, -120), (0, -120));
        assert_eq!(wheel_from_discrete(-120, 0), (-120, 0));
    }

    #[test]
    fn slow_motion_is_accumulated_rather_than_truncated_away() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..8 {
            let (dx, _) = m.push(0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, 3, "0.4 x 8 is 3.2 px; truncating each event gives 0");
    }

    #[test]
    fn the_remainder_does_not_drift_over_a_long_run() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..1000 {
            let (dx, _) = m.push(1.5, 0.0);
            total += dx;
        }
        assert_eq!(total, 1500);
    }

    #[test]
    fn negative_motion_accumulates_symmetrically() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..8 {
            let (dx, _) = m.push(-0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, -3);
    }

    #[test]
    fn held_keys_are_released_when_capture_ends() {
        let mut h = HeldKeys::default();
        h.saw(KeyCode::LEFT_SHIFT, true);
        h.saw(KeyCode(0x04), true); // the letter A
        h.saw(KeyCode(0x04), false);
        assert_eq!(h.flush(), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn flushing_twice_releases_nothing_the_second_time() {
        let mut h = HeldKeys::default();
        h.saw(KeyCode::LEFT_SHIFT, true);
        assert_eq!(h.flush().len(), 1);
        assert!(
            h.flush().is_empty(),
            "a second flush would send a key-up for a key nobody is holding"
        );
    }

    #[test]
    fn a_key_held_across_a_release_does_not_leak_into_the_next_capture() {
        // Hold Shift, cross, come back, release Shift while local. The key-up goes to
        // the compositor and this backend never sees it, so without the flush `held`
        // in ServerCore would keep Shift forever and every later Enter would be wrong.
        let mut h = HeldKeys::default();
        for k in modifier_keys(0x1) {
            h.saw(k, true);
        }
        assert_eq!(h.flush(), vec![KeyCode::LEFT_SHIFT]);
        assert!(h.flush().is_empty());
    }
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p pheme-input portal::translate`
Expected: FAIL to compile — none of `key_from_evdev`, `button_from_evdev`, `wheel_from_discrete`, `Motion`, `HeldKeys` exist.

- [ ] **Step 4: Implement**

```rust
use std::collections::BTreeSet;

use pheme_proto::Button;

use crate::keymap;
use crate::linux_uinput::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};

/// libei key codes are **raw evdev**, unlike X11's, which offset evdev by 8.
///
/// Measured twice: 42 while Shift was held (`KEY_LEFTSHIFT`) and 30 for the letter A
/// (`KEY_A`). Under the X11 convention those would decode as `KEY_G` and `KEY_Y`. The
/// libei keyboard device does carry an XKB keymap, so borrowing the X11 path's
/// `- 8` looks reasonable and shifts every key by eight positions.
pub fn key_from_evdev(code: u32) -> Option<KeyCode> {
    keymap::evdev_to_hid(u16::try_from(code).ok()?)
}

/// The inverse of `linux_uinput::button_code`, which owns this mapping.
pub fn button_from_evdev(code: u32) -> Option<Button> {
    Some(match u16::try_from(code).ok()? {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE => Button::Back,
        BTN_EXTRA => Button::Forward,
        _ => return None,
    })
}

/// libei's discrete scroll is already in the 120-per-notch unit `CaptureEvent::Wheel`
/// uses, and that the X11 backend emits.
pub fn wheel_from_discrete(dx: i32, dy: i32) -> (i32, i32) {
    (dx, dy)
}

/// Accumulates libei's `f32` relative motion into whole pixels.
///
/// Truncating each event independently loses slow movement entirely: a steady
/// 0.4 px per event would never move the pointer at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct Motion {
    rem_x: f32,
    rem_y: f32,
}

impl Motion {
    /// Returns the whole pixels to emit, carrying the remainder into the next call.
    pub fn push(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        let x = self.rem_x + dx;
        let y = self.rem_y + dy;
        let (ix, iy) = (x.trunc(), y.trunc());
        self.rem_x = x - ix;
        self.rem_y = y - iy;
        (ix as i32, iy as i32)
    }
}

/// Tracks which keys this backend has seen pressed and not released.
///
/// A Wayland server sees the keyboard only while capturing. Hold Shift, cross the
/// edge, come back, then release Shift: the key-up goes to the compositor and never
/// reaches this backend, so `ServerCore`'s `held` set keeps Shift forever and every
/// later `Enter` reports a modifier nobody is pressing. `flush` is what prevents it.
#[derive(Debug, Default)]
pub struct HeldKeys(BTreeSet<u16>);

impl HeldKeys {
    pub fn saw(&mut self, code: KeyCode, down: bool) {
        if down {
            self.0.insert(code.0);
        } else {
            self.0.remove(&code.0);
        }
    }

    /// The keys still held, clearing the set. Emit a key-up for each.
    pub fn flush(&mut self) -> Vec<KeyCode> {
        std::mem::take(&mut self.0).into_iter().map(KeyCode).collect()
    }
}
```

`KeyCode` is a tuple struct over `u16`; if its field is private, store `KeyCode` in the set directly and derive whatever ordering it needs, or use a `Vec` with `contains`. Do not change `pheme-proto` for this.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p pheme-input portal::translate`
Expected: PASS, all twenty.

- [ ] **Step 6: Verify the mutation the tests are meant to catch**

Temporarily change `key_from_evdev` to `keymap::evdev_to_hid(u16::try_from(code).ok()? - 8)` and run the tests.
Expected: `key_codes_are_evdev_and_are_not_shifted_by_eight` FAILS. Revert the change.

Then temporarily change `Motion::push` to ignore the remainder (`(dx.trunc() as i32, dy.trunc() as i32)`) and run.
Expected: `slow_motion_is_accumulated_rather_than_truncated_away` FAILS. Revert.

A test that cannot fail on the property it names is the defect this plan is most concerned with; these two checks take a minute and prove these can.

- [ ] **Step 7: Full gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: translate libei input events for the Wayland backend

Four translations, each with a measurement behind it.

Key codes from libei are raw evdev, not the X11 convention of evdev
plus 8. Measured twice: 42 with Shift held and 30 for the letter A,
which under the X11 reading would be KEY_G and KEY_Y. The libei
keyboard device does carry an XKB keymap, which makes borrowing the
X11 path's minus-eight look justified, so a test asserts the wrong
reading is wrong rather than only that the right one is right.

Button codes are evdev too. linux_uinput already owns that mapping in
the other direction, so this is its inverse and the test round-trips
the two against each other plus the one measured constant, so the pair
cannot agree with itself on a wrong value.

Discrete scroll already arrives in the 120-per-notch unit the X11
backend emits.

Relative motion arrives as f32 and is accumulated, because truncating
each event separately loses slow movement completely -- a steady
0.4 px per event would never move the pointer.

HeldKeys exists because a Wayland server sees the keyboard only while
capturing: a modifier released after the pointer comes back is never
observed, and without a flush it stays held in the core forever.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 7: The portal session

**Files:**
- Create: `crates/pheme-input/src/portal/session.rs`
- Modify: `crates/pheme-input/src/portal/mod.rs`

**Interfaces:**
- Consumes: `geometry::{Zone, barriers}` (Task 4), `translate::*` (Tasks 5–6).
- Produces: `session::{Cmd, run}`; `mod::PortalCapture` implementing `InputCapture`.

This task builds the thread. Everything decidable by a pure function already is one, so what remains is sequencing D-Bus and libei correctly — and that sequence has one step whose omission is completely silent.

**The order is not negotiable:**

```
CreateSession2
  → Start(Keyboard | Pointer)          read the GRANTED capabilities from the response
  → GetZones                           keep zone_set; it is required by SetPointerBarriers
  → SetPointerBarriers(barriers, zone_set)   check failed_barriers
  → ConnectToEIS                       an OwnedFd
  → handshake_async_io(Receiver)
  → on SeatAdded: bind_capabilities(..) then Connection::flush()   <-- see below
  → Enable
```

**The silent step:** without `Connection::flush()` after `bind_capabilities`, the bind request stays in the buffer, the compositor creates no devices, and not one input event ever arrives. The probe reproduced exactly this: `SeatAdded`, then silence, with no error anywhere. A backend missing this line is indistinguishable from a compositor that refuses to capture.

**`SetPointerBarriers` suspends the session**, so `Enable` must follow *every* call to it — including the ones a later client connection triggers, not just the first.

- [ ] **Step 1: Write the module skeleton and its types**

Create `crates/pheme-input/src/portal/session.rs`:

```rust
//! The portal session thread: D-Bus on one side, libei on the other, and the
//! `CaptureEvent` stream `ServerCore` consumes coming out.

use std::os::unix::net::UnixStream;

use ashpd::desktop::input_capture::{
    Barrier, BarrierPosition, Capabilities, ConnectToEISOptions, CreateSession2Options,
    DisableOptions, EnableOptions, GetZonesOptions, InputCapture as Portal, ReleaseOptions,
    SetPointerBarriersOptions, StartOptions,
};
use ashpd::desktop::PersistMode;
use crossbeam_channel::Sender as EventSender;
use futures_lite::StreamExt;
use pheme_core::{CaptureEvent, Rect};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tracing::{debug, error, info, warn};

use crate::portal::geometry::{barriers, Zone};
use crate::portal::translate::{
    button_from_evdev, clamp_into, key_from_evdev, modifier_keys, wheel_from_discrete, HeldKeys,
    Motion,
};
use crate::{CaptureEdge, Error, Result};

/// Commands the backend thread accepts. Each carries an acknowledgement channel, so
/// the trait's synchronous contract is met by waiting for the thread to answer.
pub(crate) enum Cmd {
    SetEdges(Vec<CaptureEdge>, async_channel::Sender<Result<()>>),
    Release {
        x: i32,
        y: i32,
        ack: async_channel::Sender<Result<()>>,
    },
    Stop,
}
```

- [ ] **Step 2: Write the session-establishment function**

```rust
struct Session {
    // `ashpd::desktop::Session<T>` takes one generic parameter and no lifetime, and
    // `InputCapture` has no lifetime parameter either.
    portal: Portal,
    session: ashpd::desktop::Session<Portal>,
    zones: Vec<Zone>,
    zone_set: u32,
    /// The barrier ids currently declared, and which side each came from.
    sides: std::collections::HashMap<u32, pheme_core::Side>,
    /// Set while a capture is running; `Release` is ignored for any other id.
    activation: Option<u32>,
}

async fn establish(edges: &[CaptureEdge]) -> Result<(Session, ei::Context)> {
    let portal = Portal::new().await.map_err(pe)?;
    let session = portal
        .create_session2(CreateSession2Options::default())
        .await
        .map_err(pe)?;

    let start = portal
        .start(
            &session,
            None,
            StartOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(pe)?
        .response()
        .map_err(pe)?;

    // Read what was GRANTED, not what was asked for. KDE advertises Touchscreen and
    // does not grant it; a backend that assumes the request was honoured would wait
    // for devices that never appear.
    let granted = start.capabilities();
    if !granted.contains(Capabilities::Pointer) {
        return Err(Error::Permission(
            "the compositor did not grant pointer capture".into(),
        ));
    }
    if !granted.contains(Capabilities::Keyboard) {
        warn!("the compositor granted pointer capture but not keyboard capture");
    }
    info!(?granted, restore_token = ?start.restore_token(), "input capture session started");

    let mut s = Session {
        portal,
        session,
        zones: Vec::new(),
        zone_set: 0,
        sides: Default::default(),
        activation: None,
    };
    s.refresh_zones().await?;
    s.set_barriers(edges).await?;

    let fd = s
        .portal
        .connect_to_eis(&s.session, ConnectToEISOptions::default())
        .await
        .map_err(pe)?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true).map_err(|e| Error::Backend(e.to_string()))?;
    let ctx = ei::Context::new(stream).map_err(|e| Error::Backend(e.to_string()))?;
    Ok((s, ctx))
}

fn pe(e: impl std::fmt::Display) -> Error {
    Error::Backend(format!("portal: {e}"))
}
```

- [ ] **Step 3: Write the zone and barrier methods**

```rust
impl Session {
    async fn refresh_zones(&mut self) -> Result<()> {
        let z = self
            .portal
            .zones(&self.session, GetZonesOptions::default())
            .await
            .map_err(pe)?
            .response()
            .map_err(pe)?;
        self.zones = z
            .regions()
            .iter()
            .map(|r| Zone {
                x: r.x_offset(),
                y: r.y_offset(),
                w: r.width(),
                h: r.height(),
            })
            .collect();
        self.zone_set = z.zone_set();
        debug!(zone_set = self.zone_set, zones = ?self.zones, "zones");
        Ok(())
    }

    /// Declares barriers for `edges` and re-enables the session.
    ///
    /// `SetPointerBarriers` suspends the session, so `Enable` must follow *every*
    /// call — not only the first. A client connecting mid-session lands here.
    async fn set_barriers(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        let want = barriers(&self.zones, edges);
        self.sides = want.iter().map(|b| (b.id.get(), b.side)).collect();
        let list: Vec<Barrier> = want
            .iter()
            .map(|b| Barrier::new(b.id, BarrierPosition::new(b.x1, b.y1, b.x2, b.y2)))
            .collect();
        let resp = self
            .portal
            .set_pointer_barriers(
                &self.session,
                &list,
                self.zone_set,
                SetPointerBarriersOptions::default(),
            )
            .await
            .map_err(pe)?
            .response()
            .map_err(pe)?;

        // A rejected barrier is reported ONLY here: the call succeeds, Enable
        // succeeds, and the session then never activates. Never swallow this.
        let failed = resp.failed_barriers();
        if !failed.is_empty() {
            for id in failed {
                let side = self.sides.get(&id.get());
                error!(id = id.get(), ?side, "the compositor rejected this pointer barrier");
            }
            return Err(Error::Backend(format!(
                "the compositor rejected {} of {} pointer barriers",
                failed.len(),
                list.len()
            )));
        }

        if list.is_empty() {
            // No edges left: stop capturing entirely rather than leave a session
            // armed with nothing to trigger it.
            self.portal
                .disable(&self.session, DisableOptions::default())
                .await
                .map_err(pe)?;
            return Ok(());
        }
        self.portal
            .enable(&self.session, EnableOptions::default())
            .await
            .map_err(pe)
    }
}
```

- [ ] **Step 4: Write the libei bind step**

```rust
/// Binds the capabilities we need on every seat the compositor offers.
///
/// The `flush` is load-bearing: without it the bind request never leaves the
/// buffer, the compositor creates no devices, and not one input event ever
/// arrives — with no error and nothing in any log. This was reproduced during
/// design: `SeatAdded` followed by silence.
fn bind_seat(ctx: &ei::Context, seat: &reis::event::Seat) {
    seat.bind_capabilities(
        DeviceCapability::Pointer
            | DeviceCapability::Keyboard
            | DeviceCapability::Scroll
            | DeviceCapability::Button,
    );
    if let Err(e) = ctx.flush() {
        error!("flushing the libei capability bind failed: {e}; no devices will appear");
    }
}
```

- [ ] **Step 5: Add the module declaration and build**

Add `pub(crate) mod session;` to `crates/pheme-input/src/portal/mod.rs`.

Run: `cargo build -p pheme-input`
Expected: compiles, with dead-code warnings for the not-yet-called items. If any ashpd or reis signature disagrees with the code above, follow the compiler — the versions are pinned in Task 4 and the compiler is authoritative, not this plan.

- [ ] **Step 6: Commit the skeleton**

```bash
git add -A
git commit -F - <<'MSG'
WIP: establish the InputCapture portal session

Creates the session, reads the capabilities the compositor actually
granted rather than the ones requested, fetches the zones, declares
barriers and connects to libei.

Three things here fail silently if written the obvious way, so each
carries the reason in a comment:

SetPointerBarriers suspends the session, so Enable must follow every
call to it and not only the first -- a client connecting mid-session
otherwise declares barriers that are never armed.

A rejected barrier appears only in failed_barriers; the call itself
succeeds and so does Enable, and the session then simply never
activates. It is treated as an error with the offending side named.

The flush after binding libei capabilities is what makes devices
appear. Without it the bind sits in the buffer and no input event ever
arrives, with nothing reported anywhere.

KDE advertises Touchscreen and does not grant it, so the granted set is
read from the response.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 8: The run loop

**Files:**
- Modify: `crates/pheme-input/src/portal/session.rs`

**Interfaces:**
- Consumes: everything from Task 7.
- Produces: `pub(crate) async fn run(tx: EventSender<CaptureEvent>, cmds: async_channel::Receiver<Cmd>, screens: Rect, edges: Vec<CaptureEdge>)`.

One `select` over four sources. The loop owns the `ei::Context`, which is not `Send` and therefore never leaves this thread.

- [ ] **Step 1: Write the loop**

```rust
pub(crate) async fn run(
    tx: EventSender<CaptureEvent>,
    cmds: async_channel::Receiver<Cmd>,
    screen: Rect,
    edges: Vec<CaptureEdge>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) {
    let (mut sess, ctx) = match establish(&edges).await {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let (conn, mut events) = match ctx
        .handshake_async_io("pheme", ei::handshake::ContextType::Receiver)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("libei handshake failed: {e}");
            return;
        }
    };
    let _ = conn;

    let mut activated = match sess.portal.receive_activated().await {
        Ok(s) => s,
        Err(e) => {
            error!("subscribing to Activated failed: {e}");
            return;
        }
    };
    let mut deactivated = match sess.portal.receive_deactivated().await {
        Ok(s) => s,
        Err(e) => {
            error!("subscribing to Deactivated failed: {e}");
            return;
        }
    };
    let mut disabled = match sess.portal.receive_disabled().await {
        Ok(s) => s,
        Err(e) => {
            error!("subscribing to Disabled failed: {e}");
            return;
        }
    };
    let mut zones_changed = match sess.portal.receive_zones_changed().await {
        Ok(s) => s,
        Err(e) => {
            error!("subscribing to ZonesChanged failed: {e}");
            return;
        }
    };

    let mut motion = Motion::default();
    let mut held = HeldKeys::default();
    let mut edges = edges;

    loop {
        // `or` is biased toward its first argument. Commands come first so a Stop is
        // never starved by a burst of motion events, which arrive in the thousands.
        let ev = futures_lite::future::or(
            async { Step::Cmd(cmds.recv().await.ok()) },
            futures_lite::future::or(
                async { Step::Portal(PortalEvent::next(&mut activated, &mut deactivated, &mut disabled, &mut zones_changed).await) },
                async { Step::Ei(events.next().await) },
            ),
        )
        .await;

        match ev {
            Step::Cmd(None) | Step::Cmd(Some(Cmd::Stop)) => break,
            Step::Cmd(Some(Cmd::SetEdges(new, ack))) => {
                edges = new;
                let r = sess.set_barriers(&edges).await;
                let _ = ack.send(r).await;
            }
            Step::Cmd(Some(Cmd::Release { x, y, ack })) => {
                let r = release_capture(&mut sess, &mut held, &tx, x, y).await;
                let _ = ack.send(r).await;
            }
            Step::Portal(PortalEvent::Activated { id, barrier, x, y }) => { /* body in Step 2 below */ }
            Step::Portal(PortalEvent::Deactivated { id }) => { /* body in Step 3 below */ }
            Step::Portal(PortalEvent::Disabled) => { /* body in Step 3 below */ }
            Step::Portal(PortalEvent::ZonesChanged) => {
                if let Err(e) = sess.refresh_zones().await {
                    error!("refreshing zones failed: {e}");
                } else if let Err(e) = sess.set_barriers(&edges).await {
                    error!("re-declaring barriers after a zone change failed: {e}");
                }
            }
            Step::Portal(PortalEvent::Closed) | Step::Ei(None) => {
                error!("the input capture session ended");
                break;
            }
            Step::Ei(Some(Err(e))) => {
                error!("libei stream error: {e}");
                break;
            }
            Step::Ei(Some(Ok(e))) => { /* body in Step 4 below */ }
        }
    }
    // Dropping `tx` here is what lets the receiver observe disconnection, which the
    // trait's `stop()` contract requires.
    drop(tx);
}
```

`Step` and `PortalEvent` are small local enums; write them to suit, keeping the bias order above. If `futures_lite::future::or` proves awkward to nest three ways, use `futures_lite::future::race` over boxed futures or a small `poll_fn` — what matters is that commands win over input events, not the combinator.

- [ ] **Step 2: Handle `Activated`**

```rust
            Step::Portal(PortalEvent::Activated { id, barrier, x, y }) => {
                sess.activation = id;
                motion = Motion::default();
                // The reported position lies outside the zone (measured: 2577, 2564,
                // 2560 on a 2560-wide screen). Unclamped it matches no edge and the
                // switch silently never happens.
                let (cx, cy) = clamp_into(&screen, x, y);
                debug!(?barrier, reported = ?(x, y), clamped = ?(cx, cy), "capture activated");
                if tx.try_send(CaptureEvent::MotionAbs { x: cx, y: cy }).is_err() {
                    warn!("dropped the activation event");
                }
            }
```

The modifier mask arrives separately, as a `KeyboardModifiers` libei event right after the devices resume — that is where `modifier_keys` is used (Step 4), not here.

- [ ] **Step 3: Handle `Deactivated` and `Disabled`**

```rust
            Step::Portal(PortalEvent::Deactivated { id }) => {
                // A Deactivated may arrive after our own Release; the specification
                // says so explicitly. Only one for the *current* activation means the
                // compositor ended the capture on its own.
                if id.is_some() && id == sess.activation {
                    sess.activation = None;
                    for k in held.flush() {
                        let _ = tx.try_send(CaptureEvent::Key { code: k, down: false });
                    }
                    // The caller turns this into ServerCore::release_remote().
                    let _ = tx.try_send(CaptureEvent::CaptureEnded);
                }
            }
            Step::Portal(PortalEvent::Disabled) => {
                warn!("the compositor disabled the session; re-enabling");
                if let Err(e) = sess.set_barriers(&edges).await {
                    error!("re-enabling after Disabled failed: {e}");
                }
            }
```

`CaptureEvent::CaptureEnded` and its routing to `ServerCore::release_remote()` were added in Task 2; this is the only backend that ever produces it.

- [ ] **Step 4: Handle libei events**

```rust
            Step::Ei(Some(Ok(e))) => match e {
                EiEvent::SeatAdded(s) => bind_seat(&ctx, &s.seat),
                EiEvent::KeyboardModifiers(m) => {
                    // The modifiers held when the capture started, including ones
                    // pressed before it — this is what carries a Shift held across
                    // the edge through to the client.
                    for k in modifier_keys(m.depressed) {
                        held.saw(k, true);
                        let _ = tx.try_send(CaptureEvent::Key { code: k, down: true });
                    }
                }
                EiEvent::PointerMotion(m) => {
                    let (dx, dy) = motion.push(m.dx, m.dy);
                    if dx != 0 || dy != 0 {
                        let _ = tx.try_send(CaptureEvent::MotionRel { dx, dy });
                    }
                }
                EiEvent::Button(b) => {
                    if let Some(btn) = button_from_evdev(b.button) {
                        let down = matches!(b.state, ei::button::ButtonState::Press);
                        let _ = tx.try_send(CaptureEvent::Button { btn, down });
                    }
                }
                EiEvent::ScrollDiscrete(s) => {
                    let (dx, dy) = wheel_from_discrete(s.discrete_dx, s.discrete_dy);
                    if dx != 0 || dy != 0 {
                        let _ = tx.try_send(CaptureEvent::Wheel { dx, dy });
                    }
                }
                EiEvent::KeyboardKey(k) => {
                    // evdev codes, with no `- 8`: that is the X11 convention (§3.3).
                    if let Some(code) = key_from_evdev(k.key) {
                        let down = matches!(k.state, ei::keyboard::KeyState::Press);
                        held.saw(code, down);
                        let _ = tx.try_send(CaptureEvent::Key { code, down });
                    }
                }
                // Frame is a batch delimiter; ScrollDelta would double-count against
                // ScrollDiscrete; device lifecycle events need no action here.
                _ => {}
            },
```

Every send is `try_send`: the trait forbids blocking and requires dropping events when the channel is full. 1292 to 2030 events arrived in a few seconds during the probe, so a full channel is a normal condition, not an error.

- [ ] **Step 5: Write `release_capture`**

```rust
async fn release_capture(
    sess: &mut Session,
    held: &mut HeldKeys,
    tx: &EventSender<CaptureEvent>,
    x: i32,
    y: i32,
) -> Result<()> {
    // Keys held when the capture ends are never seen being released: the key-up goes
    // to the compositor. Without this the core keeps them held forever.
    for k in held.flush() {
        let _ = tx.try_send(CaptureEvent::Key { code: k, down: false });
    }
    let Some(id) = sess.activation.take() else {
        return Ok(());
    };
    sess.portal
        .release(
            &sess.session,
            ReleaseOptions::default()
                .set_activation_id(Some(id))
                .set_cursor_position(Some((x as f64, y as f64))),
        )
        .await
        .map_err(pe)
}
```

The `activation_id` must be the one being ended; the specification says a compositor ignores a `Release` for an id that is no longer active, so a stale id means the capture is never released at all.

- [ ] **Step 6: Build and gate**

Run: `cargo build -p pheme-input && cargo clippy -p pheme-input --all-targets -- -D warnings`
Expected: clean. Then `cargo test --workspace` for the new core test.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -F - <<'MSG'
WIP: run the portal session and translate its events

One select over four sources -- commands, portal signals, libei events
and the stop signal -- on a single thread, because ei::Context is not
Send and must never leave the thread that made it. Commands are biased
first so a Stop is not starved by motion events, which arrived in the
thousands within seconds during the design probe.

Activated clamps the reported position into the screen rect before it
reaches the core, which is what makes a switch happen at all.

Deactivated for the current activation means the compositor ended the
capture itself, which the core cannot otherwise learn: it would still
believe it was Remote while the client still held the input. It becomes
CaptureEvent::CaptureEnded, routed to ServerCore::release_remote.

Keys still held are released on both paths that end a capture. A
modifier released after the pointer comes back is never observed by
this backend, so without the flush it would stay held in the core and
corrupt every later Enter.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 9: The backend, selection, and a live test

**Files:**
- Modify: `crates/pheme-input/src/portal/mod.rs`
- Modify: `crates/pheme-input/src/lib.rs`
- Modify: `crates/pheme-input/src/linux_x11.rs`
- Create: `crates/pheme-input/tests/portal_live.rs`

**Interfaces:**
- Consumes: `session::{Cmd, run}` from Tasks 7–8.
- Produces: `portal::PortalCapture`; `detect_capture()` choosing it on Wayland.

**The selection bug this fixes:** a KDE Wayland session also sets `DISPLAY=:0` for XWayland. Today `detect_capture()` connects to X11 first, succeeds, installs XInput2 and XTest — and captures nothing but XWayland clients. No layer reports an error. Wayland must be checked first.

Today that guard lives inside `X11Capture::new()`, which returns `Error::Unsupported` when `WAYLAND_DISPLAY` is set. Move the decision to `detect_capture()` so `X11Capture` stays usable on its own (the live test and any future diagnostic want it), and so there is exactly one place that decides.

- [ ] **Step 1: Implement the backend struct**

In `crates/pheme-input/src/portal/mod.rs`:

```rust
//! Input capture on Wayland, through the `org.freedesktop.portal.InputCapture`
//! portal and libei.

pub mod geometry;
pub(crate) mod session;
pub mod translate;

use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use pheme_core::{CaptureEvent, Rect};
use pheme_proto::ScreenInfo;
use tracing::error;

use crate::linux_screens::wayland_screens;
use crate::{CaptureEdge, CaptureMode, Error, InputCapture, Result};
use session::Cmd;

/// How long a command waits for the session thread, matching the trait's contract
/// and the X11 backend's `MODE_CHANGE_TIMEOUT`.
const CMD_TIMEOUT: Duration = Duration::from_secs(1);

pub struct PortalCapture {
    screens: Vec<ScreenInfo>,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: Option<async_channel::Receiver<Cmd>>,
    edges: Vec<CaptureEdge>,
    thread: Option<JoinHandle<()>>,
}

impl PortalCapture {
    pub fn new() -> Result<PortalCapture> {
        let screens = wayland_screens()?;
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        Ok(PortalCapture {
            screens,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            edges: Vec::new(),
            thread: None,
        })
    }

    /// Sends a command and waits for the session thread's answer, so the trait's
    /// synchronous contract holds.
    fn call(&self, make: impl FnOnce(async_channel::Sender<Result<()>>) -> Cmd) -> Result<()> {
        let (ack_tx, ack_rx) = async_channel::bounded(1);
        self.cmd_tx
            .send_blocking(make(ack_tx))
            .map_err(|_| Error::Backend("the portal session thread is gone".into()))?;
        match ack_rx.recv_blocking_timeout(CMD_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(Error::Backend("mode change timed out".into())),
        }
    }
}
```

`async_channel` may not expose `recv_blocking_timeout`; if it does not, wrap `recv_blocking` with `futures_lite::future::block_on(futures_lite::future::or(ack_rx.recv(), async { async_io::Timer::after(CMD_TIMEOUT).await; Err(..) }))`, or use a `std::sync::mpsc` for the acknowledgement direction only — it never crosses into the executor's wait. The requirement is the 1 s timeout mapped to `Error::Backend("mode change timed out")`, not a particular channel type.

- [ ] **Step 2: Implement the trait**

```rust
impl InputCapture for PortalCapture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        let cmd_rx = self
            .cmd_rx
            .take()
            .ok_or_else(|| Error::Backend("already started".into()))?;
        let screen = Rect::bounds(&self.screens);
        let edges = self.edges.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pheme-portal".into())
            .spawn(move || {
                futures_lite::future::block_on(session::run(tx, cmd_rx, screen, edges, ready_tx))
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        self.thread = Some(thread);
        // Wait for the session to exist before returning, so a `set_edges` issued
        // immediately after `start()` is not answered by a thread that has not yet
        // created its session — and so a refused permission surfaces here.
        ready_rx
            .recv()
            .map_err(|_| Error::Backend("the portal session thread exited during startup".into()))?
    }

    /// The compositor is already capturing by the time the core asks for a grab, and
    /// releasing is `release`, not a mode. Both directions are acknowledged with
    /// nothing to do.
    fn set_mode(&mut self, _mode: CaptureMode) -> Result<()> {
        Ok(())
    }

    /// A captured pointer is hidden and parked by the compositor, and an uncaptured
    /// one cannot be moved by an application under Wayland. The core only warps
    /// during `abort_switch`, where doing nothing is correct.
    fn warp_cursor(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn release(&mut self, x: i32, y: i32) -> Result<()> {
        self.call(|ack| Cmd::Release { x, y, ack })
    }

    fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        self.edges = edges.to_vec();
        if self.thread.is_none() {
            // Not started yet; `start` passes these through to the session.
            return Ok(());
        }
        self.call(|ack| Cmd::SetEdges(edges.to_vec(), ack))
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    fn stop(&mut self) {
        let _ = self.cmd_tx.send_blocking(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
```

- [ ] **Step 3: Move the Wayland guard out of `X11Capture`**

In `crates/pheme-input/src/linux_x11.rs`, delete the `WAYLAND_DISPLAY` check at the top of `X11Capture::new()`. It is replaced by the selection in Step 4, and leaving both would make `X11Capture` unusable for the live test.

- [ ] **Step 4: Write the failing selection test, then the selection**

In `crates/pheme-input/src/lib.rs`:

```rust
/// True when these values describe a Wayland session.
///
/// Takes the two values instead of reading the environment itself, so the decision
/// is a pure function and its test needs no process-wide mutation. Every
/// `cargo test` binary is multithreaded and `std::env::set_var` is unsound there —
/// which is why the 2024 edition made it `unsafe`.
///
/// This is checked **before** X11. A Wayland session also sets `DISPLAY` for
/// XWayland, so an X11 connection succeeds, XInput2 and XTest install without
/// error, and the backend then captures nothing but XWayland clients. Nothing at
/// any layer reports it.
#[cfg(target_os = "linux")]
fn is_wayland(wayland_display: Option<&std::ffi::OsStr>, session_type: Option<&str>) -> bool {
    wayland_display.is_some() || session_type.is_some_and(|v| v.eq_ignore_ascii_case("wayland"))
}
```

and in `detect_capture()`:

```rust
    #[cfg(target_os = "linux")]
    {
        let session_type = std::env::var("XDG_SESSION_TYPE").ok();
        if is_wayland(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            session_type.as_deref(),
        ) {
            return portal::PortalCapture::new()
                .map(|c| Box::new(c) as Box<dyn InputCapture>)
                .map_err(|e| match e {
                    Error::Backend(m) | Error::Unsupported(m) => Error::Unsupported(format!(
                        "Wayland capture needs a compositor that implements the InputCapture \
                         portal (KDE, GNOME): {m}. wlroots compositors such as Hyprland and \
                         Sway are not supported yet; this machine can still be used as a \
                         client."
                    )),
                    other => other,
                });
        }
        linux_x11::X11Capture::new().map(|c| Box::new(c) as Box<dyn InputCapture>)
    }
```

and the test, which needs no environment at all:

```rust
#[cfg(all(test, target_os = "linux"))]
mod selection_tests {
    use std::ffi::OsStr;

    #[test]
    fn a_wayland_session_is_detected_even_when_x11_is_also_available() {
        // Under XWayland both are set. Checking X11 first would connect happily and
        // then capture nothing but XWayland clients, with no error anywhere.
        assert!(super::is_wayland(Some(OsStr::new("wayland-0")), None));
        assert!(super::is_wayland(Some(OsStr::new("wayland-0")), Some("x11")));
    }

    #[test]
    fn the_session_type_alone_is_enough() {
        assert!(super::is_wayland(None, Some("wayland")));
        assert!(super::is_wayland(None, Some("Wayland")), "the comparison ignores case");
    }

    #[test]
    fn an_x11_session_is_not_mistaken_for_wayland() {
        assert!(!super::is_wayland(None, Some("x11")));
        assert!(!super::is_wayland(None, None));
    }
}
```

- [ ] **Step 5: Write the live test**

Create `crates/pheme-input/tests/portal_live.rs`:

```rust
//! Exercises the real InputCapture portal. Ignored by default: it needs a Wayland
//! session with a compositor that implements the portal, and it raises a permission
//! dialog.
//!
//! Run with:
//!   cargo test -p pheme-input --test portal_live -- --ignored --nocapture
//!
//! It does NOT capture input — it stops before `Enable` has anything to trigger it,
//! so it cannot take the keyboard away from whoever is running it.

#![cfg(target_os = "linux")]

#[test]
#[ignore = "needs a Wayland session and raises a permission dialog"]
fn a_session_can_be_created_and_barriers_accepted() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        panic!("not a Wayland session");
    }
    let mut cap = pheme_input::portal::PortalCapture::new().expect("portal backend");
    let screens = pheme_input::InputCapture::screens(&cap);
    assert!(!screens.is_empty(), "wl_output reported no screens");

    let (tx, _rx) = crossbeam_channel::unbounded();
    pheme_input::InputCapture::start(&mut cap, tx).expect("session start");

    // The edge the design probe used. If the barrier convention is wrong the session
    // thread reports it here rather than sitting silent forever.
    let edges = [pheme_input::CaptureEdge { side: pheme_core::Side::Right, span: (0.0, 1.0) }];
    pheme_input::InputCapture::set_edges(&mut cap, &edges).expect("barriers accepted");

    pheme_input::InputCapture::stop(&mut cap);
}
```

This is the one automated check that the barrier geometry matches what a real compositor accepts. It is the difference between "the unit tests agree with the plan" and "the compositor agrees".

- [ ] **Step 6: Run everything**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: green, with `portal_live` reported as ignored.

Run: `cargo test -p pheme-input --test portal_live -- --ignored --nocapture`
Expected: PASS on the KDE machine, after accepting the permission dialog. If `set_edges` fails, the message names how many barriers the compositor rejected — go back to Task 4's geometry with that number.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -F - <<'MSG'
Update: select the Wayland backend before falling back to X11

A KDE Wayland session also sets DISPLAY for XWayland, so the previous
order connected to X11 successfully, installed XInput2 and XTest, and
captured nothing but XWayland clients -- with no error reported at any
layer. Wayland is now checked first, and the decision lives in
detect_capture rather than inside X11Capture::new, so there is one
place that chooses and X11Capture stays usable on its own.

A compositor without the portal gets an error that names the situation:
wlroots is not supported yet, and the machine can still be a client.

PortalCapture acknowledges set_mode and warp_cursor with nothing to do.
The compositor is already capturing when the core asks for a grab, and
an application cannot move an uncaptured pointer under Wayland; the
core only warps during abort_switch, where doing nothing is correct.

Adds an ignored live test against the real portal. It is the only
automated check that the barrier geometry is what a compositor actually
accepts rather than what the unit tests and the plan agree on. It stops
before anything can trigger a capture, so it cannot take the keyboard
away from whoever runs it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 10: The lock hotkey through GlobalShortcuts

**Files:**
- Create: `crates/pheme-input/src/portal/shortcuts.rs`
- Modify: `crates/pheme-input/src/portal/mod.rs`
- Modify: `crates/pheme-app/src/server.rs`

**Interfaces:**
- Produces: `portal::shortcuts::LockShortcut`, delivering a toggle over a channel.

A Wayland server receives no keyboard events while local, so the lock hotkey cannot be observed the way X11 observes it. Worse, it cannot be undone: locking removes nothing, but with the pointer never crossing there is no capture in which the key could be pressed again. A lock that cannot be released is worse than no lock.

`org.freedesktop.portal.GlobalShortcuts` is the only mechanism that exists. The compositor owns the final binding; the configured hotkey is only a `preferred_trigger`.

- [ ] **Step 1: Implement the shortcut session**

```rust
//! The lock hotkey on Wayland, through `org.freedesktop.portal.GlobalShortcuts`.
//!
//! A Wayland server sees no keyboard events while it is not capturing, so the X11
//! approach — watch every key and match one — cannot work. This portal is the only
//! alternative, and it comes with a behavioural difference: the compositor owns the
//! binding. `hotkeys.lock` in the configuration is a request, not a decision.

use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, CreateSessionOptions, GlobalShortcuts, NewShortcut,
};
use crossbeam_channel::Sender;
use futures_lite::StreamExt;
use tracing::{info, warn};

/// The shortcut id used in `BindShortcuts` and matched on `Activated`. The portal
/// reports the id back, and a session may hold several shortcuts, so it must match.
const LOCK_ID: &str = "lock";

pub struct LockShortcut {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: async_channel::Sender<()>,
}

impl LockShortcut {
    /// Binds the lock shortcut and sends `()` on `toggle` each time it fires.
    ///
    /// `preferred_trigger` uses the portal's shortcut syntax, for example
    /// `"CTRL+ALT+l"`. The compositor may bind something else entirely; what it
    /// chose comes back in the response and is logged, because the user's
    /// configuration file will not match what actually works.
    pub fn bind(preferred_trigger: String, toggle: Sender<()>) -> LockShortcut {
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("pheme-shortcut".into())
            .spawn(move || {
                futures_lite::future::block_on(run(preferred_trigger, toggle, stop_rx))
            })
            .ok();
        LockShortcut { thread, stop: stop_tx }
    }
}

impl Drop for LockShortcut {
    fn drop(&mut self) {
        let _ = self.stop.send_blocking(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn run(
    preferred_trigger: String,
    toggle: Sender<()>,
    stop: async_channel::Receiver<()>,
) {
    // Every failure here is logged and ends the thread. A lock hotkey that could not
    // be bound must never take keyboard and mouse sharing down with it.
    let portal = match GlobalShortcuts::new().await {
        Ok(p) => p,
        Err(e) => return warn!("no GlobalShortcuts portal; the lock hotkey is unavailable: {e}"),
    };
    let session = match portal.create_session(CreateSessionOptions::default()).await {
        Ok(s) => s,
        Err(e) => return warn!("could not create a GlobalShortcuts session: {e}"),
    };
    let shortcut = NewShortcut::new(LOCK_ID, "Lock input to the current screen")
        .preferred_trigger(preferred_trigger.as_str());
    let bound = match portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await
        .and_then(|r| r.response())
    {
        Ok(b) => b,
        Err(e) => return warn!("binding the lock hotkey failed: {e}"),
    };
    for s in bound.shortcuts() {
        // The trigger the desktop actually assigned, which may differ from the
        // configured one. Without this line a user whose compositor chose something
        // else has no way to find out what.
        info!(id = s.id(), trigger = s.trigger_description(), "lock hotkey bound");
    }

    let mut activated = match portal.receive_activated().await {
        Ok(s) => s,
        Err(e) => return warn!("subscribing to shortcut activations failed: {e}"),
    };
    loop {
        let go_on = futures_lite::future::or(
            async {
                let _ = stop.recv().await;
                false
            },
            async {
                match activated.next().await {
                    Some(a) => {
                        if a.shortcut_id() == LOCK_ID && toggle.send(()).is_err() {
                            return false;
                        }
                        true
                    }
                    None => false,
                }
            },
        )
        .await;
        if !go_on {
            break;
        }
    }
}
```

Follow the compiler if any signature here disagrees with `ashpd` 0.13; the module is behind the `global_shortcuts` feature added in Task 4.

- [ ] **Step 2: Wire it into the server**

In `crates/pheme-app/src/server.rs`, where the server starts, after `detect_capture()`: when the configuration names a lock hotkey **and** this is a Wayland session, bind the shortcut and spawn a task that, on each toggle, applies the same state change the hotkey path applies under X11.

`ServerCore` toggles its lock inside `on_event` when it sees the configured key. Rather than duplicate that, give the core an explicit entry point and have both paths use it:

```rust
    /// Toggles the input lock. The X11 and Windows backends reach this through the
    /// hotkey in `on_event`; on Wayland the GlobalShortcuts portal calls it directly,
    /// because a Wayland server sees no keys while local.
    pub fn toggle_lock(&mut self) -> Vec<Action> {
        self.locked = !self.locked;
        vec![Action::SetLocked(self.locked)]
    }
```

and rewrite the hotkey branch of `on_event` to call it, so there is one implementation.

- [ ] **Step 3: Test the core entry point**

```rust
#[test]
fn toggle_lock_is_the_same_state_the_hotkey_reaches() {
    // Whatever key the neighbouring lock tests in this module use.
    let hk = Hotkeys { lock: Some(KeyCode::LEFT_CTRL) };
    let mut a = ServerCore::new(layout(), hk);
    let mut b = ServerCore::new(layout(), hk);

    a.on_event(CaptureEvent::Key { code: hk.lock.unwrap(), down: true });
    b.toggle_lock();
    assert_eq!(a.locked(), b.locked());
    assert!(a.locked());

    a.on_event(CaptureEvent::Key { code: hk.lock.unwrap(), down: false });
    a.on_event(CaptureEvent::Key { code: hk.lock.unwrap(), down: true });
    b.toggle_lock();
    assert_eq!(a.locked(), b.locked());
    assert!(!a.locked(), "a lock that cannot be released is worse than no lock");
}
```

- [ ] **Step 4: Gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: bind the lock hotkey through the GlobalShortcuts portal

A Wayland server receives no keyboard events while it is not capturing,
so the X11 approach of watching every key cannot see the lock hotkey.
The failure is not symmetric: a lock taken this way could never be
released, because with the pointer never crossing there is no capture
in which the key could be pressed again.

GlobalShortcuts is the only mechanism that exists. It brings a real
behavioural difference -- the compositor owns the binding, and the
configured hotkey is only a preferred_trigger -- so the trigger the
compositor actually chose is logged.

ServerCore gains toggle_lock and the hotkey branch of on_event now
calls it, so the portal path and the key path cannot drift apart.

A bind failure is logged and the server keeps running: a missing lock
hotkey must never take the keyboard and mouse down with it.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 11: Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/testing.md`

- [ ] **Step 1: README**

Add a Wayland section covering: which compositors work (KDE, GNOME — those implementing the InputCapture portal) and which do not yet (Hyprland, Sway and other wlroots compositors, with the note that such a machine can still be a client); that a permission dialog appears when the server starts, and may appear on every start because KDE returns no restore token; and that on Wayland the lock hotkey binding is owned by the desktop, so `hotkeys.lock` is a request the compositor may answer with a different key.

- [ ] **Step 2: `docs/testing.md`**

Add a "Wayland capture (sub-project 4)" section with rows W1–W11 exactly as the spec's §14 lists them.

- [ ] **Step 3: Gate and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

```bash
git add -A
git commit -F - <<'MSG'
Update: document Wayland capture and add its manual test rows

Records which compositors work today and which do not, that a
permission dialog appears when a Wayland server starts and may appear
on every start because KDE returns no restore token, and that the lock
hotkey binding belongs to the desktop rather than to the configuration
file.

Adds rows W1 to W11 to the manual matrix. W9 is the open question the
spec flags: whether the permission survives a restart.

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## After the last task

Rows W1–W11 in `docs/testing.md` have not been run. They are the only check on anything the portal actually does — the unit tests pin the translations, and the live test pins the barrier geometry, but session lifecycle, a real crossing, a real return and the lock hotkey are all manual. W2 (Shift held across the edge), W3 (the compositor ending a capture) and W9 (whether the permission survives a restart) each cover a failure no counter and no test in this plan can see.

Rows A1–A11 from sub-project 2 and M1–M10 from sub-project 3 also remain unverified on hardware.
