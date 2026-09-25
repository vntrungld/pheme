# Sub-project 4 — Wayland capture (server side)

Date: 2026-09-24
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-23-virtual-mic-design.md`
Expected outcome: a machine running a Wayland session can be a Pheme
server. Moving the pointer across the configured screen edge hands
keyboard and mouse to the client exactly as it does under X11, and
moving back returns them. Nothing about the client changes.

## 1. Scope

In:

- A third capture backend, `linux_portal`, built on
  `org.freedesktop.portal.InputCapture` and libei, covering the
  compositors that implement that portal — KDE and GNOME.
- The trait and core changes that model requires (§4). They touch the
  X11 and Windows backends, so they are part of this sub-project rather
  than a later cleanup.
- The lock hotkey on Wayland through `org.freedesktop.portal.GlobalShortcuts`,
  because a Wayland server receives no keyboard events at all while it
  is not capturing (§7).
- Backend selection that prefers Wayland over XWayland (§8).

Out:

- **wlroots compositors — Hyprland, Sway (§16).** They do not implement
  the InputCapture portal. Supporting them means a second, unrelated
  backend built on `zwlr_virtual_pointer`, `zwp_pointer_constraints` and
  a layer-shell surface. It is roughly the size of this whole
  sub-project, and no machine available to this project runs one, so it
  would ship having never been executed — the mistake sub-project 2 made
  with Windows, which cost three defects found only in a VM. §16 records
  the analysis so it does not have to be redone.
- Injection on Wayland. A Linux client injects through `uinput`, which
  is a kernel interface and works identically under X11 and Wayland.
  Nothing in the client changes.
- macOS, clipboard, mDNS, tray.

## 2. Why the portal does not fit the current model

Under X11 the backend watches the pointer continuously and `ServerCore`
decides when an edge has been crossed. `on_local_event` needs that
stream: it compares the current position against `last_pos` to tell a
crossing from a slide along the edge.

The portal inverts this. The application declares **pointer barriers**
up front; the compositor watches them and emits `Activated` when one is
crossed. Until that happens the application receives **no input events
of any kind** — no pointer position, no keys. Afterwards events arrive
over a libei socket, and the return trip is a `Release` call carrying a
suggested cursor position, not a warp.

Two consequences drive the whole design:

- `pheme-input` has never needed to know where clients are. Now it does,
  because barriers have to be placed on the edges the clients are on.
- The backend cannot observe anything while local, so the lock hotkey
  and the modifier state at the moment of crossing both need their own
  mechanisms.

## 3. Measurements

Everything below was measured on this project's KDE machine
(`xdg-desktop-portal-kde` 6.7.5, `libei` 1.6.0) with a throwaway probe,
before any of this design was written. The numbers are recorded because
four of them contradict what the obvious implementation would assume.

| Question | Measured |
|---|---|
| Portal version | 2 — `CreateSession2` present |
| `SupportedCapabilities` | `Keyboard \| Pointer \| Touchscreen` |
| Capabilities actually granted | `Keyboard \| Pointer` — Touchscreen requested and refused |
| Barrier coordinates | see below |
| `Activated.cursor_position` | **outside the zone** — see below |
| Button codes | `272` = `BTN_LEFT`: raw evdev |
| Key codes | `42` = `KEY_LEFTSHIFT`, `30` = `KEY_A`: **raw evdev, not XKB** |
| Discrete scroll | `120` units per notch |
| Modifiers held before capture | reported: `depressed = 0x1` while Shift was held across the barrier |
| `restore_token` | `None`, even with `PersistMode::ExplicitlyRevoked` |

### 3.1 Barrier coordinates

The portal specification states that a barrier sits on the top edge (for
horizontal) or left edge (for vertical) of its pixels, and must lie on
the outer boundary of the union of all zones. The far edge of a zone of
width `W` at `x0` is therefore `x0 + W`, **not** `x0 + W - 1`, while the
extent *along* the edge stops at the last pixel, `x0 + W - 1`.

For the single 2560x1440 zone at (0,0):

```
left    x1 = x2 = 0        y1 = 0     y2 = 1439
right   x1 = x2 = 2560     y1 = 0     y2 = 1439
top     y1 = y2 = 0        x1 = 0     x2 = 2559
bottom  y1 = y2 = 1440     x1 = 0     x2 = 2559
```

All four are accepted (`failed_barriers = []`). A barrier at `x = 2559`
— the intuitive "last pixel of the right edge" — is **rejected**. The
rejection is reported only in `failed_barriers`; the call itself
succeeds, `Enable` succeeds, and the session then sits forever without
ever activating. This failure is completely silent unless
`failed_barriers` is checked.

A third rule, measured later than the two above and missed by the first
implementation: **a barrier must span its zone's whole edge.** Measured
by feeding `portal::geometry::barriers`' own output to KWin:

```
span (0.0, 1.0)   -> (2560,0)-(2560,1439)     accepted
span (0.25, 0.75) -> (2560,360)-(2560,1079)   rejected
span (0.0, 0.5)   -> (2560,0)-(2560,719)      rejected
span (0.5, 1.0)   -> (2560,720)-(2560,1439)   rejected
```

A barrier one pixel short of the full edge, `(2560,0)-(2560,1438)`, is
rejected too, so this is not an off-by-one in the endpoint convention —
partial edges are simply not expressible. `ClientPlacement.span`
therefore cannot be carried by the barrier. §5.3 says what carries it
instead.

### 3.2 The activation position lies outside the zone

`Activated` reports where the pointer *would* have gone had the barrier
not stopped it, not where the pointer is. Three measurements on a
2560-wide screen: **2577, 2564, 2560**. The overshoot varies with
pointer speed and is never less than the width.

`on_edge()` in `pheme-core` accepts `0..=2559`. Feeding the reported
position straight into `MotionAbs` therefore matches no edge, the core
returns no actions, and the pointer simply stops at the screen edge —
no error, no log, no switch. The backend **must** clamp the position
into the server rect before it reaches the core.

### 3.3 Key codes are evdev, not XKB

The X11 backend converts with `keycode - 8`, because X11 keycodes are
evdev codes offset by 8. libei delivers evdev codes directly, confirmed
twice independently: `42` while Shift was held (`KEY_LEFTSHIFT` = 42 in
evdev; 42 in XKB space would be `KEY_G`), and `30` for the letter A
(`KEY_A` = 30 in evdev; 30 in XKB space would be `KEY_Y`).

Assuming symmetry with the X11 path would shift every key by eight
positions. The keyboard device does carry an XKB keymap, which makes the
wrong assumption look justified.

### 3.4 `bind_capabilities` does nothing without a flush

On `SeatAdded` the application binds the capabilities it wants. The
request is buffered; without an explicit `Connection::flush()` it never
reaches the compositor, no devices are created, and not one input event
ever arrives. The probe reproduced this exactly: `SeatAdded` and then
silence. With the flush, both devices appear immediately.

There is no error and no diagnostic. A backend missing this line is
indistinguishable from a compositor that refuses to capture.

## 4. Trait and core changes

### 4.1 `Action::Ungrab` carries the position

`ServerCore` emits `Action::Ungrab` and `Action::WarpCursor { x, y }` as
a pair, always adjacent and always with the same coordinates — on the
path that leaves a client, and on the path where a client disconnects.
Under X11 the backend ungrabs and then warps. Under the portal there is
no warp: the position is an argument of `Release`.

Rather than leave the backend depending on the core emitting two actions
in a particular order — a coupling that no test would notice being
broken — the pair becomes one action:

```rust
Action::Ungrab { x: i32, y: i32 }   // stop capturing; put the pointer here
```

`Action::WarpCursor` remains, used by `abort_switch()`, which warps
without ungrabbing.

The trait gains the matching method:

```rust
/// Stops capturing and places the pointer at (x, y). Synchronous, with the
/// same acknowledgement and 1 s timeout rule as `set_mode`.
fn release(&mut self, x: i32, y: i32) -> Result<()>;
```

X11 implements it as its existing ungrab followed by its existing warp.
Windows and `mock` implement it the same way. The portal backend
implements it as one `Release` call.

### 4.2 Barriers reach the backend

```rust
/// An edge a crossing may start a capture on. Named `CaptureEdge` rather than
/// `Barrier` because the portal backend imports ashpd's `Barrier` in the same file.
pub struct CaptureEdge { pub side: Side, pub span: (f32, f32) }

/// Declares the edges on which a crossing should start a capture. Backends
/// that detect crossings by observing the pointer ignore this. Default: no-op.
fn set_edges(&mut self, edges: &[CaptureEdge]) -> Result<()> { let _ = edges; Ok(()) }
```

A `CaptureEdge`'s `span` selects which *zones* take part — a span
covering the left half of a two-monitor union puts a barrier on the left
monitor only — but never narrows the barrier within a zone, which §3.1
shows the compositor rejects. The span itself is applied exactly, by the
core, when the activation arrives (§5.3).

`pheme-app`'s server calls it whenever the set of connected clients
changes, with one `CaptureEdge` per placement that currently has a
connected client — and whenever the lock changes, because locking
removes the barriers (§7): while locked the set is empty, and unlocking
puts it back. Barriers are **not** declared for edges without a connected
client: the compositor would stop the pointer at those edges, and the
core would then decline the switch, so the pointer would snag on every
screen edge for no reason.

### 4.3 `ServerCore::release_remote()`

The compositor can end a capture on its own — the portal specification
allows an implementation to deactivate at any time, and KDE binds an
escape combination for it. When that happens the core still believes it
is `Remote` and the client still believes it holds the input, so keys
held at that moment stay held on the client.

```rust
/// Returns to Local because capture ended without the pointer leaving the
/// client. Sends `Leave` so the client releases what it holds. No-op when
/// already Local.
pub fn release_remote(&mut self) -> Vec<Action>;
```

It emits `Msg::Leave` followed by `Action::Ungrab { x, y }` at the server
centre. This differs from `client_disconnected`, which sends no `Leave`
because there is nobody left to receive it.

## 5. The portal backend

### 5.1 One thread, one executor

`reis` exposes `async_io::EiConvertEventStream`, and `ashpd` can be built
on the same `async-io` reactor. The backend is therefore a single thread
running `block_on` over a select of four sources:

- portal signals: `Activated`, `Deactivated`, `Disabled`, `ZonesChanged`,
  `Session::Closed`
- the libei event stream
- control requests from `set_edges`, `set_mode`, `release`
- the stop signal

`ei::Context` is not `Send`. It is created and dropped on this thread and
never crosses one.

`stop()` signals and waits for the thread to exit, then returns. It does
not rely on the compositor closing the libei socket: the trait requires
every `Sender` clone to be dropped before `stop()` returns, and a
backend that waits on a remote party to notice it has gone is a hang.

### 5.2 Session lifecycle

```
CreateSession2 → Start(Keyboard|Pointer, persist) → GetZones
  → SetPointerBarriers(barriers, zone_set) → ConnectToEIS
  → ei handshake → bind capabilities → flush  (§3.4)
  → Enable
```

`SetPointerBarriers` suspends the session, so `Enable` must follow every
call to it, including the ones triggered by a client connecting later.

`ZonesChanged` invalidates `zone_set`; barriers must be recomputed from a
fresh `GetZones` and set again. A stale `zone_set` makes every barrier
fail.

`Disabled` means no further capture will occur until `Enable` is called
again. `Session::Closed` is terminal: the backend reports
`Error::Backend` and stops.

### 5.3 Activation

```
Activated { barrier_id, cursor_position, activation_id }
  store activation_id                          // Release needs it (§5.4)
  (x, y) = clamp(cursor_position, server_rect) // §3.2 — required, not defensive
  for each modifier in depressed mask:
      emit CaptureEvent::Key { code, down: true }
  emit CaptureEvent::CaptureActivated { x, y }
```

`CaptureActivated` is a portal-only event and it is deliberately not a
`MotionAbs`. It puts the core on its ordinary `on_local_event` path, but
it also **requires an answer**: `ServerCore::on_event` returns either the
switch (`Grab`, `WarpCursor`, `Enter`) or, when the local path produced
no `Grab`, an `Action::Ungrab { x, y }` that releases the capture.

This is also where `ClientPlacement.span` is applied. Because a barrier
covers its zone's whole edge (§3.1), an activation can arrive from a
stretch of edge the configuration puts no client behind; the core finds
no placement whose `EdgeSegment` contains it and declines, and the
release below returns the pointer. The span stays exact — only the
barrier is coarse — at the cost of one round trip's hesitation when the
pointer crosses where no client is.

Silence is not an available answer, and this is the correction to the
first draft of this section, which assumed it was. Under X11 and Windows
a declined `MotionAbs` costs nothing, because the backend was only
watching. Under the portal the compositor is *already capturing* by the
time the event arrives: the core sitting in `Local` discards `MotionRel`,
`Key`, `Button` and `Wheel`, the pointer stays parked and hidden, and
only the compositor's own escape binding — which KDE has and GNOME is
untested for — can end it. The draft's claim that "`last_pos` is never on
an edge when this happens" is also false: `project_exit` returns a point
one pixel inside the *crossed* edge, which for a span reaching a corner
lies exactly on the neighbouring edge, so the next crossing of that
neighbour is declined as a slide along it. The lock (§7), a client
disconnecting as an activation races it, and a monitor change (§9) all
reach the same declined state by other routes.

The release position is the activation position moved one pixel inside
every edge it sits on — the same convention `project_exit` uses for the
ordinary return from a client. Releasing *at* the barrier would hand the
pointer straight back to the barrier that just fired. `last_pos` is set
to that release point too, or a declined activation would leave it on
the edge and decline the next one for the same reason.

For an accepted activation, `set_mode(Grab)` is an acknowledgement with
nothing to do — the compositor is already capturing — and `warp_cursor`
is a no-op, because a captured pointer is hidden and parked by the
compositor.

### 5.4 Release

`release(x, y)` calls `Release { activation_id, cursor_position: (x, y) }`.

The `activation_id` must be the one from the activation being ended. The
specification states a compositor ignores a `Release` for an id that is
no longer active, so a stale id means the capture is never released.

A `Deactivated` may still arrive after a `Release`; it is ignored when
its `activation_id` matches one already released. A `Deactivated` for
the *current* activation is a compositor-initiated end and routes to
`ServerCore::release_remote()` (§4.3).

### 5.5 Event translation

| libei | `CaptureEvent` | Note |
|---|---|---|
| `PointerMotion { dx, dy }` (f32) | `MotionRel` | see below |
| `Button { button, state }` | `Button` | evdev codes, used directly |
| `KeyboardKey { key, state }` | `Key` | evdev codes, **no `- 8`** (§3.3) |
| `ScrollDiscrete { dy }` | `Wheel` | 120 per notch, matching the existing unit |
| `ScrollDelta` | ignored | `ScrollDiscrete` covers wheels; smooth scroll would double-count |
| `Frame` | ignored | batch delimiter only |
| `Disconnected`, `DeviceRemoved` | backend error | session is over |

Relative motion arrives as `f32`. Truncating each event loses slow
movement entirely: a steady 0.4 px/event drag would never move the
pointer. The backend keeps a sub-pixel remainder per axis, adds it to
the next event, and emits the integer part.

## 6. Modifiers at the moment of crossing

`held` in `ServerCore` is built from key events. A Wayland server sees no
keys while local, so without help `Msg::Enter` would always report no
modifiers — and holding Shift across the edge, which
`2026-09-21-kvm-core-design.md` lists as a pass criterion, would type
lowercase on the client.

libei reports the modifier state at activation. Measured: `depressed =
0x1` with Shift held across the barrier (§3). The mask is translated
with the X11 core modifier layout:

| Bit | Modifier |
|---|---|
| `0x01` | Shift |
| `0x04` | Control |
| `0x08` | Mod1 / Alt |
| `0x40` | Mod4 / Super |

These indices are a convention, not a guarantee — XKB resolves virtual
modifiers per keymap, and doing it correctly would mean linking
`xkbcommon` to look the names up. That is not worth a dependency here:
if the layout is wrong on some unusual keymap the cost is one incorrect
modifier in `Enter`, not a crash or a stuck key. Recorded in §15.

**On release, the backend emits a key-up for every key it saw pressed
and not released.** Without this: hold Shift, cross, come back, release
Shift outside the capture — the release goes to the compositor, the
backend never sees it, `held` keeps Shift forever, and every subsequent
`Enter` is wrong. Nothing in the unit suite would notice.

## 7. The lock hotkey

A Wayland server receives no keyboard events while local, so the lock
hotkey cannot be observed the way X11 observes it. Worse, it cannot be
*undone*: locking removes the barriers, and with no barriers there is
nothing left to activate a capture in which the hotkey could be pressed
again. A lock that cannot be unlocked is worse than no lock.

`org.freedesktop.portal.GlobalShortcuts` is the only mechanism that
exists. `BindShortcuts` is called with the configured hotkey as
`preferred_trigger`; the compositor shows the user a dialog and owns the
final binding. `Activated` on that portal toggles the lock.

`preferred_trigger` is written in the **XDG shortcuts specification's**
syntax — an XKB keysym name with `CTRL+`, `SHIFT+`, `ALT+` and `SUPER+`
prefixes — not in pheme's key-table names. The default `hotkeys.lock`
value, `"ScrollLock"`, is a pheme name; the keysym is `Scroll_Lock`, and
sent verbatim the request names no key at all. `hotkeys.lock` is
therefore translated from pheme's names to keysyms before it is sent, and
a value containing `+` is passed through untouched so a full trigger can
be written by hand. The portal's returned array "includes the set of all
shortcuts and the empty set", so a bind that bound nothing still
succeeds: the absence of the lock shortcut in the response must be logged
as a warning naming the requested trigger, or the user has no way to
learn the hotkey is dead.

This is a real behavioural difference from X11 and Windows, where the
configuration file decides the key outright. The README must say so: on
Wayland, `hotkeys.lock` is a *request*, and the binding that ends up in
effect is whatever the desktop assigned.

## 8. Backend selection

```
Linux:
  WAYLAND_DISPLAY set or XDG_SESSION_TYPE == "wayland"
    → portal backend
    → no InputCapture interface: Error::Unsupported naming §16
  else → X11 backend
```

Wayland is checked **first**, and this matters more than it looks: a KDE
Wayland session also sets `DISPLAY=:0` for XWayland. The current
`detect_capture()` would connect to X11 successfully, install XInput2
and XTest, and capture nothing but XWayland clients — a failure with no
error at any layer.

The error for a compositor without the portal names the situation
plainly: the compositor does not implement InputCapture, wlroots support
is not built yet, and the machine can be used as a client meanwhile.

## 9. Zones and screens

`ScreenInfo` continues to come from `wayland_screens()` (`wl_output`),
which sub-project 1 already built. Barrier geometry, however, comes from
`GetZones`, because barriers are validated against the zone set and
nothing guarantees the two agree.

`ZonesChanged` therefore triggers both: fresh zones for the barriers, and
a fresh screen list for the core.

## 10. Dependencies

| Crate | Version | Features |
|---|---|---|
| `ashpd` | 0.13 | `async-io`, `input_capture`, `global_shortcuts`, no default features |
| `reis` | 0.7 | `async-io` |
| `futures-lite` | 2 | executor and select |

`ashpd` declares `rust-version = "1.87"`; the workspace pins `1.85`. The
workspace MSRV moves to **1.87**. `ashpd`'s default feature set pulls
tokio and every portal; the per-portal features above keep it to the two
that are used.

## 11. Testing

Everything that can be a pure function is one, and is tested as one.
Every defect found while designing this sub-project lives in that layer:

| Test | Pins |
|---|---|
| barrier geometry | a right-edge barrier is `x = x0 + W`, extent `y0..=y0 + H - 1` (§3.1) |
| edge set | one barrier per *connected* placement, none for the rest (§4.2) |
| activation clamp | 2577 on a 2560-wide rect clamps to 2559 and `on_edge` accepts it (§3.2) |
| key translation | evdev `30` maps to the same `KeyCode` as X11's `38` (§3.3) |
| scroll translation | `ScrollDiscrete { dy: 120 }` is one notch in the existing unit |
| modifier mask | `0x1` becomes `Modifiers::SHIFT` (§6) |
| sub-pixel motion | eight events of `dx = 0.4` produce three pixels, not zero (§5.5) |
| held-key flush | a key pressed and not released produces a key-up on release (§6) |

The portal itself is not mocked. Session lifecycle, barrier acceptance
and real capture go in the manual matrix (§14) and run on the KDE
machine.

## 12. Definition of done

- On this KDE Wayland machine acting as server, moving the pointer across
  the configured edge hands input to the client, and moving back returns
  it — both directions repeatable without a restart.
- Holding Shift across the edge produces uppercase on the client.
- The lock hotkey toggles the lock through GlobalShortcuts.
- Ending the capture from the compositor's own escape binding returns to
  local with no key held down on the client.
- A client connecting or disconnecting adds or removes its barrier
  without restarting the session.
- An X11 session on the same machine still uses the X11 backend and
  behaves exactly as it did before this sub-project.
- A compositor without the portal produces the §8 error and the process
  exits cleanly.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` green on the Ubuntu and
  Windows CI jobs.
- README documents the Wayland requirements, the permission dialog, and
  that the lock hotkey binding is owned by the desktop.

## 13. Latency

The portal adds nothing measurable to the steady-state path: libei
events arrive on a socket the same way XInput2 events arrive on the X11
connection. The event-to-wire budget from sub-project 1 is unchanged.

Switching latency is different. A crossing costs a D-Bus signal, and the
first crossing of a session costs the whole `ConnectToEIS` handshake and
device creation. The handshake is done once, at `Enable` time, not per
crossing — so the cost lands at startup rather than on the first switch.

## 14. Manual test matrix (added to `docs/testing.md`)

Run with this machine as the Wayland server and the QEMU VM as the
Windows client, and again Wayland → Linux.

| # | Check |
|---|---|
| W1 | Crossing the configured edge hands input to the client; crossing back returns it; repeat 20 times without a restart |
| W2 | Hold Shift across the edge — the client types uppercase |
| W3 | Compositor's own escape binding ends the capture: back to local, no key stuck on the client |
| W4 | Disconnect a client and connect a different one placed on another edge, without restarting the server — the old barrier goes away and the new edge's barrier appears. (The server handles one client at a time, so this is a swap, not a second connection) |
| W5 | Disconnect a client while it holds the input — the server recovers the pointer in under 5 s |
| W6 | The lock hotkey toggles the lock, and toggles it back |
| W7 | Edges with no connected client do **not** snag the pointer |
| W8 | Change the monitor layout mid-session (`ZonesChanged`) — barriers follow |
| W9 | Restart the server: does the permission dialog appear again? (§15) |
| W10 | Run the same build in an X11 session — unchanged behaviour |
| W11 | 100 keystrokes across the edge, no stuck key; input RTT under 1 ms over cable |

## 15. Known risks

- **The permission dialog may appear on every start.** KDE returned
  `restore_token = None` even when asked to persist. Whether its
  permission store silently re-grants on the next run is untested — W9
  is the test. If it does not, a Wayland server prompts the user at every
  launch, which is a usability problem with no application-side fix.
- **GNOME has never been executed.** The portal is a specification and
  mutter implements it, but every number in §3 was measured on KDE.
  Sub-project 2 shipped Windows code that was only ever cross-compiled,
  and three defects survived to the VM session. GNOME is best-effort
  until someone runs it.
- **The modifier bit layout is a convention** (§6). A keymap that maps
  Alt somewhere other than Mod1 produces a wrong modifier on entry.
- **Touchscreen is refused.** KDE advertises the capability and does not
  grant it. Nothing in Pheme uses it, but the granted set must be read
  from the response rather than assumed to match the request.
- **A monitor change needs a restart — §9 is only half implemented.**
  §9 says `ZonesChanged` triggers "fresh zones for the barriers, and a
  fresh screen list for the core". Only the first half ships. The
  barriers are re-declared against the new zones, but the screen list
  reaches `ServerCore` once, when the server starts, and the rect every
  activation is clamped into is fixed at the same moment. After a
  monitor is added or removed the barrier sits on the new outer edge
  while edge detection still uses the old rect, so the crossing can stop
  working — and, because a barrier is physically stopping the pointer,
  an activation the core then declines is one of the cases §5.3's
  release exists for. Pushing a fresh layout through `ServerCore` and
  every client placement mid-session is its own piece of work; until it
  is done the session compares the new zone union against the screen
  rect it started with and warns that the server must be restarted.
- **A barrier rejection is silent** (§3.1). It is visible only in
  `failed_barriers`, which is why §4.2 makes checking it mandatory.

## 16. Deferred: wlroots compositors

Recorded so the analysis does not have to be redone.

Hyprland and Sway do not implement `org.freedesktop.portal.InputCapture`,
and `xdg-desktop-portal-wlr` does not either. There is no portal path on
these compositors, so support means a second backend that speaks to the
compositor directly:

- **Capture:** a `zwlr_layer_shell_v1` surface, one-pixel wide, on the
  edge where a client sits. The pointer entering it is the crossing
  signal — the analogue of the portal's barrier.
- **Grab:** `zwp_pointer_constraints_v1` to lock the pointer, plus
  `zwp_relative_pointer_v1` for relative motion, and a keyboard grab
  through the layer surface's exclusive keyboard interactivity.
- **Return:** unlock the constraint and warp with
  `zwp_locked_pointer_v1::set_cursor_position_hint`.

The shape resembles §5 closely enough that §4's trait changes cover it —
`set_edges` maps to which edges get a layer surface, and `release`
maps to unlocking with a position hint. So this sub-project does not
close the door; it only declines to build a backend nobody here can run.
