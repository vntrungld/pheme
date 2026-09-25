# Pheme

Share one machine's keyboard and mouse with another over the LAN, Deskflow-style,
with audio forwarded in both directions. Written in Rust. GPL-3.0.

Status: early development — see `docs/superpowers/specs/` for the design.

## Build

    cargo build --release

## The tray and window

Running `pheme` with **no subcommand** opens a tray icon and a window,
instead of any of the subcommands below. The window has three panels:
status (role, link state, peer, RTT and the traffic counters), pairing
(a button that shows a pairing code on a server, or the list of
servers found over mDNS on a client), and configuration (`role`,
`name`, `listen` or `connect`, the client list, the lock hotkey and the
three audio device menus). Saving the configuration panel writes
`config.toml` and restarts the server or client underneath it to pick
the change up; nothing reloads live.

The tray's menu is **Open**, **Lock input**, **Start**/**Stop** and
**Quit**. Closing the window hides it, where the platform allows it —
sharing keeps running, and the tray brings it back. **Only Quit exits
the application.** On Wayland, hiding a window is a no-op winit does
not implement there, so the window stays open instead and shows
"Pheme is still running. Use Quit in the tray to exit." Where there is
no tray to reopen the window from at all (see the GNOME note below),
closing the window exits instead, since the application would
otherwise be unreachable.

**Closing the front-end stops sharing.** `pheme` with no subcommand
runs the same server or client as a child process, and that child
exits the moment its connection back to the front-end closes — so
quitting the tray, or killing the front-end, always leaves nothing
running behind it. To run Pheme without a GUI, run `pheme server` or
`pheme client` directly from a terminal, exactly as below; every
existing subcommand is unchanged.

### Linux runtime dependencies

`pheme` links against GTK 3 directly, so it has to be installed to run
the binary **at all** on Linux — every subcommand, not only the tray
and window. `libappindicator` (its Ayatana fork is tried first) is
loaded by the tray specifically, the moment one is actually built;
install both before running `pheme` rather than after something fails:

```bash
# Debian/Ubuntu
sudo apt install libgtk-3-0 libayatana-appindicator3-1
# Arch
sudo pacman -S gtk3 libayatana-appindicator
# Fedora
sudo dnf install gtk3 libappindicator-gtk3
```

The release tarball does not bundle either. Building from source on
Linux needs the matching `-dev` packages, plus a few X11/xcb headers
`eframe` needs — see the packages `.github/workflows/ci.yml` installs
before `cargo build`.

**GNOME does not ship the AppIndicator shell extension enabled**, so
the tray icon never appears there, even with the library installed —
`pheme` notices there is no host for it, logs one warning, and opens
the window directly instead. Every panel in the window still works;
installing an AppIndicator extension (for example "AppIndicator and
KStatusNotifierItem Support" from extensions.gnome.org) is what brings
the tray icon back.

### `pheme devices`

    pheme devices

Lists the audio devices this machine currently offers — playback
devices, then capture devices, each marked as the operating system's
default or not. The NAME column is exactly what `playback_device`,
`capture_device` and `mic_device` in `config.toml` match against (see
"Audio" below), or a distinctive part of it. The configuration
window's three device menus are filled from this same list.

## Run (keyboard/mouse sharing)

1. **Server** (the machine with the keyboard and mouse), once:
   `pheme server --pair` → note the 6-digit code.
2. **Client**, once: `pheme pair <server-ip> <code>`.
   On Linux also run `sudo pheme setup` and log out/in (uinput permissions).
3. Put a config on the server (`~/.config/pheme/config.toml`, or
   `%APPDATA%\pheme\config.toml` on Windows):
   ```toml
   role = "server"
   name = "desk"
   [[clients]]
   name = "laptop"      # must match the client's `name`
   side = "right"       # left | right | top | bottom
   ```
4. `pheme server` on the server, `pheme client <server-ip>` on the client. Push the
   pointer through the configured edge.

`ScrollLock` toggles the input lock. `--stats` prints RTT and traffic counters.

## Finding the server

A server advertises itself on the local network by default, so a client can
find it by name instead of by IP address. `pheme discover` lists what it can
see:

    pheme discover

`connect` (in the config, or the `host` argument to `pheme client` and
`pheme pair`) takes either a name or an address. A name is resolved again on
every reconnect attempt, so a server that changes address, or restarts on a
different port, is reachable again without restarting the client.

Discovery uses mDNS on UDP port 5353. A firewall blocking that port makes
`pheme discover` come back empty even though the server is running; the
fallback is to put the server's address in `connect` directly. Set
`discovery = false` in the server's config to turn advertising off.

## Wayland capture (server)

A Linux **server** running under a Wayland session uses the
`org.freedesktop.portal.InputCapture` portal instead of X11's XInput2.
This works on the compositors that implement the portal today —
**KDE and GNOME**. Wayland is detected and preferred automatically
over XWayland; an X11 session on the same machine still uses the
X11 backend, unchanged.

KDE is verified: this backend was built and measured against it in a
live session. GNOME implements the same portal through mutter, so it
is expected to work, but it has not been run there — everything here
was developed against KDE. Treat GNOME as untested rather than
supported, and if you try it, a report of what happened is welcome.

**Hyprland, Sway and other wlroots compositors do not implement the
InputCapture portal**, so a machine running one of them cannot yet be
a Wayland server; `pheme server` exits with an error naming the gap.
Such a machine can still be used as a **client**: a Linux client
injects input through `uinput`, a kernel interface that works
identically under X11 and Wayland, and nothing about the client
changed in this release.

Starting a Wayland server brings up a **permission dialog** asking to
allow input capture, and it may appear on **every start**: KDE
returns no restore token even when the request asks to persist the
grant, so whether the permission survives a restart is untested —
expect the dialog again on each launch until you've confirmed
otherwise on your own compositor.

A **monitor added or removed while the server runs** needs a restart.
The pointer barriers follow the new layout, but the screen list the
server matches edges against is read once at startup, so after a
layout change the two can disagree and the edge switch may stop
working. The log says so ("the display layout changed and no longer
matches the screen list this session started with"); restart `pheme
server` to pick the new layout up.

**On Wayland the lock hotkey binding belongs to the desktop, not to
the configuration file.** Under X11 and Windows, `hotkeys.lock` in
the config decides the key outright. Under Wayland it is only a
*preferred trigger*, sent to the compositor through
`org.freedesktop.portal.GlobalShortcuts`; the compositor may bind a
different key instead. What actually got bound is logged at startup,
and so is a bind that bound *nothing* — check the log if the
configured hotkey does not do anything.

That portal names keys in its own syntax: XKB keysym names, with
`CTRL+`, `SHIFT+`, `ALT+` and `SUPER+` prefixes, which is not how
`hotkeys.lock` names them elsewhere. pheme translates the key names it
knows — the default `ScrollLock` is sent as the keysym `Scroll_Lock` —
and passes anything containing a `+` through untouched, so a full
trigger can be written by hand:

```toml
[hotkeys]
lock = "CTRL+ALT+l"      # portal syntax, used as written on Wayland
# lock = "ScrollLock"    # pheme's own name; sent to the portal as Scroll_Lock
```

A value with a `+` in it is **only** meaningful on Wayland, where the
portal owns the binding. X11 and Windows watch the keyboard for one
named key instead, so the same config there refuses to start with a
message saying so rather than quietly leaving you without a lock. Use
a plain key name unless the machine is a Wayland server.

While the input lock is on, the barriers are withdrawn: the pointer
stops at the screen edge like any other window edge instead of
handing input to the client. That is what makes the lock releasable —
with a barrier still armed the compositor would start a capture the
lock is bound to refuse.

Locking while the input is already **on the client** is the one
exception: the barriers stay as they are until the input comes back,
and are withdrawn then. Nothing is lost by waiting, because only a
pointer on the server screen can reach a barrier — and withdrawing
them there would mean disabling a capture the compositor is running,
while the lock's own rule is that the input stays where it is.

One open question, still unanswered: pressing the lock hotkey **during
an active capture** may toggle the lock twice. While a capture is
running the keypress reaches pheme through libei, and it may *also*
fire the compositor's own global-shortcut binding. Whether a
compositor delivers a global shortcut while an application holds an
InputCapture grab is compositor-defined, and this has not been
measured on either KDE or GNOME. If the lock appears not to respond
while you are controlling the client, press it once more from the
server's own screen.

## Audio

Audio is always on. Whatever the **client** plays comes out of the **server's**
speakers; there is nothing to enable. You choose where audio goes in your operating
system's sound settings, the same way you choose any other output device.

### On a Linux client

pheme creates a virtual PipeWire output node while it runs, and removes it again
when the process exits or is killed. It appears in `pactl list sinks short` as
`pheme-speaker`, described as "Pheme Speaker" in tools that show the friendlier
name (system sound settings, `pavucontrol`). Select it as your output and
everything you play goes to the server. The device stays in the list while the
server is unreachable, so your system does not fall back to another output the
moment the connection drops.

### On a Windows client

Windows has no way for a program to create a virtual output device, so pheme
records whatever your current default output is playing. Nothing to configure —
you also keep hearing the audio locally.

If you want the client to be completely silent, install the free
[VB-CABLE](https://vb-audio.com/Cable/) driver, select `CABLE Input` as your
Windows output, and pin pheme's capture to that same device (it records an
*output* endpoint via loopback, so this is the render device's name, not the
recording one VB-CABLE also creates):

```toml
[audio]
capture_device = "CABLE Input (VB-Audio Virtual Cable)"
```

Any part of the name is enough, as long as only one device contains it —
`capture_device = "CABLE Input"` picks the same endpoint. If several devices
match, pheme logs the ones it found and uses the default instead, so write more
of the name you meant.

### Choosing the server's speakers

By default the server plays on its system default output. To pick another one:

```toml
[audio]
playback_device = "Speakers (Realtek High Definition Audio)"
```

On Linux the value is a PipeWire node name — `pactl list sinks short` prints
them. On Windows it is the device's name as shown in the sound settings, or any
part of it that only one device matches.

### The server's microphone on the client

The microphone attached to the **server** appears on the **client** as an ordinary
recording device, so a call or a recording running on the client uses the microphone
you are actually sitting in front of.

On a Linux client it appears as `pheme-mic` in `pactl list sources short`, described as
"Pheme Mic" in sound settings and `pavucontrol`. Select it wherever you would pick a
microphone.

**The server's microphone is opened only while something is recording.** There is
nothing to switch on: when an application on the client opens Pheme Mic, the server
opens its microphone; about three seconds after the last one closes, the server closes
it again. Until then the device is not held open and its indicator light stays off.

```toml
[audio]
mic_device = "alsa_input.usb-Blue_Yeti-00.mono-fallback"
```

Set `mic_device` under `[audio]` on the server to choose a microphone other than the
system default — `pactl list sources short` on the server prints the names, and on
Windows it is the device's name as shown in the sound settings, the same matching
rules as `capture_device` and `playback_device` above. There is no separate
client-side setting for a virtual microphone: a Windows client does not have one (see
below), and a Linux client always exposes Pheme Mic.

A Windows client has no virtual microphone. Windows has no user-mode way for a program
to create a recording device — that needs a signed kernel driver such as VB-CABLE — so
this is deferred, not implemented in this release. A Windows client keeps its
keyboard, mouse and audio-out exactly as before; it simply never asks the server to
open its microphone. A Windows *server* sends its microphone to a Linux client
normally.

Do not route Pheme Mic into Pheme Speaker on the client: that sends the server's
microphone straight back to the server's speakers, which will howl. Nothing stops
you, because the routing is yours to choose.

### Building on Linux

The PipeWire bindings are generated at build time, so building needs:

```bash
# Debian/Ubuntu
sudo apt install libpipewire-0.3-dev libspa-0.2-dev clang pkg-config
# Arch
sudo pacman -S pipewire clang pkgconf
# Fedora
sudo dnf install pipewire-devel clang pkgconf
```

## Clipboard

The clipboard is **text only** — no images, no files.

It crosses **when the pointer crosses**, not when you copy. Copying
something without switching machines leaves the other side with whatever
it already had; that is a deliberate trade, the same one Synergy and
Deskflow have made for years, not a bug to report. It also means nothing
leaves a machine that you did not carry it to: something copied to paste
locally stays local.

**GNOME Wayland works, through Xwayland.** Mutter implements neither
`wlr-data-control` nor `ext-data-control`, the two protocols a window-less
process like pheme could use to reach the Wayland clipboard directly, and
has declined to add either. `arboard`, the clipboard crate pheme uses,
tries that Wayland path first and, when it fails, falls back to its X11
backend — which succeeds on a stock GNOME session because Xwayland is
running and `DISPLAY` is set. `arboard` logs its own warning when it takes
this fallback, so that line in your logs is how you can tell the Xwayland
route was used.

The Xwayland route has a real cost worth knowing about: the clipboard is
bridged by the compositor rather than owned directly, and, as on any X11
session, text pheme put on the clipboard disappears when pheme exits
unless a clipboard manager has taken it.

A session with no Xwayland and no data-control protocol at all — a bare
compositor, or a headless session — is where the clipboard is genuinely
unreachable. There, opening the clipboard fails, clipboard sharing stays
off for the process lifetime, and keyboard, mouse and audio are
unaffected.

Content over 1 MiB stays local: the transfer is refused with a log line,
and nothing else stops working.

## Limitations

- One client at a time: the server serves a single client connection; another
  client is not served until that one disconnects.
- IPv4 only.
- A Linux **server** works under X11, or under Wayland on compositors that
  implement the InputCapture portal (KDE, GNOME) — see "Wayland capture
  (server)" above. Hyprland, Sway and other wlroots compositors cannot yet
  be a Wayland server, though they can still be a client. A Linux
  **client** works under X11 or Wayland on any compositor (injection goes
  through uinput).
- macOS support comes in a later sub-project.
- The clipboard is text only, works on GNOME Wayland only through the
  Xwayland fallback and is unreachable on a compositor or headless
  session with neither Xwayland nor a data-control protocol (see
  "Clipboard" above), and syncs on the crossing, not on every copy.
- A Windows client cannot receive the server's microphone: creating a recording
  device on Windows needs a signed kernel driver (VB-CABLE), which is deferred. A
  Windows server's microphone reaching a Linux client works normally.

See `docs/testing.md` for the manual test checklist.
