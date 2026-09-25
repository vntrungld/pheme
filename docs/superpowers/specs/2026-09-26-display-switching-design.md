# Sub-project 7 — Display input switching

Date: 2026-09-26
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-25-tray-and-gui-design.md`
Expected outcome: a person with one physical monitor cabled to both
machines sees the machine the pointer is on. Crossing the edge moves the
keyboard, the mouse, the clipboard — and now the picture.

## 1. Scope

In:

- A new crate, `pheme-display`: read and set a monitor's input source over
  DDC/CI on Windows and Linux, behind one trait with a mock.
- An automatic switch on every pointer crossing, in both directions.
- A recovery hotkey and a GUI button for when the monitor missed a command.
- `pheme displays`, which prints what each monitor is and which input
  values it accepts, because those values are vendor-specific and a person
  cannot configure this without them.

Out:

- **Screen streaming.** Nothing here captures or transmits a picture. The
  monitor is physically cabled to both machines and pheme only tells it
  which cable to look at.
- **Choosing the input value for the user.** A monitor's capability string
  is advisory and often wrong or absent. `pheme displays` reports what it
  finds; the user writes the number into the config.
- **Switching anything but the input source.** Brightness, contrast and
  power are all VCP features too, and all out of scope. The crate exposes
  one feature code (§3.1).
- **macOS.** `ddc-macos` exists and would fit the same trait, but pheme has
  no macOS support anywhere else and this sub-project does not add the
  first.
- **Recovering a monitor that answers nothing.** When DDC/CI does not work
  on a person's hardware, the feature turns itself off and says so once
  (§7). Input, audio and clipboard are unaffected.

## 2. What the hardware forces

DDC/CI travels on the I2C lines of the video cable itself, and on most
monitors only the **currently displayed input** answers. Every other
decision in this document follows from that one fact.

It means the machine that is about to be switched *away from* must issue
the command, because it is the one the monitor is listening to:

- Server hands the pointer to client C: **the server** commands
  `input = C`. The server is on screen, so the command lands.
- Client hands the pointer back: **the client** commands `input = server`.
  The client is on screen, so that command lands.

So the client needs the same monitor-control code as the server. The
feature is symmetric, and neither side can do the other's half.

It also means the obvious design for a recovery hotkey does not work. When
the monitor is stuck showing the client while the pointer is on the
server, the server cannot fix it: the server is not the displayed input,
so the monitor ignores everything it says. §8 resolves this by sending the
request to both machines and letting whichever is on screen carry it out.

## 3. The crate: `pheme-display`

### 3.1 The trait

```rust
/// VCP feature code for Input Select (MCCS 2.2 §8.4).
pub const INPUT_SELECT: u8 = 0x60;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// Nothing on this machine answered a read of VCP 0x60.
    #[error("no monitor answered DDC/CI")]
    NoMonitor,
    /// A monitor was found but `display.monitor` matched none of them.
    #[error("no monitor matches {0:?}")]
    NoMatch(String),
    #[error("DDC/CI: {0}")]
    Backend(String),
}

/// One monitor that answers DDC/CI.
pub trait Monitor: Send {
    /// A stable, human-readable name. Matched case-insensitively against
    /// `display.monitor`, and printed by `pheme displays`.
    fn identity(&self) -> &str;
    /// Where the backend found it: an i2c device path on Linux, the
    /// physical monitor description on Windows. Printed, and matched
    /// after the identity: a monitor whose EDID carried no name has its
    /// bus path for an identity, and nothing else to choose it by.
    fn location(&self) -> &str;
    fn get_input(&mut self) -> Result<u16, DisplayError>;
    fn set_input(&mut self, value: u16) -> Result<(), DisplayError>;
    /// The raw capability string, when the monitor returns one.
    fn capabilities(&mut self) -> Result<String, DisplayError>;
}

/// Every monitor on this machine that answers a read of VCP 0x60.
///
/// Slow: measured at 1.09 s on a two-output Linux laptop, because it opens
/// every i2c bus and reads an EDID from each. Never call it from a thread
/// that carries input. `DisplayService` calls it once, on its own thread,
/// at startup (§4).
pub fn enumerate() -> Vec<Box<dyn Monitor>>;

/// The monitor whose `identity()` or `location()` contains `want`
/// (case-insensitive), or the first one when `want` is `None`.
pub fn open(want: Option<&str>) -> Result<Box<dyn Monitor>, DisplayError>;
```

Answering a read of `0x60` is the membership test on purpose. A laptop's
internal eDP panel enumerates as an i2c bus and returns a valid EDID, but
it has no input to select and fails the read. Filtering on the read keeps
it out without special-casing panel types.

### 3.2 Backends

**Linux** (`backend_i2c.rs`, `#[cfg(target_os = "linux")]`): read
`/dev/i2c-*` with `std::fs::read_dir`, sort by bus number, and for each
call `ddc_i2c::from_i2c_device(path)`. Identity comes from the EDID
(§3.3); location is the device path.

**Windows** (`backend_winapi.rs`, `#[cfg(windows)]`):
`ddc_winapi::Monitor::enumerate()`. Identity and location are both
`Monitor::description()`, which is what the Win32 API offers.

**Anything else**: `enumerate()` returns an empty vector and `open()`
returns `NoMonitor`. The crate compiles everywhere; the feature is simply
never available.

`ddc::Ddc::{get_vcp_feature, set_vcp_feature}` and `ddc::Edid::read_edid`
are the only traits used, and both backends implement them, so the two
files differ only in how a handle is obtained.

### 3.3 Identity from EDID

Using the `edid` crate would pull `nom 3.2.1` into the build, which cargo
already reports as *"code that will be rejected by a future version of
Rust"*. The two fields needed are a fixed offset apart, so
`pheme-display` reads them itself:

```rust
/// A display name built from a raw EDID base block: the manufacturer id
/// (bytes 8-9), the descriptor tagged 0xFC (monitor name) and the one
/// tagged 0xFF (serial number).
///
/// `None` when the block is shorter than 128 bytes, the 8-byte header is
/// not `00 FF FF FF FF FF FF 00`, or the bytes do not sum to 0 mod 256.
pub fn identity_from_edid(edid: &[u8]) -> Option<String>;
```

Rules, all fixed by EDID 1.3/1.4:

- Manufacturer id: bytes 8-9 big-endian, three 5-bit letters, `A` = 1.
- Four 18-byte descriptors at offsets 54, 72, 90 and 108. A descriptor is
  a *display* descriptor when its first two bytes are zero; byte 3 is the
  tag; bytes 5..18 are the text, ended by `0x0A` and padded with `0x20`.
- Tag `0xFC` is the monitor name, tag `0xFF` the serial number.

Output is `"<manufacturer> <name> (<serial>)"`, dropping whichever of name
and serial is absent — `"GSM LG ULTRAGEAR (106NTMXE1579)"` on the machine
this was developed against. With neither, the identity is the manufacturer
id alone, and `display.monitor` matching falls back on `location()`.

### 3.4 Policy: `DisplaySwitch`

The policy is pure — no I/O, no clock of its own, `Instant` passed in — so
every rule below is a unit test that needs no monitor. It is the same
shape as `ClipSync` in sub-project 5, for the same reason.

```rust
pub struct DisplaySwitch {
    cooldown: Duration,
    /// The input the monitor is believed to be showing. `None` means
    /// unknown, which is also what a failed command leaves behind.
    selected: Option<u16>,
    /// When the last command was handed out.
    last_at: Option<Instant>,
    /// A value asked for during the cooldown and not yet handed out.
    pending: Option<u16>,
}

impl DisplaySwitch {
    pub fn new(cooldown: Duration) -> Self;
    /// Seed `selected` from a successful read at startup, or from the
    /// moment this machine becomes the displayed input (below).
    pub fn observe(&mut self, value: u16);
    /// A crossing asks for `value`. `Some(v)` means issue it now.
    pub fn request(&mut self, value: u16, now: Instant) -> Option<u16>;
    /// The recovery hotkey asks for `value`, ignoring rules 1 and 2.
    pub fn force(&mut self, value: u16, now: Instant) -> u16;
    /// Hand out a held request that has come due.
    pub fn poll(&mut self, now: Instant) -> Option<u16>;
    /// When `poll` could next return something.
    pub fn deadline(&self) -> Option<Instant>;
    /// The command the policy handed out reached the monitor.
    pub fn confirm(&mut self, value: u16);
    /// It did not. `selected` becomes unknown so rule 1 cannot refuse the
    /// retry.
    pub fn forget(&mut self);
}
```

Three rules:

1. **Never command the input already selected.** `request(v)` with
   `selected == Some(v)` and nothing pending returns `None`.
2. **Never command twice inside the cooldown.** Monitors take one to three
   seconds to switch and re-sync, and a second command arriving mid-switch
   is at best ignored.
3. **A request made during the cooldown is held, not dropped.** `pending`
   keeps the most recent one; `poll` hands it out when the cooldown
   elapses, and discards it if by then it equals `selected`.

Rule 3 is the one that has to be built this way. A leading-edge throttle —
the obvious implementation — drops the second request, so brushing the
edge and coming straight back leaves the monitor showing the machine the
pointer is no longer on, permanently. Holding the last request instead
means the final state always converges, and the discard clause means the
common "crossed out and back" case costs no command at all.

`confirm`/`forget` exist because of a defect shipped in sub-project 5:
`ClipSync` recorded text before the write that carried it succeeded, so
one failed write made the policy refuse every retry of the same text
forever. Here the equivalent would be worse — `selected` would name an
input the monitor is not showing, and rule 1 would refuse to correct it.
So the service records the value **only after** `set_input` returns `Ok`,
and calls `forget()` when it does not.

**How the belief learns what the peer did.** `selected` is a belief about a
resource *both* machines command, so it goes stale every time the peer
switches the monitor. Nothing in the list above tells it so, and a stale
belief is not harmless: rule 1 refuses a command for the input it thinks is
already selected, silently. Walk the first version of this design: the
client reads `0x11` at startup, the server switches the monitor to the
client's `0x0f` on the first crossing, and on the crossing back the client's
rule 1 refuses its own correct command because it still believes `0x11`. The
picture stays on the client while the pointer is on the server — the exact
failure this feature exists to prevent, after exactly one crossing.

There is one moment at which each machine can learn the answer, and it is
the same moment §2 gives it the power to act: **when it becomes the
displayed input**. Both sides know it exactly:

- the **client**, on `Msg::Enter` — the server has just commanded the
  monitor to the client's own input;
- the **server**, when the pointer returns to local (it sends `Msg::Leave`)
  — the client has just commanded the monitor to the server's own input.

`DisplayService::became_displayed(own_input)` carries it, alongside
`switch_to` and `force` and with the same never-blocks, never-fails
contract. The service thread answers it with `observe(own_input)`.

`own_input`, and deliberately **not** a fresh `get_input()`. The event is
the stronger evidence: it means the peer has just commanded the monitor to
this machine's cable. A read races that command — the peer issues it on its
own display thread and sends the crossing at once — so a read taken here can
still return the *peer's* input, and observing that is the very defect this
hook exists to fix. On the hardware §2 describes the read would merely fail
and fall back to `own_input`; on a monitor that answers from a non-displayed
input (§15) it succeeds with a stale value, which
`tests/display_crossing.rs` fails on deterministically. Believing
`own_input` wrongly — when the peer's command did not land — cannot wedge
anything, because the only command rule 1 can refuse is one for this
machine's own input, and a crossing never asks for that; it asks for the
peer's.

### 3.5 The mock

`MockMonitor` / `MockMonitorHandle`, mirroring `MockClipboard` from
sub-project 5: `input()`, `sets()`, `fail_with(DisplayError)`,
`stop_failing()`. The handle is cloneable and readable from the test
thread while the service thread owns the monitor.

## 4. The service

`crates/pheme-app/src/display.rs`:

```rust
pub struct DisplayService { tx: crossbeam_channel::Sender<Req> }

enum Req { Switch(u16), Force(u16), BecameDisplayed(u16) }

/// How the service acquires its monitor. Injected so the tests can hand it
/// a `MockMonitor`; production passes a closure over `pheme_display::open`
/// that has already captured `cfg.monitor`, which keeps the boxed closure
/// free of borrowed arguments and so `Send`.
///
/// `FnMut`, because §7's bounded retry calls it more than once.
pub type OpenFn =
    Box<dyn FnMut() -> Result<Box<dyn Monitor>, DisplayError> + Send>;

impl DisplayService {
    /// `None` when the feature is off, i.e. no `[display] input` in the
    /// config. This returns before the monitor is opened, so it cannot and
    /// does not report whether one answered: enumeration costs a second
    /// and runs on the spawned thread. A thread that finds no monitor says
    /// so once (§7), holds no handle and drops every later `switch_to`,
    /// and exits once its retries are spent.
    pub fn spawn(cfg: &DisplayCfg, open: OpenFn) -> Option<DisplayService>;
    /// Never blocks and never fails. A full queue drops the request:
    /// whatever filled it is a more recent intention than this one.
    pub fn switch_to(&self, value: u16);
    pub fn force(&self, value: u16);
    /// This machine is now the input the monitor is displaying, and it is
    /// cabled to `own_input` (§3.4). Reopens a monitor if none is held,
    /// and tells the policy what is on screen. Never blocks, never fails.
    pub fn became_displayed(&self, own_input: u16);
}
```

One dedicated `std::thread` owns the monitor handle and the policy, which
is the only way a 1.09-second enumeration and a multi-hundred-millisecond
I2C write can exist in this program at all. The channel is bounded at 4
and written with `try_send`, the same shape the status channel uses in
sub-project 6. **Nothing on the input path ever waits for this thread.**

The loop is `recv_timeout(deadline)`: a held request from rule 3 needs a
wakeup that no incoming message will provide. With nothing held it blocks
indefinitely.

On startup the thread runs the `OpenFn` it was given, then `get_input()` once
and `observe()`s the result if it succeeds. A failed read is not an error
— it leaves `selected` unknown, and the first crossing issues a command
that would otherwise have been deduplicated away. It is the only read the
service ever takes; after it, the policy learns the monitor's state from
`confirm`, `forget` and `became_displayed` (§3.4).

## 5. Where it hooks

No new message is needed for the automatic path. Both sites already exist
and already carry the clipboard for the same reason.

**Server** — `Shared::run_actions`, in the `Action::SendControl(m)` arm,
beside the `Msg::Enter` clipboard hook:

```rust
if matches!(m, Msg::Enter { .. }) {
    if let Some(c) = &self.clipboard { c.send_to(l.sender.clone()); }
    if let (Some(d), Some(v)) = (&self.display, l.display_input) {
        d.switch_to(v);
    }
}
```

**Client** — `client.rs`, the `matches!(m, Msg::Leave { .. })` arm that
already exists for the clipboard:

```rust
if matches!(m, Msg::Leave { .. }) {
    if let Some(c) = &clipboard { c.send_to(sender.clone()); }
    if let (Some(d), Some(v)) = (&display, server_display_input) { d.switch_to(v); }
}
```

Each side also answers the *opposite* transition — the one that makes it
the displayed input — with `became_displayed(own_input)` (§3.4): the server
in the same arm, on `Msg::Enter`'s mirror `Msg::Leave`, and the client on
`Msg::Enter`. Those two calls issue no command; they keep this machine's
belief about a monitor the peer also commands from going stale, and give a
machine that found no monitor at startup its chance to find one.

`display_input: Option<u16>` is a new field on the existing `Link` struct
(`server.rs:77`), set from the client's `Hello` when the link is built.
`run_actions` already clones the link at its top, so reading it costs no
lock that is not taken today, and a disconnect clears it for free by
replacing the whole `Link`. The app layer holds one link at a time, so one
value is enough even though the core tracks several configured clients.

The client keeps the server's value from `HelloAck` in a local variable
alongside the `clipboard` handle it already carries through the same loop.

## 6. Configuration

Each machine declares the input **it** is plugged into. That is a question
a person can answer by reading their monitor's on-screen menu, unlike
"which input is the other machine on", and it stays one number per machine
when a server has several clients configured.

```toml
[display]
# The VCP 0x60 value of the input THIS machine is cabled to.
input = 0x11
# Optional. A case-insensitive substring of the identity or the location
# `pheme displays` prints. Only needed when more than one monitor answers.
monitor = "ULTRAGEAR"
# Optional. Minimum gap between commands, milliseconds.
cooldown_ms = 1000
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct DisplayCfg {
    pub input: Option<u16>,
    pub monitor: Option<String>,
    pub cooldown_ms: u64,
}
```

added to `Config` as `#[serde(default)] pub display: DisplayCfg`. No
`[display]` section, or the section without `input`, means the feature is
off; that is the default and no existing config file changes meaning.

TOML accepts `0x11`; `toml::to_string_pretty` writes it back as `17`. That
is the same number and nothing depends on the spelling, but the GUI's Save
will rewrite a hand-written `0x11`, so the config comment above says what
the value is rather than leaving the reader to recognise hex.

`HotkeysCfg` gains `#[serde(default)] pub switch_display: Option<String>`,
defaulting to `None`. The attribute is explicit rather than load-bearing:
serde's derive already treats an absent `Option<T>` field as `None`, which
is how the existing `lock` field parses without one. It is written out so a
reader sees the intent at the field instead of having to know that rule.

## 7. When the hardware says no

On the machine this was designed on, `ddcutil detect` reports *"No
displays implementing DDC/CI found"* for a monitor whose EDID it reads
perfectly. `ddc-i2c` fails identically, so this is the hardware, not the
library. It is a common case and the feature must treat it as ordinary,
not exceptional:

- `enumerate()` finding nothing, or `open()` matching nothing, makes the
  service thread log **one** warning naming what was tried and hold no
  monitor. The hooks in §5 go on calling `switch_to`, and each call is
  discarded exactly as a full queue is.

  It does not exit on the spot, because on the hardware §2 is premised on a
  startup enumeration finding nothing may mean only that this machine was
  not the one on screen — and that is the machine, by §2, that will have to
  command the monitor later. So the open is retried at the one instant its
  answer can have changed: `became_displayed` (§3.4). **Nothing retries on
  a schedule** — there is no timer anywhere in this design — and nothing
  retries for ever: the open is attempted `OPEN_ATTEMPTS` = 3 times in all,
  once at startup and once at each of the first two crossings that put this
  machine on screen. Three covers a monitor that was still re-syncing on the
  first retry. When they run out the thread says so once more and exits,
  which disconnects the channel and makes every later send a dropped one,
  exactly as before. The bound is what keeps the common case — hardware that
  answers no DDC/CI at all — from spending 1.09 s of the display thread on
  every crossing for the life of the program.
- A `set_input` that fails logs a warning the first time and at `debug`
  after that, calls `forget()`, and leaves the service running.
- `switch_to` on a `None` service is a no-op at the call site, because the
  hook is inside `if let Some(d) = &self.display`.

Input, audio and clipboard never observe any of this.

## 8. The recovery path

The failure this exists for: the monitor is showing the client while the
pointer is on the server. The person cannot see the server's screen, so a
GUI button alone is useless — and, by §2, the server's own DDC command
cannot reach the monitor either.

So the hotkey asks **both** machines, and whichever is the displayed input
succeeds while the other fails harmlessly:

- `pheme-core` gains `Hotkeys.switch_display: Option<KeyCode>` and
  `Action::SwitchDisplay { local: bool }`, where `local` is
  `self.remote.is_none()` when the key went down. The core computes which
  machine holds the pointer; it never learns what a VCP value is.
  `server.rs:249` already tests hotkeys *before* the local/remote split,
  so the key is intercepted in both states and never forwarded.
- The app turns `local` into a target — its own `display.input` when
  `local`, the peer's when not — then calls `DisplayService::force` and
  sends `Msg::SwitchDisplay { input: target }` to the peer.
- A peer receiving `Msg::SwitchDisplay` calls `force` with the value it
  was given.

This is the only new message, and it is sent only by this hotkey, never on
a crossing.

The front-end gets the same reach the Lock command already has: a
`Command::SwitchDisplay` variant on the existing IPC enum, a "Switch
display" button beside Lock in the window and the tray, and a status line
naming the monitor and the input last confirmed. The button is the
convenience; the hotkey is the one that works when it matters, because it
needs no visible screen.

## 9. Protocol changes

```rust
Hello    { version, name, os, screens, audio, display_input: Option<u16> }
HelloAck { version, name, audio, display_input: Option<u16> }

/// Either direction: "if you are the input the monitor is showing, select
/// `input`". Sent by the recovery hotkey only (§8).
SwitchDisplay { input: u16 }
```

`Hello` and `HelloAck` already carry each end's capabilities — `audio:
AudioParams` is there so neither side transmits into a format the other
cannot read — and `display_input` is the same idea.

`PROTOCOL_VERSION` goes from 2 to 3. postcard is not self-describing, so
an added field is a wire change and both machines must upgrade together.
Both ends do compare versions and refuse a mismatch (`server.rs:749`,
`client.rs:388`), but that comparison is not what an out-of-date peer
meets on this particular step: `display_input` is appended to `Hello`, so
a version 2 `Hello` fails to decode before the version inside it is read,
and the error names neither version. The guard test at
`pheme-proto/src/lib.rs:434` asserting the version is updated in the same
commit, which is what it is for. `SwitchDisplay` is appended after the
last existing variant.

## 10. CLI

`pheme displays` — the command that makes the feature configurable:

```
IDENTITY                             LOCATION      CURRENT  SUPPORTED
GSM LG ULTRAGEAR (106NTMXE1579)      /dev/i2c-10   0x11     0x0f 0x11 0x12
```

`SUPPORTED` comes from the capability string: inside `vcp(...)`, find
`60(` and read space-separated hex bytes to the matching `)`. Monitors
that return no capability string, or one with no `60(...)` list, print
`-`; many do, which is why `CURRENT` is there — switching the input by
hand and re-running the command is the fallback way to learn a value.

`pheme setup` on Linux gains the i2c prerequisites: `i2c-dev` appended to
`/etc/modules-load.d/pheme.conf` (`MODULES_LOAD` becomes
`"uinput\ni2c-dev\n"`) and a second rule in `/etc/udev/rules.d/80-pheme.rules`:

```
KERNEL=="i2c-[0-9]*", GROUP="i2c", MODE="0660", TAG+="uaccess"
```

`TAG+="uaccess"` gives the logged-in user access without group
membership, matching what the uinput rule already does. Setup prints, but
does not run, `usermod -aG i2c <user>` as the fallback for a session that
`uaccess` does not cover.

The copy-and-paste instructions `setup.rs` prints when it is run without
root (`setup.rs:30-33`) name the file contents and the `modprobe` line, so
both change with the constants; the existing unit tests over `UDEV_RULE`
and `MODULES_LOAD` gain the i2c cases beside the uinput ones.

## 11. Dependencies

| Crate | Version | Target | Why |
|---|---|---|---|
| `ddc` | 0.2 | all | the `Ddc` and `Edid` traits |
| `ddc-i2c` | 0.2, `default-features = false, features = ["with-linux"]` | Linux | `/dev/i2c-*` |
| `ddc-winapi` | 0.2 | Windows | `SetVCPFeature` via Dxva2 |

All three are MIT, compatible with pheme's GPL-3.0.

`ddc-hi`, the high-level wrapper over exactly these three, is deliberately
**not** used. Measured: `ddc-hi` resolves to 43 crates against 16 for the
three above, and the 27 it adds include `nom 3.2.1` (cargo:
*"will be rejected by a future version of Rust"*), `serde_yaml 0.7.5`,
`yaml-rust`, `syn 3`, `memchr 1` and `libudev-sys` — a second native C
dependency on Linux beside GTK, on a project whose third priority is easy
installation. What it provides over the raw crates is bus enumeration via
udev and EDID parsing, which §3.2 and §3.3 replace in about sixty lines.

Both dependency sets were confirmed to build: `cargo check` clean on
Linux and on `x86_64-pc-windows-gnu`.

`default-features = false` on `ddc-i2c` drops its `with-linux-enumerate`
feature, which is what pulls udev.

## 12. Testing

Automated, none of it needing a monitor:

- `DisplaySwitch`, one test per rule: a repeat of the selected input is
  refused; a second request inside the cooldown is held, not issued; the
  held value is the most recent one, not the first; a held value equal to
  `selected` is discarded when it comes due; `forget()` lets an identical
  retry through where `confirm()` would refuse it; `force()` ignores both
  the dedupe and the cooldown.
- `identity_from_edid`: a synthetic valid block; a bad header; a bad
  checksum; a block with no `0xFC` descriptor; one with no `0xFF`.
- The capability-string input list: a real-shaped string, one with no
  `60(...)`, one that is truncated mid-list.
- **Both machines over one shared `MockMonitor`** (`display_crossing.rs`):
  a real `run_server` and a real `run_client` cross out, back, out and back,
  with the monitor's input asserted after each of the four transitions.
  This is the test the sub-project turns on: one crossing cannot show a
  stale belief about a monitor two machines command, and every test written
  before it passed with §3.4's defect in place. Deleting the client's
  `became_displayed` fails transition 2; deleting the server's fails
  transition 3.
- `DisplayService` over `MockMonitor`: `spawn` returns `None` when
  `display.input` is unset, and `Some` with an `OpenFn` that fails, whose
  `switch_to` is then a no-op rather than a panic or a block; a failing
  `set_input` does not stop the next
  identical request from reaching the monitor (the `forget` path, driven
  end to end rather than asserted on the policy alone); `force` reaches
  the monitor when `switch_to` for the same value would not; an open that
  found nothing is retried on `became_displayed` and works from then on;
  and that retry stops after `OPEN_ATTEMPTS`, observed through the
  `OpenFn`'s own drop when the thread gives up.
- Config: `[display]` with a hex `input`; an absent section leaving the
  feature off; `[hotkeys]` naming only `lock` still parsing.
- Proto: `Hello`/`HelloAck` round-trip with and without `display_input`;
  `SwitchDisplay` round-trip; the version guard reading 3.
- Core: the `switch_display` hotkey yields `Action::SwitchDisplay` with
  `local` true while local and false while remote, and is not forwarded to
  the client in either state.

Every one of these must be written so that a named single-line change to
the production code makes it fail. Sub-projects 5 and 6 shipped seven
tests that were green because they could not fail, and the counter-measure
is to name that change for each test in the plan.

## 13. Manual test matrix (added to `docs/testing.md`)

These are the only tests that exercise real DDC/CI. They need one monitor
cabled to both machines.

| # | Action | Pass |
|---|---|---|
| E1 | `pheme displays` on each machine, **without** switching the monitor to that machine first — run it on the machine the monitor is *not* showing | each monitor is listed with a plausible identity and a current input. Record, per machine, whether it answered while off screen: a machine that lists nothing until the monitor is switched to it is the case E1 exists to find, and the client is the machine it matters on |
| E2 | Set `display.input` on both, then cross the edge and back **twice**, slowly, waiting for the picture each time | all four transitions switch: out → the client, back → the server, out → the client, back → the server. The second round trip is the one that matters; the first one passed even with the defect this row was rewritten for |
| E3 | Restart the client while the monitor is showing the **server**, then cross the edge | the monitor shows the client. A client that enumerated no monitor at startup must still find one once it is on screen |
| E4 | Sweep the pointer across the edge and back inside one second | the monitor switches **at least once and at most twice**, and ends on the machine the pointer ended on. Zero switches is a failure, not a pass: a feature that is wedged also "ends" on the right machine |
| E5 | Press the `switch_display` hotkey while the monitor is on the wrong machine | the monitor corrects itself |
| E6 | Click "Switch display" in the window twice: once on the machine that both holds the pointer and is on screen, and once on the client while the client is on screen but the pointer is on the server | the first click leaves the monitor where it is, because it is already right; the second switches the monitor to the server. A click that moves the picture away from the machine holding the pointer is a failure |
| E7 | Run with `display.input` set where the monitor ignores DDC/CI | one warning at startup, nothing later, input and audio unaffected, and the window's Display row says no monitor answered rather than "nothing commanded yet" |
| E8 | Remove `[display]`, cross the edge | the startup log never says "display switching is on", no DDC warning appears, and everything else is unchanged |
| E9 | Unplug the monitor's second cable, cross the edge | the failed command warns once and does not repeat on later crossings |
| E10 | Give both machines the same `display.input` and connect | the "same monitor input" warning appears on the server |

## 14. Definition of done

- `pheme-display` builds on Linux and Windows and has no monitor in any
  test.
- Crossing the edge switches the monitor in both directions, with the
  policy's three rules under unit test.
- The recovery hotkey and the GUI button both work, and the hotkey reaches
  the machine that is on screen.
- `pheme displays` prints enough to configure the feature.
- `pheme setup` leaves `/dev/i2c-*` accessible.
- The feature is off by default and turns itself off, once and loudly,
  where the hardware does not support it.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` clean on both CI legs; `Cargo.lock` committed
  with the manifest change.
- `README.md` documents `[display]`; `docs/testing.md` carries E1–E9;
  architecture §9 lists this sub-project and renumbers packaging to 8.

## 15. Known risks

- **The monitor may not answer at all.** Proven on the development
  machine, where `ddcutil` and `ddc-i2c` fail identically. §7 makes this
  ordinary rather than fatal, but it means the automated tests can never
  tell us the feature works — E1–E10 are the only evidence there will be.
- **Input values are vendor-specific.** MCCS assigns 0x0F to DisplayPort-1
  and 0x11 to HDMI-1, and vendors disregard it freely. Hence `pheme
  displays`, hence a raw number in the config rather than a friendly name.
- **Some monitors answer on a non-displayed input.** The design no longer
  depends either way on whether they do. It used to: a machine that could
  not open a monitor at startup was off for good (§7), which meant the
  feature worked end to end only where this risk was *realised* — where the
  premise in §2 was false. §7's bounded retry removes that dependence, so
  both kinds of hardware now work.

  What remains of the risk is contention: where both machines' commands
  land, they could fight. Rules 1 and 2 bound that to one redundant command,
  and the hotkey's deliberate both-machines broadcast is harmless for the
  same reason. The one place it still bites is a *read*, which is why
  §3.4's `became_displayed` does not take one: on such a monitor a read can
  answer with the input the peer is about to switch away from.
- **i2c bus numbers move between boots.** `display.monitor` matches the
  EDID identity, never the bus path, so a renumbered bus changes nothing.
- **A DDC write can block for hundreds of milliseconds** under GPU load.
  It happens on the service thread and the input path never waits on it
  (§4); the bounded channel drops rather than queues.
- **GTK is already a hard link-time dependency of the whole binary**
  (sub-project 6). This sub-project adds no native dependency on Linux —
  `ddc-i2c` without `with-linux-enumerate` links nothing — but the
  server-only build question raised at the end of sub-project 6 remains
  open and belongs to packaging.
