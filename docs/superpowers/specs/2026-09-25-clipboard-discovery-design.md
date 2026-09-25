# Sub-project 5 — Text clipboard and mDNS discovery

Date: 2026-09-25
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-24-wayland-capture-design.md`
Expected outcome: text copied on one machine can be pasted on the other
after the pointer crosses the edge, and a client finds its server by
name instead of by IP address.

## 1. Scope

In:

- A new crate, `pheme-clip`: read and write the system text clipboard on
  Windows, Linux X11 and Linux Wayland, behind one trait with a mock.
- Clipboard exchange at the moment the pointer changes machine, over a
  QUIC unidirectional stream (architecture §6).
- mDNS: the server advertises `_pheme._udp.local.`, the client resolves
  the `connect` name through it on every reconnect, and a new
  `pheme discover` subcommand lists what is on the network.

Out:

- **Images and files.** Architecture §2 fixes v1 at text only. Nothing
  here forecloses them: `Msg::Clipboard` already carries a `mime`, and a
  second MIME type is a later addition to `pheme-clip`, not a redesign.
- **Continuous clipboard monitoring.** The clipboard is read when the
  pointer crosses, never in the background (§3.1).
- **GNOME Wayland clipboard.** Mutter implements neither
  `wlr-data-control` nor `ext-data-control` and has declined to; there
  is no other route for a window-less process (§3.3). Input and audio
  are unaffected.
- **Zero-configuration connection.** Discovery resolves the name the
  user configured. It never picks a server on its own (§5.3).

## 2. Two features, one sub-project

Clipboard and discovery share no code. They are one sub-project because
the roadmap (architecture §9) makes them one, and because each is too
small to justify its own spec → plan → review cycle. The implementation
plan keeps them in separate tasks; nothing in §3 depends on §5.

## 3. Clipboard

### 3.1 When the clipboard moves

The clipboard is exchanged when the pointer changes machine, and at no
other time:

| Event | Side | Action |
|---|---|---|
| Core emits `Msg::Enter` | server | read the local clipboard, send `Msg::Clipboard` |
| `Msg::Leave` or `Msg::Bye` received | client | read the local clipboard, send `Msg::Clipboard` |
| `Msg::Clipboard` received | either | write it to the local clipboard |

This is the model Synergy and Deskflow use, and it was chosen over
syncing on every clipboard change for three reasons:

1. It is what the user means. A copy is intended for a paste, and the
   paste that needs the network is the one on the other machine.
2. Nothing leaves the machine that the user did not carry there. A
   password copied to paste locally stays local.
3. It needs no background clipboard monitoring. On Wayland, watching the
   clipboard is the part that does not work: `wlr-data-control` offers
   no change notification a window-less process can rely on, and polling
   the selection wakes the compositor on a timer forever.

The cost is that copying without crossing leaves the other machine with
stale content. That is the trade the model makes, and it is the trade
Synergy users have lived with for twenty years.

### 3.2 The crate

`crates/pheme-clip/`:

```rust
pub trait Clipboard: Send {
    /// `Ok(None)` when the clipboard holds no text — including when it
    /// holds an image, which is not an error.
    fn get_text(&mut self) -> Result<Option<String>, ClipError>;
    fn set_text(&mut self, text: &str) -> Result<(), ClipError>;
}

/// Opens the platform clipboard. Fails where no clipboard is reachable,
/// which is a supported state, not a crash (§3.3).
pub fn open() -> Result<Box<dyn Clipboard>, ClipError>;

pub struct MockClipboard { /* … */ }   // tests; also `Clipboard`
```

Files:

- `src/lib.rs` — the trait, `ClipError`, `open()`.
- `src/sync.rs` — `ClipSync`, the policy (§3.4). No OS calls.
- `src/backend.rs` — the `arboard` implementation.
- `src/mock.rs` — `MockClipboard`, with a settable failure mode.

### 3.3 Backend: `arboard`

One backend covers all three platforms:

| Platform | What `arboard` uses |
|---|---|
| Windows | Win32 clipboard (`OpenClipboard` / `CF_UNICODETEXT`) |
| Linux X11 | `x11rb`, with a background thread that owns the selection |
| Linux Wayland | `wl-clipboard-rs` 0.9, speaking `ext-data-control-v1` and falling back to `zwlr_data_control_manager_v1` |

Dependency: `arboard = { version = "3.6", default-features = false, features = ["wayland-data-control"] }`.
Turning off default features drops `image-data` and with it the `image`
crate and the macOS graphics stack. License MIT OR Apache-2.0, which
GPL-3.0-only can consume.

This repository writes its own backends everywhere else — PipeWire,
WASAPI, uinput, the InputCapture portal — because no crate did those
jobs. The clipboard is different on two counts. First, a crate does do
the job, on all three platforms, with the Wayland protocol this spec
requires. Second, the X11 half is not a thin wrapper: X11 has no
clipboard, only selections, and a process that sets the clipboard must
own the `CLIPBOARD` selection and answer every `SelectionRequest` from
every other client until someone takes ownership away. That is a window,
an event loop and a conversion table — roughly three hundred lines whose
bugs appear only when a particular other application asks for a
particular target.

`open()` fails on GNOME Wayland, where no data-control protocol exists.
That is a supported outcome: the failure is logged once at `warn` with
the reason, the clipboard feature stays off for the process lifetime,
and input and audio run untouched. The same path covers a headless
session or a compositor that offers nothing at all, so there is one
degraded mode, not several.

The `arboard` API is blocking. Every call runs on a blocking thread
(`tokio::task::spawn_blocking`), never on the runtime, and never on the
input path.

### 3.4 Policy: `ClipSync`

```rust
pub struct ClipSync { last: Option<String> }

impl ClipSync {
    /// The text to send, or `None` to send nothing.
    pub fn outgoing(&mut self, text: String) -> Option<String>;
    /// `true` when the caller should write `text` to the local clipboard.
    pub fn incoming(&mut self, text: &str) -> bool;
}
```

`last` holds the last content this side exchanged in either direction.
Both methods compare against it and update it. Three rules fall out of
one field:

- **Empty is not sent.** An empty clipboard carries no intent, and
  sending it would clear the other machine's.
- **Repeats are not sent.** Crossing the edge four times without copying
  anything sends one clipboard, not four.
- **Echoes stop.** A sends text to B; B stores it in `last`; when B
  later crosses back, `outgoing` sees the same text and returns `None`.
  Without this the content would bounce on every crossing.

Anything over `proto::MAX_CLIP_BYTES` is dropped by `outgoing`, with a
`warn` naming the size. One mebibyte is about 500 pages of text; above
it the content is not something a person copied to paste.

The constant lives in `pheme-proto`, not in `pheme-clip`. Both the
sender's policy (§3.4) and the receiver's stream reader (§3.5) enforce
it, and those live in different crates that must not disagree — a
receiver with a smaller cap than the sender would silently drop content
the sender believed it had delivered. `pheme-net` already depends on
`pheme-proto`; it must not depend on `pheme-clip`.

```rust
// pheme-proto
/// The largest clipboard payload Pheme sends or accepts, in bytes.
pub const MAX_CLIP_BYTES: usize = 1024 * 1024;
```

`ClipSync` touches no OS and no network. It is the unit-tested core of
the feature.

### 3.5 Transport: one unidirectional stream per change

`Msg::Clipboard { mime, data }` already exists in `pheme-proto` with a
round-trip test, and `is_datagram()` already answers `false` for it.
`mime` is `"text/plain;charset=utf-8"`; a receiver that does not
recognise the MIME logs and ignores the message.

It does **not** travel on the control stream. The control stream carries
`Key`, `Button`, `Enter` and `Leave`, and QUIC delivers one stream in
order: a one-megabyte clipboard written there would hold every
keystroke behind it until the transfer completed. That is head-of-line
blocking on the exact path this project optimises first. A separate
unidirectional stream per clipboard change is what architecture §6
specifies, and this is why.

`pheme-net/src/transport.rs` gains:

- `PeerSender::send_clipboard(&self, m: &Msg)` — opens a uni-stream,
  writes one frame, finishes, all in a spawned task. It never blocks the
  caller and never fails the connection; an error is logged at `debug`.
- An `accept_uni` reader task in `Peer::new`, alongside the existing
  control-stream and datagram readers. It reads the stream to its end
  under a limit of `MAX_CLIP_BYTES + CLIP_FRAME_SLACK` bytes, where the
  slack covers the postcard header, the MIME string and the length
  prefix. A stream that reaches the limit is reset without being
  decoded; what fits is decoded as one `Msg` and forwarded on a new
  bounded channel.
- `Peer::take_clipboard() -> mpsc::Receiver<Msg>`, matching the existing
  `take_incoming` and `take_audio`.

A stream that exceeds the cap, decodes to something other than
`Msg::Clipboard`, or fails to decode is reset and logged at `debug`. The
connection survives: a peer with a different idea about clipboard
content must not be able to drop the input link.

### 3.6 Where it hooks

`pheme-core` does not change. No new `Action`, no new state. The
architecture document's crate table (line 73) lists "clipboard sync"
among the core's responsibilities; that line is amended by this spec,
because the clipboard needs no state machine and the core is better for
staying free of I/O policy.

- **Server** — `pheme-app/src/server.rs::run_actions` already matches on
  every `Action::SendControl(m)`. When `m` is `Msg::Enter`, it also
  spawns the clipboard send. The `Link` struct already holds the
  `PeerSender` the send needs.
- **Client** — `pheme-app/src/client.rs`'s message loop already
  special-cases `Msg::Bye` before handing it to the core, and passes
  everything else through one generic arm. It gains an arm for
  `Msg::Leave`, and both spawn the clipboard send before the message
  reaches the core.
- **Both** — a task draining `Peer::take_clipboard()` calls
  `ClipSync::incoming` and, when it answers `true`, writes the text
  through `spawn_blocking`.

The send is fire-and-forget: reading the clipboard must never delay the
`Enter` that hands over the pointer. The two therefore race, and the
clipboard can arrive before or after the `Enter` — QUIC orders one
stream, not two. This is harmless. The user needs hundreds of
milliseconds to reach Ctrl+V; the clipboard needs one round trip.

One race is real and accepted: the user crosses back to the server, the
client sends its clipboard, and in the milliseconds before it lands the
user copies something new on the server. The arriving text wins.
Narrowing the window means holding the pointer handover until the
clipboard is in, which trades a guaranteed cost against a rare one.

## 4. Discovery

### 4.1 What the server advertises

`pheme-net/src/discovery.rs`, on `mdns-sd` 0.21 (architecture §4 names
it; it needs no async runtime and works the same on Windows and Linux).

- Service type `_pheme._udp.local.` — the transport is QUIC over UDP.
- Instance name: the `name` field from the config.
- Port: the port of `listen`.
- TXT records: `fp=<fingerprint>` and `v=1`.

The fingerprint is the same 64-character string `pheme-net`'s
`identity::fingerprint` produces and the trust store keys on. It is
published so `pheme discover` can show it and the user can compare it
against what `pheme pair` prints. It is advisory: trust is still
established by the pairing code, never by a TXT record.

Advertising is on by default and controlled by `discovery = false` in
the config. The `Advertiser` unregisters on drop, so a server that exits
cleanly stops answering immediately instead of leaving a stale record to
time out.

### 4.2 The API

```rust
pub struct Advertiser(/* … */);   // unregisters on drop
pub fn advertise(name: &str, port: u16, fingerprint: &str) -> Result<Advertiser>;

pub struct Found {
    pub name: String,
    pub addr: SocketAddr,
    pub fingerprint: Option<String>,
}

/// Every instance seen before `timeout` elapses.
pub async fn browse(timeout: Duration) -> Result<Vec<Found>>;

/// The first instance whose name matches, or `None` at `timeout`.
pub async fn resolve(name: &str, timeout: Duration) -> Result<Option<SocketAddr>>;
```

`mdns-sd` delivers events on its own channel from its own thread; both
functions wrap that in a blocking task and hand results back as futures.
The default timeout is three seconds, overridable on `pheme discover`.

### 4.3 Resolving `connect`

`pheme-app/src/target.rs` — pure, no I/O, unit-tested:

```rust
pub enum Target {
    Fixed(SocketAddr),
    Dns(String),    // host or host:port
    Mdns(String),   // instance name
}
pub fn parse(s: &str, default_port: u16) -> Target;
```

Rules, in order:

1. Parses as a `SocketAddr` → `Fixed`. Covers `192.168.1.5:24800` and
   IPv6 literals.
2. Contains `:` or `.` → `Dns`. Covers `laptop:24800`, `10.0.0.4` and
   `laptop.lan`, and preserves today's behaviour exactly.
3. Otherwise a bare label such as `laptop-win` → `Mdns`.

A `Mdns` target that finds nothing within the timeout falls back to
`Dns` before failing, so a bare hostname that a local resolver knows
still works. A `.local` name goes down the `Dns` path, where the
system's own mDNS resolver handles it if one is installed; Pheme does
not second-guess the resolver.

**Resolution repeats on every reconnect.** Today `client.rs` receives a
`SocketAddr` resolved once at startup, so a server that changes address
is unreachable until the client is restarted. `ClientDeps` takes a
`Target` instead, and the reconnect loop resolves it on each attempt.
This is the reason discovery is worth building: a DHCP lease change or a
server restart on another port now heals itself within one backoff
interval.

### 4.4 CLI

```
pheme discover [--timeout 3]
```

prints one row per instance — name, address, fingerprint — and a warning
line when two instances share a name, because in that case `resolve`
returns whichever answered first and the choice is not the user's.

`pheme client <host>` and `pheme pair <host>` accept an mDNS instance
name wherever they accept an address, through the same `Target::parse`.

## 5. What this does not do

- It never connects to a server the user did not name. A machine
  advertising on the LAN is not an invitation.
- It never treats a TXT fingerprint as trust. Pairing is unchanged.
- It never makes the clipboard a transport for anything but text.

## 6. Dependencies

| Crate | Version | Where | Why |
|---|---|---|---|
| `arboard` | 3.6, `default-features = false`, `features = ["wayland-data-control"]` | `pheme-clip` | clipboard on three platforms (§3.3) |
| `mdns-sd` | 0.21 | `pheme-net` | service advertisement and browsing (§4.1) |

`arboard` pulls `x11rb` (already in the tree for `pheme-input`),
`wl-clipboard-rs` and `clipboard-win`. Both licenses are MIT OR
Apache-2.0.

## 7. Testing

Automated, and therefore run on CI:

- `ClipSync`: empty input, repeated input, over-cap input, the echo
  A→B→A, and that `incoming` reports `false` for content it just sent.
- `Target::parse`: socket addresses, IPv6 literals, `host:port`, dotted
  hostnames, bare labels, and the empty string.
- Transport: a uni-stream carrying a valid `Msg::Clipboard` arrives on
  `take_clipboard`; a stream over the cap is dropped and the connection
  still carries a subsequent control message; a uni-stream carrying a
  non-clipboard message is ignored.
- Integration, server and client in one process over real QUIC with
  `MockClipboard` on both sides: crossing the edge carries the server's
  text to the client; crossing back carries the client's text to the
  server; crossing twice with no copy in between sends one message, not
  two; a client whose `open()` failed still crosses the edge normally.

Not on CI:

- The mDNS round trip — register, browse, resolve on loopback — exists
  as an `#[ignore]`d test. GitHub's runners do not reliably pass
  multicast, and a test that is green because nothing listened is worse
  than no test.
- Every real clipboard backend. No CI runner has an X11 display, a
  Wayland compositor or a Windows desktop session with a clipboard.
  This is the same limit sub-project 4 has, and the manual matrix is the
  answer.

## 8. Manual test matrix (added to `docs/testing.md`)

| ID | What | Pass |
|---|---|---|
| C1 | Linux X11 server → Windows client: copy on the server, cross, paste | the text pastes |
| C2 | Windows server → Linux X11 client: copy on the client, cross back, paste on the server | the text pastes |
| C3 | KDE Wayland server → Windows client, both directions | the text pastes both ways |
| C4 | Unicode: emoji, Vietnamese diacritics, CRLF from a Windows editor | pastes unchanged, no mojibake |
| C5 | A 1 MiB paste, then a 2 MiB one | the first crosses; the second is refused with a log line and input keeps working |
| C6 | GNOME Wayland server | one `warn` at startup, clipboard silently inactive, input and audio normal |
| C7 | Copy, cross, copy again on the far side, cross back | each side ends with what the other last copied; nothing bounces |
| D1 | `pheme discover` with a server running | one row, correct name, address and fingerprint |
| D2 | `connect = "<name>"`, then change the server's IP and restart it | the client reconnects without being restarted |
| D3 | Two servers with the same name | `discover` lists both and warns |
| D4 | Windows with the firewall at its default | `discover` finds the Linux server, or the firewall prompt appears and `pheme setup` explains it |

## 9. Definition of done

- `pheme-clip` exists with the trait, `arboard` backend, mock, and
  `ClipSync` with its tests.
- Clipboard crosses in both directions in the integration test over real
  QUIC.
- A clipboard transfer cannot delay or block an input message: the
  clipboard has its own stream, and the send never runs on the runtime
  thread that carries input.
- `open()` failing leaves every other feature working.
- The server advertises, `pheme discover` lists, and `connect` accepts a
  bare name.
- The client re-resolves its target on every reconnect attempt.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` are clean on Linux and
  Windows.
- `README.md` states that the clipboard is text only, crosses with the
  pointer, and does not work on GNOME Wayland.
- `docs/testing.md` carries C1–C7 and D1–D4.
- The architecture document's crate table no longer places clipboard
  sync in `pheme-core` (§3.6).

## 10. Known risks

- **GNOME Wayland has no clipboard.** Accepted and documented. If Mutter
  ever ships `ext-data-control`, `arboard` picks it up with no change
  here.
- **Windows Firewall blocks UDP 5353 by default for some profiles**, so
  `pheme discover` can come back empty on a machine that is otherwise
  working. `pheme setup` reports it; the fallback is an IP address,
  which still works.
- **`arboard` owns the X11 selection in a thread of its own.** When the
  process exits, the clipboard content it set is gone. Every X11
  application behaves this way; clipboard managers exist for this
  reason.
- **`wl-clipboard-rs` needs the compositor to offer a data-control
  protocol at the version it knows.** KWin and wlroots do today. A
  compositor that offers neither lands in the same degraded mode as
  GNOME, which is why that mode is a supported state rather than an
  error path.
- **Two machines with the same name** make `resolve` non-deterministic.
  `discover` warns; the fix is a unique `name`, and the config already
  requires one for the client list.
