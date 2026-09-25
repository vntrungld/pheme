# Sub-project 6 — Tray and configuration GUI

Date: 2026-09-25
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-25-clipboard-discovery-design.md`
Expected outcome: running `pheme` with no subcommand puts an icon in the
system tray and opens a window that shows what Pheme is doing, pairs
with the other machine, and edits the configuration — enough that a
person who never opens a terminal can set Pheme up and watch it work.

## 1. Scope

In:

- `pheme` with no subcommand: a front-end process that owns a tray icon
  and a window, and runs the existing server or client as a child
  process.
- A local-socket protocol between the two, carrying status one way and
  commands the other.
- A window with three panels: status, pairing, configuration.
- Writing `config.toml` from the window, atomically.
- Audio device enumeration in `pheme-audio`, and the `pheme devices`
  subcommand that architecture §12 has always listed but which has never
  existed. The configuration panel's device menus need it.

Out:

- **Packaging (sub-project 7).** The Inno Setup installer, the systemd
  `--user` unit, start-with-Windows, and wiring either into
  `release.yml`. The installer has to know what it is installing —
  shortcuts, autostart and a start-menu entry differ between a CLI and a
  tray application — so it follows this sub-project rather than leading
  it.
- **A drag-and-drop screen layout.** The core places one client on one
  edge; a canvas would express more than the core can do.
- **Editing the configuration while the core runs.** A configuration
  change restarts the child (§8). Nothing becomes reloadable.
- **macOS.**

## 2. Why two processes

The front-end could have hosted the runtime in-process. It does not, for
three reasons, in the order they matter for this project:

1. **Input latency is the first priority.** A repaint, a font atlas
   rebuild or a blocked window event loop can take tens of milliseconds.
   In one process those share a CPU and a scheduler with the thread that
   forwards keystrokes. In two they cannot.
2. **Three components each want a thread they consider theirs.** `eframe`
   wants the main thread for its window, the Windows low-level keyboard
   hook wants a message pump on the thread that installed it, and the
   InputCapture portal runs its own async loop. Separating the processes
   removes that argument entirely rather than arbitrating it.
3. **The existing binary keeps working untouched.** `pheme server`,
   `pheme client` and the rest are what the front-end runs. They gain one
   flag and no new behaviour, so every path this project has already
   tested stays on the same code.

The cost is a protocol and a child process to supervise, which §4 and §3
spend.

## 3. The front-end and the core

`pheme` with no subcommand is the **front-end**. It:

1. Loads the configuration and decides the role from `role`.
2. Creates the IPC listener (§4).
3. Spawns itself as a child — the same executable, with the subcommand
   the role implies and one added flag, `--ipc <path>`.
4. Shows the tray icon, and the window when asked.
5. On exit, closes the listener, waits up to two seconds for the child
   to exit on its own, and kills it if it has not. Two seconds is far
   longer than a clean shutdown takes and short enough that quitting
   never feels stuck; a core that ignores it is a bug worth the kill.

The **core** is `pheme server` or `pheme client` exactly as they are
today, plus `--ipc <path>`. When the flag is present the core connects
to that path at startup, pushes status, and obeys commands. When it is
absent — every existing invocation — nothing changes.

`--ipc` is hidden from `--help` (`#[arg(hide = true)]`). It is not a
thing a person invokes; it is how the front-end talks to its own child,
and documenting it would invite someone to run a core pointed at a
socket nothing is serving.

**The core exits when the IPC connection closes.** The front-end is its
supervisor: closing the window's application quits sharing. This keeps
the lifetime trivially correct, with no pidfile, no adoption protocol and
no orphaned process left holding a uinput device or a global keyboard
hook, which is a far worse failure than an application that stops when
you quit it. A person who wants Pheme running without a GUI runs
`pheme server` from a terminal or from the systemd unit sub-project 7
adds; that path is untouched.

The consequence, stated plainly because it is the cost of this choice: if
the front-end crashes, sharing stops. A front-end that merely *stalls*
does not affect input, which is the property §2 is about.

## 4. IPC

`crates/pheme-app/src/ipc.rs`.

**Transport.** A Unix domain socket on Linux at
`$XDG_RUNTIME_DIR/pheme/ipc-<pid>.sock` (falling back to `/tmp` when the
variable is unset), and a named pipe on Windows at
`\\.\pipe\pheme-<pid>`. Both come from `tokio::net`, which is already a
dependency: `UnixListener` and `windows::named_pipe`. No new crate.

The front-end listens and the core connects, so the core never has to
guess when the front-end appeared, and the path carries the front-end's
process id so two front-ends do not collide.

**Framing.** The same shape as the control stream: a length prefix and a
postcard body. The types are *not* `pheme_proto::Msg`. That enum is the
protocol between two machines and must not grow fields that exist only
for a local GUI.

```rust
/// Pushed by the core once a second, and once immediately on connect.
pub struct Status {
    pub role: Role,
    pub state: LinkState,
    /// The peer's name once connected.
    pub peer: Option<String>,
    pub rtt_us: u64,
    pub locked: bool,
    /// Input messages sent or received in the last second.
    pub events: u64,
    /// Input messages the client believes were lost, cumulative.
    pub lost: u64,
    pub audio_depth_ms: u32,
    pub audio_lost: u64,
    pub mic_depth_ms: u32,
    pub mic_lost: u64,
}

pub enum LinkState {
    Starting,
    Listening,
    Connecting,
    Connected,
    /// The core is running but the link failed; the string is why.
    Failed(String),
}

/// Sent by the front-end.
pub enum Command {
    Lock,
    Unlock,
    /// Stop cleanly. The core exits; the front-end restarts it after a
    /// configuration change.
    Stop,
}
```

The fields mirror what `--stats` already logs, so the window shows the
numbers the project already trusts rather than a second set.

Not every field is meaningful to every role, and the spec says which
rather than leaving the implementer to guess. `lost` is counted only by
the client, which numbers the gaps in the server's sequence; a server
sends `0`. `audio_*` describes the stream the server plays and `mic_*`
the stream the client plays, so each side fills the pair it owns and
sends `0` for the other. A zero therefore means "not measured here", and
the window labels a field by role rather than showing a misleading nought
for something the other end would have counted.

**Failure.** A frame longer than `MAX_IPC_FRAME` (64 KiB, a constant in
`ipc.rs`) is refused without being decoded, and a malformed one is
refused after. Either closes the connection, and a closed connection
exits the core, as §3 says. Sixty-four kibibytes is roughly a thousand
times the largest `Status` and exists to bound a reader, not to carry
anything. The front-end shows the core as
stopped and offers to start it again. Nothing about IPC can reach the
wire protocol or the input path.

## 5. The tray

Crate `tray-icon`, which covers Windows and Linux behind one API.

**This adds runtime dependencies on Linux**: `libappindicator3` and
GTK 3, because that is how `tray-icon` reaches the Linux tray. A tarball
is therefore no longer self-contained on Linux, and sub-project 7's
packaging and the README must say which packages to install. This was a
deliberate trade: the alternative was writing two tray backends (`ksni`
over D-Bus, and `Shell_NotifyIcon` through the `windows` crate) to keep
GTK out, at roughly 230 more lines.

On GNOME the tray needs the AppIndicator shell extension, which GNOME
does not ship enabled. The window is reachable without the tray, so a
missing tray is a degraded mode, not a failure: if the icon cannot be
created, log one warning and open the window directly.

Menu: **Open**, **Lock input** (a checkmark that mirrors `Status.locked`),
**Stop** / **Start**, **Quit**. The icon shows connected or disconnected.

## 6. The window

`eframe` and `egui`. Opened from the tray, and on first run when no
configuration file exists. Closing it hides it; **Quit** exits.

**Status panel.** Role, link state, peer name, RTT, and the counters from
`Status`. When `state` is `Failed`, its message, since that is the thing
a person needs and today it exists only in the log.

**Pairing panel.** On a server, a button that shows a pairing code and
waits for one pairing, which is what `pheme server --pair` already does.
On a client, the list of servers found over mDNS — `pheme_net::discovery::browse`
already returns name, address and fingerprint — a field for the code, and
a button that runs `pheme_net::pairing::client_pair` in the front-end
process. The front-end already links `pheme-net`, so nothing shells out.

**Configuration panel.** `role`, `name`, `listen` or `connect`, the
client list with each entry's `side` and `span`, the lock hotkey, and the
three audio device menus filled from §7. Saving writes the file (§8) and
restarts the child.

Validation happens before the file is written, through the same checks
`Config` already performs when loading, so the window cannot produce a
file the CLI would then refuse.

## 7. Device enumeration

New in `pheme-audio`:

```rust
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,   // Playback | Capture
    pub is_default: bool,
}

pub fn list_devices() -> Result<Vec<DeviceInfo>, Error>;
```

- **Linux**: a PipeWire registry listener collecting nodes whose
  `media.class` is `Audio/Sink` or `Audio/Source`, using the same
  `pipewire` binding the playback and capture paths already use.
- **Windows**: `IMMDeviceEnumerator::EnumAudioEndpoints` over `eRender`
  and `eCapture`, with `GetDefaultAudioEndpoint` marking the default.

This is the second-largest piece of this sub-project, and it is two
platform backends rather than one abstraction over an existing crate.
It exists because the configuration panel's device menus are unusable
without it, and because `pheme devices` has been in architecture §12
since the beginning and has never been implemented.

`pheme devices` prints the list as a table. The window uses the same
function. An empty device name in the configuration continues to mean
"the operating system default", as it does today.

## 8. Writing the configuration

```rust
impl Config {
    /// Writes to `path` atomically: a temporary file in the same
    /// directory, then a rename.
    pub fn save(&self, path: &Path) -> anyhow::Result<()>;
}
```

The rename is what makes it safe. Writing in place means a crash or a
full disk between truncating and finishing leaves a configuration file
that no longer parses, and the next start has no configuration at all —
on the one path whose whole job is to keep the user out of a text editor.

`Config` already derives `Serialize`, so the body is
`toml::to_string_pretty` plus the temporary-file dance.

Comments in a hand-written `config.toml` do not survive a save. The
window says so before its first write, because silently discarding what
someone wrote by hand is worse than warning them once.

After a successful save the front-end sends `Command::Stop`, waits for
the child, and starts it again with the new configuration.

## 9. What does not change

- `pheme-core` gains no code.
- The wire protocol between machines does not change.
- Every existing subcommand behaves exactly as it does today; `--ipc` is
  the only addition and it is absent unless the front-end passes it.
- `pheme setup` is unchanged and still the way OS prerequisites are
  installed.

## 10. Dependencies

| Crate | Version | Where | Why |
|---|---|---|---|
| `eframe` | 0.32 | `pheme-app` | the window; pulls `egui`, `winit`, `glow` |
| `tray-icon` | 0.24 | `pheme-app` | the tray on both platforms (§5) |

Both are MIT OR Apache-2.0. `tray-icon`'s Linux path brings
`libappindicator` and GTK 3 as system libraries (§5).

## 11. Testing

Automated, and therefore on CI:

- IPC: every `Status` and `Command` survives an encode/decode round
  trip; an oversized frame is refused; a truncated frame does not panic.
- `Config::save`: load → save → load yields an equal `Config`; a save
  over an existing file leaves the old contents intact when the write
  fails; the temporary file does not survive a successful save.
- Front-end and a stub core over a real socket, headless: the front-end
  receives status, sends a command and sees it acted on, and observes
  the core exiting when the socket closes.
- `list_devices` is not tested on CI — no runner has an audio device —
  but any parsing or sorting layer around it is.

Not on CI, and this is a larger gap than in previous sub-projects: the
tray, the window, and every device backend need a desktop session. CI
proves this sub-project compiles and that its protocol and file handling
are correct. It proves nothing about whether the application works.

## 12. Definition of done

- `pheme` with no subcommand shows a tray icon and opens a window.
- The window shows live status from a real core over the socket.
- A server can show a pairing code and a client can pair from the
  window, without a terminal.
- The configuration panel writes a `config.toml` that the CLI accepts,
  and the child restarts with it.
- `pheme devices` lists devices on Linux and Windows.
- Quitting the front-end leaves no process behind.
- A tray that cannot be created leaves the window working.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` clean on Linux and
  Windows.
- `README.md` documents `pheme` with no subcommand, and the GTK and
  AppIndicator requirements on Linux.
- `docs/testing.md` carries G1–G10.

## 13. Manual test matrix (added to `docs/testing.md`)

| ID | What | Pass |
|---|---|---|
| G1 | `pheme` with no config file, Linux KDE | the window opens on first run; the tray icon appears |
| G2 | `pheme` on Windows | the same |
| G3 | Pair two machines entirely from the windows | both ends end up paired; `pheme client` then connects |
| G4 | Edit the client list and save | `config.toml` parses, the child restarts, the new edge works |
| G5 | Save over a config file containing comments | the warning appears before the first write |
| G6 | Quit from the tray | no `pheme` process remains |
| G7 | Kill the front-end with SIGKILL | the core exits within a second; no orphan |
| G8 | Lock from the tray while the pointer is on the client | the checkmark and the core agree; unlocking restores |
| G9 | `pheme devices` on both platforms | the list matches what the OS sound settings show |
| G10 | GNOME without the AppIndicator extension | one warning, no tray, the window still opens |

## 14. Known risks

- **The tray on GNOME** needs an extension GNOME does not enable. §5's
  degraded mode is the answer, and G10 checks it.
- **GTK 3 becomes a runtime dependency on Linux**, so the release tarball
  is no longer self-contained. Sub-project 7 must state the packages.
- **`eframe` under Wayland with fractional scaling** has a history of
  blurry or mis-sized windows. If it lands badly, the fallback is to run
  the window under XWayland, which costs nothing here.
- **Device enumeration is two platform backends** with no crate to lean
  on and no CI coverage, which makes it the most likely part to be wrong
  in a way only a person notices.
- **A configuration written by the window discards comments.** Warned
  once (§8), and the CLI path remains for anyone who wants to keep them.

## 15. Deferred to sub-project 7

The Inno Setup installer, start-with-Windows, the systemd `--user` unit,
the GTK and AppIndicator packaging notes, and wiring all of it into
`release.yml`.
