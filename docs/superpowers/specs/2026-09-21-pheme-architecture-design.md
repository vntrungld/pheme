# Pheme — Overall Architecture

Date: 2026-09-21
Status: approved in brainstorming, awaiting spec review
License: GPL-3.0-only

## 1. Goals

Pheme is a Deskflow-style keyboard/mouse sharing tool (switch machines
when the pointer hits a screen edge) combined with bidirectional audio
forwarding:

- **Server**: the machine with the physical keyboard, mouse, speakers
  and microphone.
- **Client**: a "headless" machine — receives input from the server;
  audio the client plays is heard on the server's speakers; the server's
  microphone appears as a virtual microphone on the client.

Priorities, in order:

1. Lowest possible input latency on a LAN.
2. Highest audio quality (bit-exact, no lossy codec).
3. Easy installation on Windows and Linux (X11 and Wayland). macOS later.

Any machine can act as server or client (single binary, role chosen at
launch). v1: one server + one client on the same LAN; the protocol and
layout allow multiple clients but that is not tested.

## 2. Out of scope for v1

- macOS (the traits allow adding a backend later).
- Client-to-client adjacency (clients are only adjacent to the server).
- Operation over the Internet / NAT.
- A Windows virtual audio driver (VB-CABLE is used instead, see §7).
- Image/file clipboard (v1 is text only).

## 3. Key technical decisions

| Topic | Decision | Rationale |
|---|---|---|
| Language | Rust, Cargo workspace | Static binaries, easy cross-compilation, performance, memory safety |
| Transport | QUIC (`quinn` + `rustls`) | Unreliable datagrams for input/audio, reliable streams for control/clipboard, TLS 1.3 built in |
| Audio codec | PCM 48 kHz stereo i16, 5 ms frames, uncompressed | LAN has ample bandwidth (~1.5 Mbps per direction); zero codec latency; bit-exact |
| Serialization | `postcard` | Input packets are ~10 bytes |
| Keys | USB HID usage codes, mapped to scancode/evdev in the backend | Keyboard layout is decided by the client, like a physical USB keyboard |
| Linux inject | uinput | Works on X11, Wayland and every compositor; install = one udev rule |
| Linux capture | X11: XInput2 + XTest; Wayland: xdg-portal InputCapture/libei (GNOME/KDE), layer-shell (wlroots) | X11 first; Wayland is its own sub-project |
| Windows input | Low-level hooks + Raw Input (capture), SendInput (inject) | Standard approach, same as Deskflow |
| Audio Linux | PipeWire (`pipewire-rs`): virtual `Audio/Sink` + `Audio/Source` nodes on the client | No driver; the user picks the devices in the OS |
| Audio Windows | WASAPI (`wasapi` crate); virtual mic via VB-CABLE | Windows has no userspace API for virtual audio devices |
| Clock sync | Adaptive jitter buffer + drift-compensating resampler (`rubato`) | Keeps latency stable, no clicks |
| GUI | `tray-icon` + `eframe/egui` | Pure Rust, no webkit2gtk dependency |
| Auth | Pairing with a 6-digit code over SPAKE2, then pinned certificate fingerprints (mTLS) | Unknown machines on the LAN are rejected at the TLS layer |
| Discovery | mDNS `_pheme._udp.local` (`mdns-sd`) | No need to type IP addresses |

Implementation references: `lan-mouse` (Rust, GPLv3 — code may be
borrowed), Deskflow (GPL-2.0-only — reference API usage only, do not
copy).

## 4. Workspace layout

```
pheme/
├── Cargo.toml                 workspace
├── crates/
│   ├── pheme-proto/           Msg enum, postcard encode/decode, version
│   ├── pheme-net/             QUIC transport, pairing, mDNS, reconnect
│   ├── pheme-input/           InputCapture / InputInject traits + per-OS backends
│   │   └── src/{keymap,windows,linux_x11,linux_uinput,linux_wayland,mock}/
│   ├── pheme-audio/           AudioCapture / AudioPlayback / VirtualSource traits,
│   │   └── src/{jitter,drift,pipewire,wasapi,mock}/
│   ├── pheme-core/            ScreenLayout, ActiveScreen state machine,
│   │                          shadow key state — NO cfg(target_os)
│   └── pheme-app/             `pheme` binary: CLI, TOML config, tray + egui
└── docs/
    ├── superpowers/specs/     per-sub-project specs
    └── testing.md             manual cross-OS checklist
```

Principles:

- `pheme-core` is pure logic: `fn on_event(&mut self, ev) -> Vec<Action>`;
  it never calls the OS and is testable on every platform.
- OS backends only implement traits; `pheme-app` selects a backend via
  cfg at compile time and auto-detects at runtime (`$WAYLAND_DISPLAY`,
  `$XDG_SESSION_TYPE`).
- Every crate has a `mock` backend for integration tests that do not
  touch the OS.

## 5. Data flow

```
SERVER                                          CLIENT
InputCapture ─► core::Router ─► QUIC ──────────► core::Applier ─► InputInject
AudioPlayback ◄─ Jitter ◄─ QUIC datagram ◄────── AudioCapture (Pheme Speaker / loopback)
AudioCapture(mic) ─► QUIC datagram ─► Jitter ──► VirtualSource (Pheme Mic)
Clipboard ◄──────── QUIC stream (both ways) ───► Clipboard
Control   ◄──────── QUIC bi-stream ────────────► Control
```

Threading: tokio for network/control. Input capture runs on the OS's own
thread (hook thread / X event loop) and pushes into a bounded lock-free
channel; the network task sends immediately, no batching. Audio callbacks
(realtime) only read/write a lock-free ring buffer (`rtrb`), never
allocate, never touch tokio.

## 6. Protocol (summary — details in the sub-project 1 spec)

Channels on a single QUIC connection:

| Channel | Kind | Content |
|---|---|---|
| Control | first bi-stream | Hello, HelloAck, Ping/Pong, Key, Button, Enter, Leave, Bye |
| Input motion | datagram | MouseMove (relative), Wheel |
| Audio Out | datagram | AudioFrame{Playback} |
| Audio Mic | datagram | AudioFrame{Mic} |
| Clipboard | one uni-stream per change | ClipboardData |

`Key`/`Button`/`Enter`/`Leave` go over the reliable stream so no key-state
resync mechanism is needed; only mouse motion and audio use datagrams
(packet loss is acceptable there).

## 7. Audio — always on

Audio runs as soon as a connection exists; there is no on/off flag. The
client exposes virtual devices; the user selects them in the OS sound
settings. The server uses its default output/input (overridable by device
name in the config).

| | Linux (PipeWire) | Windows |
|---|---|---|
| Client speaker → server | `Pheme Speaker` node (`Audio/Sink`) | WASAPI loopback on the default output (user picks any device; audio still plays locally if the device has speakers). Optional: set `audio.capture_device` to the cable's playback half (`CABLE Input`) to make the client fully silent — loopback attaches to a render endpoint, so naming the recording half matches nothing |
| Server mic → client | `Pheme Mic` node (`Audio/Source`) | Render into `CABLE Input` (VB-CABLE, free); apps use `CABLE Output` as the mic |

When the connection drops, the virtual devices stay (Speaker swallows
audio, Mic emits silence) so the OS does not fall back to another
default; on reconnect audio simply resumes.

Pipeline per direction: capture callback → ring → network task packs 240
samples/channel + seq + ts → datagram → JitterBuffer (target 10 ms, grows
to 40 ms, repeat-and-fade PLC on loss) → drift resampler (`rubato`,
±0.1 %) → ring → playback callback. Expected latency 20–25 ms per
direction.

## 8. Security

- Identity: self-signed Ed25519 certificate generated on first run,
  stored in `~/.config/pheme/identity.*` (Windows: `%APPDATA%\pheme\`).
- Pairing: the server shows a 6-digit code; the client runs
  `pheme pair <host> <code>`; SPAKE2 with the code → shared key →
  fingerprints confirmed with an HMAC. The code is single-use.
- After pairing: the peer fingerprint is stored in `trusted.toml`; rustls
  pins fingerprints in both directions (mTLS). Connections from unknown
  certificates are rejected.

## 9. Sub-project roadmap

Each sub-project gets its own spec and plan, building on the previous one.

1. **KVM core** — proto, net (QUIC, pairing, reconnect), core state
   machine, lock hotkey, Windows + Linux X11 capture, uinput inject, CLI.
   Outcome: keyboard/mouse usable across the screen edge.
2. **Audio out** — client → server: PipeWire sink / WASAPI loopback,
   jitter buffer, drift, playback on the server.
3. **Virtual mic** — server → client: PipeWire source / VB-CABLE render.
4. **Wayland capture** — portal InputCapture + libei (GNOME/KDE),
   layer-shell (Hyprland/Sway).
5. **Text clipboard + mDNS.**
6. **Tray + egui config GUI**, Windows installer (Inno Setup), systemd
   user unit, CI release.

## 10. Testing

| Layer | Method |
|---|---|
| proto | round-trip every variant; input packet ≤ 16 B, audio ≤ 1200 B |
| core | state-machine unit tests: enter/leave edges with `span`, coordinate mapping, lock, key release on disconnect, no switch when the motion vector does not point outward |
| input keymap | round-trip HID ↔ evdev ↔ Windows scancode, no duplicates |
| net | two quinn endpoints on localhost: pairing with right/wrong code, unknown cert rejected, reconnect |
| audio | JitterBuffer with simulated loss/reorder/late sequences; drift resampler keeps latency stable with 0.1 % clock skew |
| integration | server + client in one process with `mock` backends over real QUIC; measure input RTT |

**Mandatory cross-OS matrix before every release** (manual, `docs/testing.md`):

| Server → Client | Input | Audio out | Mic | Clipboard |
|---|---|---|---|---|
| Linux X11 → Windows | ✔ | ✔ | ✔ | ✔ |
| Windows → Linux (X11 and Wayland sessions) | ✔ | ✔ | ✔ | ✔ |
| Linux → Linux | ✔ | ✔ | ✔ | ✔ |
| Windows → Windows | ✔ | ✔ | ✔ | ✔ |

Pass criteria (examples): 100 keystrokes without a stuck key; holding
Shift across the edge does not stick; unplug the client's network → the
server regains its mouse in < 5 s; no audio clicks or drift after 30
minutes; input RTT < 1 ms over cable.

Measurement: `tracing`; a `--stats` flag prints RTT, loss, jitter depth
and CPU every second.

## 11. Installation & distribution

- Linux: binary (dynamically linked to libpipewire, libX11), tarball;
  `pheme setup` writes the udev rule `/etc/udev/rules.d/80-pheme.rules`
  (`KERNEL=="uinput", MODE="0660", GROUP="input", TAG+="uaccess"`) and
  adds the user to the `input` group; sample systemd `--user` unit.
  AUR/Flatpak later.
- Windows: `.exe` + Inno Setup, optional start-with-Windows, link to
  install VB-CABLE; `pheme setup` checks that VB-CABLE is present.
- CI: GitHub Actions build + test on Linux/Windows, release on tag.

## 12. CLI & config

```
pheme server [--config path]
pheme client <host|name> [--config]
pheme pair <host> <code>
pheme discover
pheme setup
pheme devices
pheme                       # tray + config, role from config (SP6)
```

```toml
role = "server"                # "server" | "client"
name = "desk-linux"
listen = "0.0.0.0:24800"       # server
connect = "laptop-win"         # client: mDNS name or IP

[hotkeys]
lock = "ScrollLock"

[[clients]]                    # server
name = "laptop-win"
side = "right"                 # left | right | top | bottom
span = [0.0, 1.0]              # optional: portion of the edge

[audio]                        # optional; empty = OS default device
capture_device = ""            # Windows client: empty = loopback of default output
virtual_mic_device = "CABLE Input"
playback_device = ""           # server
mic_device = ""                # server
```
