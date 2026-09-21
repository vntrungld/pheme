# Manual test checklist — KVM core

Run before every release, for **each** row of the matrix. Record the result
(pass/fail + notes) in the release PR.

| # | Server → Client                          | Result |
|---|------------------------------------------|--------|
| 1 | Linux X11 → Windows                      |        |
| 2 | Windows → Linux (X11 session)            |        |
| 3 | Windows → Linux (Wayland session)        |        |
| 4 | Linux X11 → Linux (X11 or Wayland)       |        |
| 5 | Windows → Windows                        |        |

## Setup

1. Build `pheme` on both machines (`cargo build --release`).
2. Linux client: `sudo pheme setup`, log out/in, verify `ls -l /dev/uinput` shows group `input`.
3. Server config (`~/.config/pheme/config.toml` or `%APPDATA%\pheme\config.toml`):
   ```toml
   role = "server"
   name = "<server-name>"
   [[clients]]
   name = "<client-name>"
   side = "right"
   ```
4. Client config: `role = "client"`, `name = "<client-name>"`, `connect = "<server-ip>"`.

## Steps and pass criteria

1. **Pairing** — `pheme server --pair` on the server, `pheme pair <ip> <code>` on the
   client. Both print "Paired". Then from a third, unpaired machine (or after deleting
   the client's `trusted.toml`) run `pheme client <ip>`: the connection must be refused
   and the server log must show "rejecting untrusted peer".
2. **Edge switch** — with `pheme server` and `pheme client <ip>` running, push the
   pointer through the server's right edge: the server cursor disappears, the client
   cursor appears at the matching height on its left edge. Move left past the client's
   left edge: control returns, the server cursor reappears one pixel inside its right
   edge at the matching height.
3. **Sliding along the edge** — drag the pointer up and down while touching the right
   edge for 5 s: no switch.
4. **Typing** — on the client, open a text editor and type 100 characters including
   `Shift+letters`, `Ctrl+A`/`Ctrl+C`/`Ctrl+V`, `Alt+Tab`, arrows, Home/End, Numpad
   digits with NumLock on, `PrintScreen`, `Pause`, F1–F12. Everything arrives, nothing
   repeats, no key is left held (check with `xev`/`evtest` on Linux or by typing after).
5. **Modifier across the edge** — hold Shift on the server, cross to the client, type
   `a` (expect `A`), release Shift on the client, type `a` (expect `a`), cross back,
   type `a` on the server (expect `a`).
6. **Scrolling** — vertical and horizontal wheel on the client scroll smoothly; one
   notch = one notch. Hi-res mice (free-spin) scroll proportionally.
7. **Lock hotkey** — press ScrollLock on the server: the pointer can no longer leave;
   press again: it can. While on the client, press ScrollLock: the pointer cannot
   return until pressed again.
8. **Client disconnect** — while controlling the client, unplug its network. Within
   5 s the server regains its cursor at the screen centre and no key stays held on the
   client. Plug the cable back in: the client reconnects within 10 s and the edge
   switch works again.
9. **Multi-monitor client** — with two monitors on the client, `Enter` lands on the
   monitor adjacent to the server edge and the pointer can travel across both.
10. **Stats** — run both sides with `--stats`: on a wired LAN the reported RTT is
    below 1 ms and `datagrams` lost stay at 0 during 1 minute of continuous movement.
