# Sub-project 4 — Wayland capture (server side)

Date: 2026-09-25
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-23-virtual-mic-design.md`
Expected outcome: a machine running a Wayland session can be a Pheme
**server**. The pointer crosses the configured screen edge onto the
client exactly as it does under X11, keyboard and mouse follow, and the
lock hotkey still works. Today `detect_capture()` refuses outright on
Wayland and tells the user to switch to an X11 session.

## 1. Scope

In:

- A third capture backend, `linux_portal`, built on
  `org.freedesktop.portal.InputCapture` plus libei.
- The lock hotkey on Wayland, through
  `org.freedesktop.portal.GlobalShortcuts`.
- The trait change the portal's event model forces on every backend
  (§4.1): `Action::Ungrab` gains the position, `set_mode`/`CaptureMode`
  give way to `grab()` and `release(x, y)`, and `InputCapture` gains
  `set_barriers()`.
- Backend selection that prefers Wayland over the XWayland `DISPLAY`
  that is also present in a Wayland session.
- `ServerCore::toggle_lock()`, so the lock has one entry point rather
  than two.

Out:

- **wlroots compositors (Hyprland, Sway, river)**, which have no
  InputCapture portal. They need an entirely different backend —
  `zwlr_virtual_pointer`, `zwp_pointer_constraints`, a layer surface —
  with no shared code beyond the event mapping. §12 records the design
  so it does not have to be rediscovered. On those compositors
  `detect_capture()` returns a clear error naming that sub-project.
- The client side. A Linux client injects through `uinput`, which is a
  kernel interface and does not care which display server is running;
  Wayland clients already work and this sub-project does not touch them.
- Touchscreen capture. The portal offers it (`SupportedCapabilities`
  reports it on KDE) and Pheme's protocol has no touch events.
- macOS, and X11-session behaviour, which is unchanged.

## 2. Measured facts

Everything in this section was measured on the development machine —
CachyOS, KDE Plasma, `xdg-desktop-portal-kde` 6.7.5, `libei` 1.6.0 —
with throwaway probes. None of it is quoted from documentation, and
three of the five contradict the obvious assumption. Each one has a test
in §8 that fails if the fact stops holding.

### 2.1 The portal is version 2 and grants what we need

`org.freedesktop.portal.InputCapture` reports `version = 2` and
`SupportedCapabilities = 7` (Keyboard | Pointer | Touchscreen).
`CreateSession2` exists, so the deprecated `CreateSession` path is not
needed. `Start` grants `Keyboard | Pointer` as requested.

### 2.2 Barrier geometry mixes two coordinate conventions

This is the fact that cost the first probe its entire run: a rejected
barrier never fires, and nothing in the protocol says why.
`SetPointerBarriers` returns `failed_barriers`, and it must be checked.

For a zone region at `(x0, y0)` of size `w × h`:

| Edge | Accepted | Rejected |
|---|---|---|
| Right | `(x0+w, y0) – (x0+w, y0+h-1)` | `(x0+w-1, …)`, `… – (x0+w, y0+h)` |
| Left | `(x0, y0) – (x0, y0+h-1)` | `… – (x0, y0+h)` |
| Top | `(x0, y0) – (x0+w-1, y0)` | `… – (x0+w, y0)` |
| Bottom | `(x0, y0+h) – (x0+w-1, y0+h)` | `(…, y0+h-1)` |

The axis **perpendicular** to the barrier uses a boundary convention —
the left edge is `x0`, the right edge is `x0 + w`, not `x0 + w - 1`. The
axis **parallel** to the barrier uses a pixel convention — `y0` to
`y0 + h - 1` inclusive.

Measured on a 2560×1440 region at the origin: `(2560,0)-(2560,1439)` is
accepted; `(2559,0)-(2559,1439)` and `(2560,0)-(2560,1440)` are both
rejected.

### 2.3 A barrier must span its whole edge

`(2560,0)-(2560,1438)` — one pixel short — is rejected, as are
`(2560,0)-(2560,719)`, `(2560,720)-(2560,1439)` and
`(2560,360)-(2560,1080)`. A barrier outside the zone, `(2561,…)`, is
rejected. Point order does not matter: `(2560,1439)-(2560,0)` is
accepted.

So `ClientPlacement.span`, which places a client against *part* of an
edge, cannot be expressed as a pointer barrier. §5.3 says what happens
instead.

Submitting several barriers in one call reports failures per id: three
barriers, of which one was malformed, returned exactly that one id.

### 2.4 `Activated` reports a position outside the screen

`Activated` carries `barrier_id`, `activation_id` and `cursor_position`.
On a 2560-wide screen, two runs reported `x = 2563` and `x = 2561`, the
overshoot varying with pointer speed. The position must be clamped into
the server rect before it is used, or the synthesised crossing (§5.2)
lands outside `on_edge()` and silently matches nothing.

### 2.5 libei delivers evdev codes, not xkb codes

The capture keyboard advertises an **Xkb** keymap, which invites the
assumption that its key codes are xkb codes. They are not. Holding Shift
and pressing `c` produced `key=42` and `key=46` — `KEY_LEFTSHIFT` and
`KEY_C` in `linux/input-event-codes.h`. The xkb codes would have been 50
and 54.

The X11 backend subtracts 8 to get from an X11 keycode to an evdev code.
**The portal backend must not.** Applying the X11 rule here shifts the
entire keyboard by one row.

Buttons are in the same space: `272` and `273` are `BTN_LEFT` and
`BTN_RIGHT`, which is what `uinput` already uses on the client.

Scrolling produces `ScrollDiscrete` with `dy = ±120` per notch. Pheme's
`InputInject::wheel` is already documented in units of 1/120 of a notch,
so the value passes through unchanged.

`KeyboardModifiers` arrives once when capture opens and again after
every change, with `depressed = 0x1` while Shift is held — the standard
xkb mask, where bit 0 is Shift, 2 is Control and 3 is Alt.

### 2.6 KDE does not remember the permission

`Start` was called three times with
`PersistMode::ExplicitlyRevoked`. Every call returned
`restore_token = None`, and every call blocked on a permission dialog —
over 16 s the first time, and still blocked after 12 s on the third.

Pheme creates one session for the lifetime of the server process, so
this is one dialog per server start, not one per client connection. It
is still a real limitation: a Pheme server cannot start silently at
login on KDE. §11 records it; the README must say it.

The code still requests a restore token and still passes one back when
it has it, because GNOME may behave differently and the cost is two
lines.

## 3. What the portal changes about capture

The X11 backend watches the pointer continuously and lets `ServerCore`
decide when it has crossed an edge. `on_local_event` compares the
current position against `last_pos` to tell a crossing from a slide
along the edge.

The portal inverts this. While capture is inactive we receive **no
events at all**. Instead we declare pointer barriers up front, the
compositor watches them, and it tells us after the fact that the pointer
hit one. Coming back is not a warp — there is no way to move the pointer
on Wayland — but a `Release` call carrying the position we want it left
at.

Two consequences run through the whole design:

- **The backend needs the layout.** `pheme-input` knows nothing about
  clients today. It cannot place barriers without knowing which edges
  have a client on them.
- **Nothing observes the keyboard while local.** The lock hotkey cannot
  be seen through the capture path, and a lock that can be set but not
  cleared is worse than no lock at all: with the pointer locked the
  barriers come down, so nothing would ever wake capture again. §6
  resolves this.

## 4. Interface changes

### 4.1 `Action::Ungrab` carries the position

`ServerCore` emits `Action::Ungrab` and, always, immediately after it,
`Action::WarpCursor` with the position the pointer should return to —
in `on_remote_event`'s leaving path and in `client_disconnected` alike.
The X11 backend ungrabs and then warps. The portal backend has a single
call that does both.

Leaving the pair implicit would make the *order of two lines in the core*
load-bearing for a backend in another crate: reorder them, or drop the
warp, and Wayland stops working while every test stays green. That is
the defect shape that cost sub-project 3 six rounds.

So:

```rust
pub enum Action {
    // ...
    Ungrab { x: i32, y: i32 },
    WarpCursor { x: i32, y: i32 },
}
```

`Action::WarpCursor` remains, because `abort_switch()` warps without
ungrabbing.

The trait changes to match. `set_mode(CaptureMode)` and `CaptureMode`
itself are removed: once `Ungrab` carries a position, the only value
ever passed would be `CaptureMode::Grab`, and an enum with a variant
nobody constructs is an invitation to reintroduce the second, positionless
way to stop capturing — which is exactly the one the portal backend
cannot implement.

```rust
pub trait InputCapture: Send {
    // ...
    /// Starts swallowing input: hides and confines the cursor and
    /// reports relative motion. **Synchronous**: it returns only after
    /// the backend has grabbed or failed to, and on `Err` the backend
    /// is still observing. Backends that hand the request to their own
    /// thread wait for that thread's acknowledgement with a 1 s
    /// internal timeout, mapping a timeout to
    /// `Error::Backend("mode change timed out")`.
    fn grab(&mut self) -> Result<()>;

    /// Stops capturing and leaves the pointer at `(x, y)` in server
    /// coordinates. **Synchronous**, same contract: it returns only
    /// after the backend has released, and on `Err` the backend is
    /// still capturing.
    fn release(&mut self, x: i32, y: i32) -> Result<()>;
}
```

X11 and Windows implement `release` as their existing ungrab followed by
their existing warp — the two calls `pheme-app` makes today, moved down
one layer, where the ordering is a single function body instead of two
lines of a match arm in another crate. The portal implements it as
`Release { activation_id, cursor_position }`.

### 4.2 `set_barriers`

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarrierSpec {
    pub side: Side,
    pub span: (f32, f32),
}

pub trait InputCapture: Send {
    // ...
    /// Declares the edges that should hand input over. Backends that
    /// detect crossings themselves ignore it. Called whenever the set
    /// of connected clients, the layout, or the lock state changes; an
    /// empty slice means "capture nothing". An `Err` means the
    /// compositor rejected a barrier (§2.2) and that edge will never
    /// fire — never a silent no-op.
    fn set_barriers(&mut self, _barriers: &[BarrierSpec]) -> Result<()> {
        Ok(())
    }
}
```

X11, Windows and the mock take the default. `pheme-app`'s server calls
it on client connect, client disconnect, and lock change.

### 4.3 `ServerCore::toggle_lock()`

```rust
impl ServerCore {
    /// Toggles the pointer lock from outside the capture stream, for
    /// backends whose lock hotkey arrives through a different channel.
    pub fn toggle_lock(&mut self) -> Vec<Action>;
}
```

Same effect as the hotkey path in `on_event`: flip `locked`, return
`vec![Action::SetLocked(locked)]`.

## 5. The portal backend

### 5.1 Threads and lifetime

`reis`'s `EiConvertEventIterator` is **not `Send`**: it must be created
on the thread that consumes it. Only the `OwnedFd` from `ConnectToEIS`
crosses a channel. So the backend runs two threads:

- **Portal thread.** Owns the `ashpd` session and runs
  `futures_lite::future::block_on` over an async-io executor: creates
  the session, starts it, reads zones, sets barriers, connects to EIS,
  enables capture, and then serves `Activated` / `Deactivated` /
  `Disabled` / `ZonesChanged` and the commands from `set_barriers`,
  `grab` and `release`.
- **EIS thread.** Takes the fd, runs `handshake_blocking`, binds seat
  capabilities, and iterates. It translates each event and pushes it on
  the `Sender<CaptureEvent>`.

`pheme-input` gains no `tokio` dependency. The app runs a tokio runtime,
and blocking on a nested tokio runtime panics; async-io has no such
trap.

**Binding seat capabilities requires a flush.** `bind_capabilities`
queues a request; without `Connection::flush()` it never leaves the
buffer and no device is ever created. The probe that omitted it received
`SeatAdded` and then nothing at all for ten seconds — no error, no
device, no events.

`stop()` shuts the EIS socket down (`shutdown(Both)` on a duplicated
raw fd), which makes the blocking `poll` return, `read` report EOF, and
the iterator end. The portal thread is stopped by dropping the session.

### 5.2 Activation

On `Activated`:

1. Clamp `cursor_position` into the server rect (§2.4).
2. Map `barrier_id` to the `BarrierSpec` that produced it.
3. Test the clamped position against that spec's span, using
   `pheme_core::geometry::{edge_segment, on_edge}` — the same functions
   `ServerCore` uses, not a reimplementation.
4. If it is outside the span, `Release` immediately at the clamped
   position and emit nothing.
5. Otherwise record `activation_id` and emit
   `CaptureEvent::MotionAbs` at the clamped position.

`ServerCore::on_local_event` then takes its ordinary path: the position
is on the edge, inside the segment, and `last_pos` holds the interior
point where the previous return left the pointer — so the
slide-along-the-edge rule passes rather than being circumvented.

Doing the span test in the backend, rather than emitting the event and
waiting to see whether a `Grab` comes back, is what removes the need for
a confirmation timeout. A timeout would also have been racy: EIS events
still queued from the previous capture could be processed after
`Activated` set a pending flag and before the synthesised event was
dequeued, cancelling an activation that was in fact valid.

`grab()` after an activation is an acknowledgement and does not call the
portal: the compositor is already capturing. It fails only if there is no
activation to acknowledge.

### 5.3 Barriers and `span`

For each `BarrierSpec`, place a **full-edge** barrier on every zone
region whose edge the span overlaps, using the geometry in §2.2. Keep
the spec alongside the barrier id for the §5.2 test.

Where a span covers only part of a monitor's edge, the barrier still
covers the whole edge and the §5.2 test rejects the rest. Crossing at a
point with no client behind it therefore stops the pointer for one
activation round trip before it is released — a brief, visible hitch.
The same configuration file works on X11 and on Wayland, and the
behaviour is identical apart from that hitch.

Zones are the **single source of geometry** for this backend, including
`screens()`. `wayland_screens()` and the portal's zones are two
independent views of the same monitors; mixing them would mean the core
computes an exit position in one space and `Release` interprets it in
another. `ZonesChanged` re-reads the zones and re-places the barriers.

### 5.4 Event mapping

| From | To |
|---|---|
| `Activated` | `MotionAbs` at the clamped position (§5.2) |
| `PointerMotion { dx, dy }` | `MotionRel` (rounded to i32) |
| `Button { button, state }` | `Button` — code passed through |
| `ScrollDiscrete { dx, dy }` | `Wheel` — value passed through |
| `ScrollDelta` | ignored; `ScrollDiscrete` carries the notches |
| `KeyboardKey { key, state }` | `Key` — **evdev code, no offset** |
| `KeyboardModifiers { depressed }` | seeds the held-modifier set |
| `Deactivated` | ignored; the core drives leaving |
| `Disabled` | backend stops and reports through `tx` closing |

`KeyboardModifiers` arriving while capture opens is what lets a
crossing made with Shift held reach the client as Shift-held: the
backend turns the depressed mask into synthetic key-down events for the
matching modifier keys, emitted before the `MotionAbs`, so `ServerCore`
builds the right `Modifiers` for `Msg::Enter`. Left-hand variants are
used, since the mask does not distinguish sides.

### 5.5 Backend selection

```rust
if std::env::var_os("WAYLAND_DISPLAY").is_some() {
    // portal backend; on a missing InputCapture interface,
    // Error::Unsupported naming the wlroots sub-project
} else if std::env::var_os("DISPLAY").is_some() {
    // X11 backend
}
```

The order matters. A Wayland session also sets `DISPLAY`, for XWayland.
Taking the X11 branch there produces a backend that connects, grabs and
reports success while seeing only XWayland clients — a failure with no
error message.

## 6. The lock hotkey

On Wayland, `Hotkeys.lock` is **not** passed to `ServerCore`. The lock
is bound through `org.freedesktop.portal.GlobalShortcuts`, whose
`Activated` signal calls `ServerCore::toggle_lock()`.

Routing it through both paths would double-toggle: while captured, the
key also arrives over EIS, and `on_event` would act on it a second time.
One path, and it works identically whether the pointer is local or
remote.

While locked and local, the backend is given an empty barrier list, so
the compositor never captures. This is why the lock needs a channel
outside the capture path at all.

The portal owns the binding. Pheme passes the configured key as
`preferred_trigger`; the desktop may show a dialog and may bind
something else. The README must say that on Wayland the lock shortcut is
configured through the desktop's own shortcut settings, unlike X11 and
Windows where `config.toml` decides.

## 7. Dependencies and MSRV

`pheme-input`, Linux targets only:

```toml
ashpd = { version = "0.13", default-features = false, features = [
    "async-io", "input_capture", "global_shortcuts",
] }
reis = "0.7"
futures-lite = "2"
```

`ashpd`'s per-portal features keep the dependency to the two portals
used. `default-features = false` drops its tokio default, which would
otherwise pull a second runtime into a crate that has none.

**`ashpd` 0.13 requires Rust 1.87.** The workspace `rust-version` is
1.85 and must be raised to 1.87. This is a Global Constraint for the
plan: every task inherits it.

## 8. Testing

Unit tests, run everywhere:

- `Action::Ungrab { x, y }` round-trips through the core's leaving path
  and through `client_disconnected`, asserting the coordinates, not just
  the variant. The coordinates must be the ones the old `WarpCursor`
  carried, so the X11 behaviour is provably unchanged.
- `toggle_lock()` flips `locked` and returns `SetLocked`, and a locked
  core refuses to switch.
- Barrier geometry: `BarrierSpec` → portal rectangle for all four sides
  on a single region and on two side-by-side regions, asserting the
  mixed convention of §2.2 exactly — including that the right edge is
  `x0 + w` and the parallel axis ends at `h - 1`.
- The §5.2 span test accepts a position inside the span and rejects one
  outside, on each side.
- Key mapping: evdev `42` maps to the HID code for Left Shift **and**
  the test fails if 8 is subtracted first. A test that only asserts the
  happy path would pass under the X11 rule too.
- Modifier mask `0x1` produces a Left Shift key-down; `0x5` produces
  Left Shift and Left Control.
- `cursor_position` beyond the screen is clamped: `(2563.0, 163.0)` on a
  2560-wide rect becomes `x = 2559`, and the test asserts the clamped
  value lies on the edge according to `on_edge()`.

Tests that need a portal, and so run only where one exists — behind a
feature flag, not in CI:

- A session starts, barriers are accepted (`failed_barriers` empty), and
  `ZonesChanged` re-places them.
- A rejected barrier is reported as an error rather than ignored.

CI runs `cargo fmt`, `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo test --workspace` on Ubuntu and Windows, as before.
The portal backend compiles on Linux only.

## 9. Manual test matrix (added to `docs/testing.md`)

Run with this machine (KDE Wayland) as the server and the QEMU VM as the
Windows client, and again Linux Wayland → Linux.

| # | Check |
|---|---|
| W1 | Pointer crosses the configured edge onto the client and back; ten round trips without losing the pointer |
| W2 | Keyboard follows: 100 keystrokes on the client, no stuck key |
| W3 | **Hold Shift while crossing** — the client receives Shift-held, not lowercase (§5.4) |
| W4 | Mouse buttons and scrolling work on the client, scroll direction and speed match X11 |
| W5 | Lock hotkey toggles from the desktop's shortcut binding, both while local and while remote, and unlocks again |
| W6 | While locked and local, the pointer does not cross |
| W7 | Crossing at a point of the edge with **no client behind it** releases immediately and leaves the pointer usable (§5.3) |
| W8 | Unplug the network: the server regains its pointer within 5 s |
| W9 | Change the monitor layout mid-session: barriers follow (`ZonesChanged`) |
| W10 | A second monitor: crossing works on the outer edge of the correct monitor |
| W11 | Input latency over cable is indistinguishable from the X11 backend |
| W12 | Audio out and the virtual mic keep working with the Wayland backend active |

## 10. Definition of done

- A KDE Wayland session runs `pheme server` and a client takes the
  pointer and keyboard across the configured edge.
- `detect_capture()` picks the portal backend on a Wayland session even
  though `DISPLAY` is set, and the X11 backend on an X11 session.
- On a compositor with no InputCapture portal, the error names the
  deferred sub-project rather than failing obscurely.
- A barrier rejected by the compositor is an error, not a silent
  no-capture.
- The lock hotkey works on Wayland through GlobalShortcuts.
- X11 and Windows behaviour is unchanged; their tests still pass
  untouched except for the `Ungrab`/`release` rename.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace` green on Ubuntu and Windows.
- README documents: Wayland server support, the per-start permission
  dialog, the lock shortcut being desktop-configured, and wlroots not
  being supported yet.

## 11. Known risks

- **A permission dialog on every server start (§2.6).** Measured on KDE,
  three times, with no restore token. GNOME is untested and may persist.
  This rules out starting a Pheme server silently at login on KDE.
- **GNOME is untested.** The development machine runs KDE. GNOME's
  mutter implements InputCapture, but every fact in §2 was measured
  against KWin, and §2.2's coordinate convention in particular is the
  kind of thing two implementations can disagree about. The barrier
  geometry test in §8 is a unit test of our arithmetic, not of the
  compositor's acceptance; only W1 on a GNOME machine settles it. This
  is the same shape of risk that let three defects reach the VM in
  sub-project 2.
- **The span hitch (§5.3).** Crossing an edge where the configuration
  places no client stops the pointer for one round trip. Correct, and
  surprising the first time it is seen.
- **`Deactivated` is ignored.** The compositor can end capture on its
  own — a session switch, a compositor restart. The core would then
  believe it is still remote. Handling it properly means teaching the
  core that capture can be lost; this sub-project logs it and lets the
  next `Activated` re-synchronise, which is wrong for the window
  between. Recorded rather than hidden.
- **Clipboard and drag-and-drop across the edge do not work**, on any
  backend. Sub-project 5.

## 12. Deferred: wlroots compositors

Recorded so the analysis does not have to be redone.

Hyprland, Sway and river have no InputCapture portal —
`xdg-desktop-portal-hyprland` and `-wlr` implement ScreenCast and
RemoteDesktop, not InputCapture. There is no user-mode way to ask those
compositors to hand over input at a barrier.

The working approach, the one `lan-mouse` uses, is to do by hand what
the portal does:

- A `zwlr_layer_shell_v1` surface one pixel wide along the edge, in the
  overlay layer, with an input region covering it. The pointer entering
  that surface is the crossing event.
- `zwp_pointer_constraints_v1` to lock the pointer once it has crossed,
  and `zwp_relative_pointer_v1` for the motion deltas.
- `wlr_virtual_pointer` is not needed on the server side; it is the
  injection half.

Nothing is shared with the portal backend except the event mapping in
§5.4 and the geometry in §4.2. It is a second backend, not a branch in
this one, which is why it is a sub-project rather than a task.

It also cannot be verified here: the development machine runs KDE, and
shipping a wlroots backend from this machine would repeat sub-project
2's mistake of shipping platform code that was compiled but never run.
