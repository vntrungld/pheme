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
5. **Modifier across the edge** — hold Shift on the server, cross to the client, release
   Shift on the client, then type `a` on the client (expect `a`) and, after crossing
   back, `a` on the server (expect `a`): nothing stays stuck on either side. (The
   modifier state at `Enter` is logged, not replayed on the client.)
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
10. **Stats** — run both sides with `--stats`. The client logs a `stats/s` line every
    second with `rtt_us`, `received` and `lost` (datagrams missing from the `seq`
    numbering); the server logs `events`, `control` and `datagrams` sent. On a wired
    LAN `rtt_us` stays below 1000 and `lost` stays at 0 during 1 minute of continuous
    movement.

## Audio (client → server)

Audio only ever flows client to server, so every row below is run in both
role combinations: Windows client → Linux server, and Linux client → Windows
server. The Windows capture and playback backends compile and pass lint and
unit tests, but this matrix is the only thing that has ever run them against
real hardware — treat every Windows result as unverified until you've watched
it pass yourself, and note the exact failure (log line, silence, crash) if a
row does not.

Run both sides with `--stats` for A2 and A5; the client logs `audio_sent` and
`audio_suppressed`, the server logs `audio_depth_ms` and `audio_underruns`
(see the client/server `--stats` output above for the rest of the line).

| # | Check | Pass |
|---|---|---|
| A1 | Select "Pheme Speaker" (Linux) or leave the default output (Windows) on the client and play music | Audible on the server's speakers, no crackle, no stutter |
| A2 | Leave it playing for ten minutes with `--stats` | `audio_underruns` stays 0 after the first seconds, and `audio_depth_ms` stays level. On a clean link it alternates between 5 and 10: the counter samples the buffer just after a pop, so the 2-frame (10 ms) target reads as 5 or 10, not a flat 10. It rises a frame at a time on a lossy link, up to 40. What must not happen is a steady climb |
| A3 | Unplug the network for 3 s, then plug it back in | Audio resumes on its own; no restart needed |
| A4 | On a Windows client, change the default output device mid-stream | Audio continues on the new device within a few seconds |
| A5 | Pause playback for 30 s, then resume | No audio traffic while paused (`audio_sent` drops to 0, `audio_suppressed` rises); sound is back within 100 ms of resuming |
| A6 | Stop the server while the client keeps running | The client still offers "Pheme Speaker", logs no errors, and does not hang |
| A7 | Play a click track on the client and record both machines' speakers with a phone | The offset between the two clicks is under 40 ms |
| A8 | Kill and restart the PipeWire daemon on a Linux machine mid-stream (`systemctl --user restart pipewire pipewire-pulse`) | Audio comes back within about 5 s without restarting pheme. The logs show `the PipeWire connection failed` (or `is gone`), then `audio capture stopped` / `audio playback stopped`, then `audio capture started` five seconds later. Measured on PipeWire 1.6.8 with the `sink_dump` and `audio_loopback` examples: ill health reported about 2 ms after the restart, backends rebuilt 5.02 s later |
| A9 | On the Windows side of the pair (Windows client capturing, or Windows server playing back), start the connection and begin playing audio immediately — do not wait before checking. Time from the connection coming up to audio first being audible on the server | Audible within about 2 s. If it takes much longer, or nothing ever plays despite the build and `--stats` looking healthy, suspect a regression in `start()` never actually completing — this failure mode has shipped before while compiling and linting clean |
| A10 | On a Windows **server**, change the default output device mid-stream (unplug headphones, or switch output in the sound settings) | Audio continues, and `--stats` keeps reporting `audio_depth_ms`. If the new endpoint runs at a different rate — 44.1 kHz where the old one was 48 kHz — the server logs `the render device reopened at a different sample rate` and rebuilds the whole playback pipeline, which costs a few seconds of silence. Audio that comes back at the wrong pitch, playing about 9 % fast with nothing in the log, is the failure this row exists to catch |

If A2 shows `audio_depth_ms` climbing steadily over ten minutes, the drift controller is
not holding — report the trend, do not just restart.
