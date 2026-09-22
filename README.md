# Pheme

Share one machine's keyboard and mouse with another over the LAN, Deskflow-style,
with audio forwarded from the client to the server (the reverse direction is
planned). Written in Rust. GPL-3.0.

Status: early development — see `docs/superpowers/specs/` for the design.

## Build

    cargo build --release

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

## Limitations

- One client at a time: the server serves a single client connection; another
  client is not served until that one disconnects.
- IPv4 only.
- A Linux **server** needs an X11 session (input capture uses XInput2). A Linux
  **client** works under X11 or Wayland (injection goes through uinput).
- Wayland capture and macOS support come in later sub-projects; clipboard
  forwarding is not implemented yet.
- Audio is one direction only for now: client to server. The server's microphone
  reaching the client comes in the next release.

See `docs/testing.md` for the manual test checklist.
