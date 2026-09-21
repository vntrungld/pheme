# Sub-project 1 — KVM core

Date: 2026-09-21
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Expected outcome: run `pheme server` on machine A and `pheme client <ip>`
on machine B; moving the pointer across the screen edge controls B with
A's keyboard and mouse, and moving back returns control. Windows and
Linux X11 can be servers; Windows and Linux (uinput) can be clients.

## 1. Scope

In:

- `pheme-proto`: the complete `Msg` enum (including Audio/Clipboard
  variants so the wire format does not change later — unused for now).
- `pheme-net`: QUIC, identity, SPAKE2 pairing, fingerprint pinning,
  reconnect.
- `pheme-core`: layout, state machine, shadow key state, lock hotkey.
- `pheme-input`: HID keymap; Windows + Linux X11 capture; Windows
  (SendInput) + Linux (uinput) inject; `mock` backend.
- `pheme-app`: CLI `server`, `client`, `pair`, `setup`; TOML config; logging.

Out: audio, clipboard, mDNS, Wayland capture, tray/GUI, macOS.

## 2. `pheme-proto`

```rust
pub const PROTOCOL_VERSION: u16 = 1;

pub enum Os { Linux, Windows, MacOs }

pub struct ScreenInfo { pub x: i32, pub y: i32, pub w: u32, pub h: u32, pub primary: bool }

pub struct Modifiers(u8);         // bits: Shift, Ctrl, Alt, Meta (left/right merged)
pub struct KeyCode(pub u16);      // USB HID usage (page 0x07 keyboard; page 0x0C consumer uses bit 15)
pub enum Button { Left, Right, Middle, Back, Forward }
pub enum AudioStream { Playback, Mic }
pub struct AudioParams { pub rate: u32, pub channels: u8, pub frame_samples: u16 }

pub enum Msg {
    // Control stream (reliable, ordered)
    Hello    { version: u16, name: String, os: Os, screens: Vec<ScreenInfo> },
    HelloAck { version: u16, name: String, audio: AudioParams },
    Bye      { reason: String },
    Ping(u64), Pong(u64),
    Key      { seq: u32, code: KeyCode, down: bool },
    Button   { seq: u32, btn: Button, down: bool },
    Enter    { seq: u32, x: u16, y: u16, mods: Modifiers },
    Leave    { seq: u32 },
    // Datagram (unreliable)
    MouseMove{ seq: u32, dx: i16, dy: i16 },
    MouseAbs { seq: u32, x: u16, y: u16 },
    Wheel    { seq: u32, dx: i16, dy: i16 },       // 1/120 of a notch
    Audio    { stream: AudioStream, seq: u32, ts_us: u64, samples: Vec<i16> },
    // Clipboard stream
    Clipboard{ mime: String, data: Vec<u8> },
}

pub fn encode(m: &Msg, buf: &mut Vec<u8>);             // postcard; no allocation if buf is large enough
pub fn decode(bytes: &[u8]) -> Result<Msg, ProtoError>;
impl Msg { pub fn is_datagram(&self) -> bool; }
```

`seq` is a separate u32 counter per direction, incremented per input
message; it is used to log datagram loss (not for resync). `Enter.x/y`
are in client space, in pixels, origin at the top-left of the client's
virtual screen.

Tests: round-trip every variant; `MouseMove` encodes to ≤ 8 bytes; `Key`
≤ 8 bytes.

## 3. `pheme-net`

### Identity & trust

- `identity.key` (Ed25519 PKCS#8) + `identity.crt` (self-signed X.509,
  CN = `name`) generated with `rcgen` when missing. Fingerprint =
  SHA-256(DER cert).
- `trusted.toml`: `[[peers]] name = "..." fingerprint = "hex"`.
- Custom rustls verifiers (server: `ClientCertVerifier`; client:
  `ServerCertVerifier`) only compare the fingerprint against
  `trusted.toml`; CA chain, name and validity period are ignored.

### Pairing

Runs on the **same QUIC port** with a dedicated ALPN `pheme-pair/1` (the
main ALPN is `pheme/1`). The server only accepts the pairing ALPN while
in pairing mode (`pheme server --pair`, or a tray menu item later); the
mode ends after 120 s or after one successful pairing.

1. The server generates a random 6-digit code and prints it.
2. The client opens a QUIC connection with the pairing ALPN; its cert is
   not yet trusted, so the pairing-mode verifier accepts any cert but
   records the fingerprint.
3. On a stream: SPAKE2 (`spake2` crate, Ed25519 group) with password =
   code. Both sides derive `K`.
4. Each side sends `HMAC(K, own_fingerprint || observed_peer_fingerprint)`.
   The other side verifies. On mismatch: close; after 3 failures the
   server leaves pairing mode.
5. On success both sides write the peer into `trusted.toml`. Done.

The code is valid for one handshake, online brute force is limited to 3
attempts, and offline brute force is infeasible thanks to SPAKE2.

### Transport

```rust
pub struct Endpoint;                        // wraps quinn::Endpoint + config
impl Endpoint {
    pub fn server(cfg: &NetConfig, id: &Identity, trust: &TrustStore) -> Result<Self>;
    pub fn client(cfg: &NetConfig, id: &Identity, trust: &TrustStore) -> Result<Self>;
    pub async fn accept(&self) -> Result<Peer>;                     // server
    pub async fn connect(&self, addr: SocketAddr) -> Result<Peer>;  // client
}

pub struct Peer;
impl Peer {
    pub fn remote_name(&self) -> &str;
    pub async fn send_control(&self, m: &Msg) -> Result<()>;
    pub fn send_datagram(&self, m: &Msg);        // errors ignored, logged at debug
    pub fn incoming(&self) -> &mpsc::Receiver<Msg>;  // merged control + datagram
    pub fn rtt(&self) -> Duration;
    pub async fn closed(&self) -> CloseReason;
}
```

quinn tuning: `max_idle_timeout = 5 s`, `keep_alive_interval = 1 s`,
`initial_rtt = 1 ms`, `datagram_send_buffer_size = 16 KiB`,
`datagram_receive_buffer_size = 64 KiB`, `max_concurrent_bidi_streams =
4`. Control stream framing: `u16 LE length` + postcard bytes.

Reconnect (client): loop `connect → run → closed → backoff`; backoff
starts at 0.5 s, doubles, caps at 5 s; resets after a connection lasts
≥ 10 s.

Tests: two `Endpoint`s on `127.0.0.1` in one process: (a) correct code →
both trust stores contain each other; (b) wrong code → error, nothing
written; (c) untrusted client → rejected at TLS; (d) control + datagram
round-trip; (e) server shutdown → `closed()` returns in < 6 s.

## 4. `pheme-core`

### Types

```rust
pub enum Side { Left, Right, Top, Bottom }
pub struct ClientPlacement { pub name: String, pub side: Side, pub span: (f32, f32) }
pub struct Layout { pub server_screens: Vec<ScreenInfo>, pub clients: Vec<ClientPlacement> }

pub enum CaptureEvent {
    MotionAbs { x: i32, y: i32 },           // Observe mode
    MotionRel { dx: i32, dy: i32 },         // Grab mode
    Button { btn: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
}

pub enum Action {
    SendControl(Msg), SendDatagram(Msg),
    Grab, Ungrab, WarpCursor { x: i32, y: i32 },
    SetLocked(bool),                        // lets the app update tray/log
}

pub struct ServerCore { .. }
impl ServerCore {
    pub fn new(layout: Layout, hotkeys: Hotkeys) -> Self;
    pub fn client_connected(&mut self, name: &str, screens: Vec<ScreenInfo>) -> Vec<Action>;
    pub fn client_disconnected(&mut self, name: &str) -> Vec<Action>;
    pub fn on_event(&mut self, ev: CaptureEvent) -> Vec<Action>;
    pub fn active(&self) -> Active;         // Local | Remote(name)
}

pub struct ClientCore { .. }                // tracks held keys; release_all on Leave/disconnect
impl ClientCore {
    pub fn on_msg(&mut self, m: Msg) -> Vec<InjectAction>;
    pub fn on_disconnect(&mut self) -> Vec<InjectAction>;   // release everything held
}
```

### `ServerCore` behaviour

- **Local**: only `MotionAbs` is handled. For each client, compute the
  edge segment `[a, b]` on the bounding rectangle of `server_screens`
  from `side` and `span`. If `x`/`y` is exactly on the boundary
  (`x == min_x` for Left, `x == max_x - 1` for Right, likewise
  Top/Bottom), the along-edge coordinate is within `[a, b]`, the previous
  event was *not* on that boundary (i.e. the pointer is moving outward),
  `!locked`, and the client is connected → switch to Remote:
  - Compute the entry position on the client: project the along-edge
    position onto the client's opposite edge using
    `(pos - a) / (b - a)` × client size; the perpendicular coordinate is
    0 (Right → client x = 0; Left → x = client_w - 1; …).
  - Return `[Grab, WarpCursor{center}, SendControl(Enter{x,y,mods})]`.
  - `mods` comes from the shadow key state so the client knows which
    modifiers are held.
- **Remote(c)**: every event is forwarded:
  - `MotionRel` → update the virtual position `(vx, vy)` in client space
    (clamped to the client screen); `SendDatagram(MouseMove{dx,dy})`. If
    the virtual position crosses the client's opposite edge (the one
    leading back to the server) →
    `[SendControl(Leave), Ungrab, WarpCursor{matching point on the server edge}]`,
    back to Local. Other client edges only clamp; they never leave.
  - `Key` → update the shadow set; if it is the lock hotkey → toggle
    `locked`, emit `SetLocked`, do *not* forward; otherwise
    `SendControl(Key)`.
  - `Button`/`Wheel` → `SendControl(Button)` / `SendDatagram(Wheel)`.
- **Lock** while Local: the lock key toggles `locked`; while `locked`,
  never switch to Remote. While Remote, lock means "stay on the client" —
  do not leave when crossing the edge; pressing it again releases.
- `client_disconnected` while Remote(c) → `[Ungrab, WarpCursor{server center}]`,
  back to Local; the shadow set is kept (keys are still physically held;
  the LL hook / XI2 will report the release later — but since we are
  Local we do not forward, so the shadow set is only cleared on release).
- On returning to Local via `Leave`, no local injection is needed to
  avoid stuck modifiers on the server: the LL hook / XI2 grab only
  blocks *forwarding to apps*; the OS still sees the physical key-up after
  Ungrab. If a modifier does get stuck on some OS in practice, add
  `Action::ReleaseLocalModifiers` there (recorded as a risk to verify
  manually in §7).

### `ClientCore` behaviour

- `Enter` → `MoveAbs(x,y)`; `mods` is recorded for logging (modifiers are
  not injected from `mods` — the real key-down was/is forwarded via `Key`).
- `Key`/`Button` → update the held set, inject.
- `Leave`, `Bye`, disconnect → `release_all()` for every key/button in
  the held set, then clear.
- `MouseMove`/`Wheel` → inject directly.

### Tests (unit, no OS)

1. Right/left/top/bottom, full and partial `span`: enters at the right
   position; projected coordinates are correct when client resolution
   differs from the server's.
2. Pointer on the edge while the previous event was already on the edge
   (dragging along the edge) → no switch.
3. Edge with no client → no switch.
4. Client not connected → no switch.
5. Remote: crossing back → Leave + Ungrab + Warp to the right point;
   crossing another edge → clamp only.
6. Lock: blocks switching while Local; blocks leaving while Remote; the
   lock key is not forwarded.
7. Disconnect while Remote → Ungrab, back to Local.
8. `ClientCore`: hold 3 keys + 1 button then `Leave` → 4 releases in the
   right order (buttons first, then keys, modifiers last).
9. Property test (`proptest`): a random event sequence never emits `Grab`
   twice in a row or `Ungrab` while Local.

## 5. `pheme-input`

### Traits

```rust
pub enum CaptureMode { Observe, Grab }

pub trait InputCapture: Send {
    fn start(&mut self, tx: crossbeam::Sender<CaptureEvent>) -> Result<()>;
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
    fn stop(&mut self);
}

pub trait InputInject: Send {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, btn: Button, down: bool) -> Result<()>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn key(&mut self, code: KeyCode, down: bool) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
}

pub fn detect_capture() -> Result<Box<dyn InputCapture>>;  // by OS/session
pub fn detect_inject()  -> Result<Box<dyn InputInject>>;
```

`set_mode(Grab)` must: block keyboard + mouse from reaching local apps,
hide the cursor, confine the cursor (warp to center on every event or
`ClipCursor` to 1×1), and switch to emitting `MotionRel`.
`set_mode(Observe)` undoes all of that. `set_mode` is called from the app
thread; the backend must hand the request to its own thread (Windows:
`PostThreadMessage`; X11: pipe/eventfd into the event loop).

### Keymap

`keymap/hid.rs`: `KeyCode` constants for every common HID usage.
`keymap/evdev.rs`: `hid_to_evdev(KeyCode) -> Option<u16>`,
`evdev_to_hid(u16) -> Option<KeyCode>` — static table generated from
`linux/input-event-codes.h`.
`keymap/win.rs`: `hid_to_scancode(KeyCode) -> Option<(u16, bool /*ext*/)>`
and the inverse — static table from the USB HID Usage Tables §10 +
Windows scancode set 1. X11 keycode = evdev + 8.

Tests: every entry round-trips; no two HID codes map to the same
scancode; tricky keys (Pause, PrintScreen, NumLock, extended arrow keys,
Right Ctrl/Alt, Win/Meta, basic media keys) have dedicated tests.

### Windows

Capture (`windows` crate):
- Dedicated thread running a message loop. `SetWindowsHookExW(WH_KEYBOARD_LL)`
  and `WH_MOUSE_LL`. Observe: the hook only reads the pointer position
  from `MSLLHOOKSTRUCT` and returns `CallNextHookEx`. Grab: the hook
  returns `1` (swallow) for every event; additionally
  `RegisterRawInputDevices` (mouse, `RIDEV_INPUTSINK`) on a hidden window
  to get raw `dx/dy` (no pointer acceleration), `ClipCursor` to a 1×1
  rectangle at the screen center, and the arrow cursor replaced by a
  blank one via `SetSystemCursor(OCR_NORMAL)` (restored with
  `SystemParametersInfo(SPI_SETCURSORS)`); `ShowCursor` is per-thread and
  cannot hide the cursor over other applications' windows.
- Ignore events flagged `LLKHF_INJECTED` / `LLMHF_INJECTED`.
- Scancode from `KBDLLHOOKSTRUCT.scanCode` + `LLKHF_EXTENDED`; Pause and
  PrintScreen have special scancodes and are handled explicitly.
- Wheel: `mouseData` HIWORD (signed, units of 120) → keep the unit.
- `screens()`: `EnumDisplayMonitors`; DPI-aware
  (`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`) so coordinates
  are physical pixels.

Inject: `SendInput`. Keys: `KEYEVENTF_SCANCODE` (+`EXTENDEDKEY`).
Relative mouse: `MOUSEEVENTF_MOVE` (Windows applies the client's
acceleration — accepted, a physical USB mouse behaves the same).
Absolute: `MOUSEEVENTF_ABSOLUTE | VIRTUALDESK` with coordinates
normalized to 0..65535 on the virtual desktop. Wheel:
`MOUSEEVENTF_WHEEL`/`HWHEEL` with `mouseData` = 1/120 value.

### Linux X11

Capture (`x11rb` with the `xinput`, `xtest`, `xfixes`, `randr` extensions):
- Dedicated event thread with its own X connection (a second connection
  is used by the app thread for `warp_cursor` and to wake the event loop
  with a `ClientMessage`). `XISelectEvents` on the root window with
  `XIAllMasterDevices` for `XI_RawMotion`, `XI_RawButtonPress/Release`,
  `XI_RawKeyPress/Release` (raw events reach the root window regardless
  of which window is under the pointer; non-raw `XI_Motion` would not).
  Observe: on every RawMotion call `QueryPointer(root)` and emit
  `MotionAbs`; raw keys are emitted as `Key` (needed for the lock
  hotkey) and nothing is blocked. Grab: emit `MotionRel` from
  `raw_values` (pre-acceleration) plus keys/buttons.
- Grab: `XIGrabDevice` on the master pointer + master keyboard with
  `owner_events = false` (apps receive nothing), `XFixesHideCursor` on
  root, warp to center with `XIWarpPointer` after every RawMotion.
- X11 keycode − 8 → evdev → HID.
- `screens()`: RandR CRTCs.
- Wayland session (`$XDG_SESSION_TYPE == wayland`) → `detect_capture()`
  returns a clear error: "Wayland capture is not supported yet
  (sub-project 4); use an X11 session or run this machine as a client".

Inject (`evdev` crate, `/dev/uinput`):
- One uinput device "Pheme Virtual Input" with `EV_KEY` (all keyboard
  keycodes + BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA), `EV_REL` (REL_X, REL_Y,
  REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES), `EV_ABS`
  (ABS_X, ABS_Y with range = virtual screen) — libinput accepts a hybrid
  device if `INPUT_PROP_POINTER` is set; if libinput rejects it in
  practice, split into 2 devices (keyboard + relative mouse, absolute
  tablet). Recorded as a risk.
- Wheel: send both `REL_WHEEL_HI_RES` (1/120) and `REL_WHEEL` once 120
  has accumulated.
- `screens()`: X11 → RandR; Wayland → no standard API, use `wl_output`
  via `wayland-client` (geometry only, no permission required).
- Missing `/dev/uinput` permission → error with a hint to run
  `pheme setup`.

### Mock

`MockCapture` (events pushed from tests) and `MockInject` (records calls
into a `Vec`) for `pheme-app` integration tests.

## 6. `pheme-app`

Server runtime:

```
tokio main
├── accept task: Endpoint::accept → Peer → HelloAck; only 1 peer per client name
├── capture thread (OS) ──crossbeam──► router task:
│      loop { ev = rx.recv(); for a in core.on_event(ev) { execute(a) } }
│      execute: SendControl → peer.send_control (spawned, not awaited)
│               SendDatagram → peer.send_datagram
│               Grab/Ungrab/Warp → capture.set_mode / warp_cursor
└── peer reader task: incoming → Ping/Pong, Bye → core.client_disconnected
```

Client runtime: `connect loop` → `Hello` → reader task: `core.on_msg` →
inject. Disconnect → `core.on_disconnect` → release_all → backoff.

CLI (`clap`): `server [--config] [--pair]`, `client <host[:port]>
[--config]`, `pair <host[:port]> <code>`, `setup`, `--stats`, `-v/-vv`.
TOML config as in the overall spec, only the keys used by SP1 (`role`,
`name`, `listen`, `connect`, `hotkeys.lock`, `clients`). Default location
`~/.config/pheme/config.toml` / `%APPDATA%\pheme\config.toml`; no file →
defaults + CLI arguments.

`--stats`: log RTT (QUIC), events sent/received and lost datagrams
(via `seq` gaps) every second.

`pheme setup` (Linux): write the udev rule, `usermod -aG input`,
`udevadm control --reload` — needs sudo; prints the commands if not
privileged. (Windows): SP1 only prints "OK".

## 7. Definition of done

Automated: `cargo test --workspace` green on Linux and Windows (CI); mock
integration test: server + client in one process, 10 000 `MouseMove`
over QUIC localhost, mean RTT < 0.5 ms, no loss.

Manual (`docs/testing.md`), for all 4 combinations of Linux X11/Windows
× Linux/Windows:

1. Pairing succeeds; a third, unpaired machine attempting to connect is
   rejected.
2. Pointer across the right edge → controls the client; returns across
   the client's left edge.
3. Dragging along the edge does not switch machines.
4. Type 100 characters including Shift/Ctrl/Alt combos, arrow keys,
   Numpad, PrintScreen, Pause — correct and nothing stuck.
5. Hold Shift while crossing the edge, release on the client → not stuck
   on either machine.
6. Vertical/horizontal scrolling is smooth (hi-res).
7. Lock hotkey: on → cannot leave; off → can leave.
8. Unplug the client's network while Remote → the server regains its
   mouse in < 5 s; plug back in → the client reconnects in < 10 s, no
   stuck keys.
9. Multi-monitor client: `Enter` lands on the right monitor; the pointer
   can travel across all of them.

## 8. Known risks

- Stuck modifiers on the server after Ungrab (§4) — verify manually;
  fallback `ReleaseLocalModifiers`.
- libinput may reject a hybrid rel+abs uinput device — fallback: split
  into 2 devices.
- Windows: if the process dies while grabbed, the blank system cursor
  persists until `SPI_SETCURSORS` runs again (re-running Pheme or
  toggling any pointer setting restores it).
- X11 `XIGrabDevice` fails if another app holds a grab (an open menu) →
  retry 3 times 10 ms apart, then skip that switch and stay Local.
