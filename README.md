# Pheme

Share one machine's keyboard and mouse with another over the LAN, Deskflow-style,
with bidirectional audio forwarding (planned). Written in Rust. GPL-3.0.

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

## Limitations (sub-project 1)

- One client at a time: the server serves a single client connection; another
  client is not served until that one disconnects.
- IPv4 only.
- A Linux **server** needs an X11 session (input capture uses XInput2). A Linux
  **client** works under X11 or Wayland (injection goes through uinput).
- Wayland capture and macOS support come in later sub-projects; audio and clipboard
  forwarding are not implemented yet.

See `docs/testing.md` for the manual test checklist.
