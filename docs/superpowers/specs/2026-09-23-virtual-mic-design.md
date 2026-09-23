# Sub-project 3 — Virtual mic (server → client)

Date: 2026-09-23
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-21-audio-out-design.md`
Expected outcome: the microphone attached to the **server** appears on the
**client** as an ordinary recording device. On a Linux client the user
selects "Pheme Mic" in the sound settings; anything that records from it
hears the server's microphone with under 40 ms of latency. The server's
microphone is opened only while something on the client is actually
recording, so it is not held open — and its indicator light is not lit —
for the life of the session.

## 1. Scope

In:

- A demand-driven mic path: the client tells the server when something is
  recording, and the server opens its microphone only then.
- Linux client: a virtual PipeWire `Audio/Source` node named "Pheme Mic".
- Linux server: microphone capture from the default source or a named one.
- Windows server: microphone capture through WASAPI on a real `eCapture`
  endpoint, plus the mono→stereo fix this needs in `ToWire`.
- Protocol: `Msg::MicWanted`, `AudioParams` on `Msg::Hello`,
  `PROTOCOL_VERSION` 1 → 2.
- `pheme-net`: `Msg::Audio` moves off the shared incoming channel.
- Two deduplications that must land before the mic code, not after it
  (§4): the device-thread handshake in `pheme-audio`, and the audio
  supervisor in `pheme-app`.
- The four debts sub-project 2 carried forward (§9).

Out:

- **A virtual microphone on a Windows client** (§13). It is the only part
  of either audio direction that needs a third-party kernel driver
  (VB-CABLE), and it is the only part that can conflict with sub-project
  2's optional "silent client" setup. Deferring it removes the driver
  dependency, the single-cable conflict, the echo-loop guard and a whole
  consumer-detection mechanism from this sub-project without touching
  anything that works today: a Windows client keeps its keyboard, mouse
  and audio-out exactly as sub-project 2 shipped them. §13 records the
  design so it does not have to be rediscovered.
- macOS, compressed codecs, more than one client, in-app volume control,
  per-application routing, echo cancellation, noise suppression.

Audio out remains always-on and unflagged. The mic direction is
demand-driven rather than flagged: there is still no configuration to
turn it on or off, but it costs nothing while nobody is listening.

## 2. Wire format and protocol changes

### 2.1 `Msg::Audio` — unchanged

```rust
Msg::Audio { stream: AudioStream, seq: u32, ts_us: u64, samples: Vec<u8> }
```

The mic direction uses `AudioStream::Mic` and is byte-for-byte the same
shape as `AudioStream::Playback`: interleaved little-endian i16, 240
samples per channel, 2 channels, 960 bytes, 5 ms. Every rule sub-project
2 set for the playback stream applies unchanged — `seq` counts audio
time including suppressed frames, `ts_us` is diagnostic only, frames
travel in QUIC datagrams and are concealed rather than retransmitted.

There is one audio format in this project and there will never be a
second one. A real microphone is usually mono, and sending it as two
identical channels doubles its bandwidth to 1.54 Mbit/s; this was
considered and rejected, because a LAN has the bandwidth to spare and a
second wire format would cost format negotiation in every direction
forever. Demand gating (§3) saves far more than a mono format would,
since it removes the traffic entirely rather than halving it.

### 2.2 `Msg::MicWanted` — new

```rust
MicWanted { wanted: bool }
```

Client → server, on the **control stream**: reliable and ordered, not a
datagram. A lost or reordered demand signal would either strand the
microphone open or leave it shut, and neither is acceptable for a
message sent a handful of times per session.

### 2.3 `Msg::Hello` gains `audio`

```rust
Hello { version: u16, name: String, os: Os, screens: Vec<ScreenInfo>, audio: AudioParams }
```

Sub-project 2 left the parameter exchange one-way — `AudioParams`
travelled in `HelloAck` only — and explicitly deferred the decision to
this sub-project, because a second stream in the opposite direction
changes the calculus. It does: the server now sends audio too, so it
must be able to check what the client speaks rather than transmit and
hope. On a mismatch the server logs an error once and runs the session
with no audio in either direction; keyboard and mouse are unaffected.

### 2.4 `PROTOCOL_VERSION` 1 → 2

Adding a field changes `Hello`'s postcard encoding, and postcard decodes
the whole message at once. A version-1 client's `Hello` therefore fails
to **decode** at a version-2 server, before the version check can run:
the client sees "first message was not Hello" rather than a clean
protocol-mismatch message. Both ends are built from the same tree and
nothing is released yet, so this costs nothing in practice — but it is a
real limitation of the handshake and is recorded here rather than
discovered later. Restructuring the handshake so the version survives an
encoding change is not worth doing for a pre-release protocol.

### 2.5 `Msg::Audio` leaves the shared incoming channel

`Peer::new` currently merges the control stream and the datagram stream
into one bounded channel of 256 messages, and `take_incoming()` hands it
over whole. Sub-project 2 tolerated this because only one side received
audio. Sub-project 3 makes **both** sides receive audio and input at the
same time, and the arithmetic is bad: a brief stall can park a mouse
event behind up to 255 audio frames — **1.28 seconds** — in a project
whose first stated priority is input latency.

- `Peer::take_audio()` returns a separate bounded channel carrying only
  `Msg::Audio`.
- `Peer::take_incoming()` keeps everything else, unchanged for callers.
- The audio channel holds **32 frames**. Nothing is gained by queueing
  more: `JitterBuffer` downstream has a `MAX_DEPTH` of 24 frames, so a
  deeper upstream queue can only add latency it will then have to
  discard.
- A full channel drops the frame being delivered and counts it. This
  matches what `RecvSide` already does at its own handoff and what the
  jitter buffer does at its ceiling: the newest sample in hand is the one
  that goes, and nothing already accepted is rewritten.

## 3. Demand-driven microphone

### 3.1 The rule

**The demand signal may only ever keep the microphone open longer than
necessary. It may never close one that should be open.**

A microphone that is wrongly held open wastes bandwidth and lights an
indicator. A microphone that is wrongly held shut makes the whole feature
fail *silently* — the exact defect class that reached the user three
times in sub-project 2 (the WASAPI readiness timeout, the dead
`healthy()` path, the unbounded jitter depth). The asymmetry is
deliberate and the type system carries it:

```rust
/// Whether anything is consuming what a playback backend emits.
pub enum Demand { Wanted, Idle, Unknown }

/// On `AudioPlayback`:
fn demand(&self) -> Demand { Demand::Unknown }
```

Only `Idle` closes the microphone. `Unknown` is the trait default, so a
backend that has not implemented detection — every backend except the
Linux virtual source — fails safe without writing a line of code. The
server's speaker playback keeps the default and nothing reads it.

### 3.2 What the client sends

The client owns the decision, because the client is where consumers
live. Four cases, each for its own reason:

| Client state | Sends | Why |
|---|---|---|
| Backend running, reports `Idle` | `MicWanted(false)` | Nothing is recording |
| Backend running, reports `Wanted` or `Unknown` | `MicWanted(true)` | Unknown means open |
| Backend stopped, failed, or being rebuilt | `MicWanted(false)` | Nothing *could* consume the audio |
| No connection | — (server closes) | No client means no consumer |

The third row is where a naive reading of §3.1 gets it wrong. "Fail
open" protects against *not knowing*; it does not apply when the answer
is known negatively. A client with no working virtual microphone cannot
deliver audio to anything, so opening the server's microphone for it
would be pure cost. This is also exactly what makes a Windows client
coherent under §13 without a special case: `detect_virtual_mic` returns
`Unsupported`, the backend never runs, the client sends
`MicWanted(false)`, and the server's microphone stays shut.

The client sends `MicWanted` **once immediately after the handshake** and
again on every change. The server starts each session with the
microphone closed and opens it only on an explicit `MicWanted(true)`. A
test asserts the post-handshake send exists, because losing it is a
silent-failure bug of precisely the kind this section exists to prevent.

### 3.3 Linger

The client debounces a `Wanted → Idle` transition by **3 seconds** before
sending `MicWanted(false)`. Applications probe recording devices —
enumerating, opening briefly, closing — and without a linger the
server's microphone would open and close repeatedly, which is both
visible to the user and hard on the device. A `Idle → Wanted` transition
is sent immediately; only the closing edge waits.

### 3.4 Resuming: the client resets its own jitter buffer

This is the subtlest interaction in the sub-project and it does not
surface in any automated test written against either half alone.

Sub-project 2 established that `seq` counts audio time and that the
receiver's read cursor advances at its own device rate alongside it, and
it warned about the precise failure this creates: *"A client whose
capture device stops producing frames entirely — rather than producing
silent ones — would let the server's cursor walk past the sender's
numbering, and everything that arrived afterwards would count as `late`
until the gap passed 200 frames."* Gating the microphone does exactly
that, deliberately.

The sequence: a consumer attaches, the virtual source goes `Streaming`
and begins pulling samples immediately, while the server's microphone is
still one round trip plus a device open away. The read cursor advances
tens of frames. The first frame then arrives with `seq` restarted near
zero. If that gap is **under** `RESET_GAP` (200 frames), the jitter
buffer does not reset — it classifies every arriving frame as `late` and
discards it until the sender catches up. The user hears up to **750 ms of
silence at the start of every recording**, with every error counter
reading zero.

The fix needs no protocol field and no inference from sequence numbers:
**the client resets its mic jitter buffer at the moment it sends
`MicWanted(true)`.** The client is the one component that knows exactly
when it asked for the stream to resume, so it does not have to guess
from the wire. `RecvSide::reset()` drops everything buffered and
re-prefills. Manual test M4 exists to catch a regression here, because
this failure is inaudible to a counter and obvious to an ear.

### 3.5 Detecting consumers on Linux — verified

Measured on this machine against PipeWire 1.6.8, with a throwaway probe
that created the node and logged every stream state transition while a
recorder attached and detached:

```
[  0.00s] Unconnected -> Connecting
[  0.00s] Connecting  -> Paused        no consumer
[  2.07s] Paused      -> Streaming     parecord attached
[  5.05s] Streaming   -> Paused        parecord detached
```

Three things this settles:

1. A `media.class = Audio/Source` node connected with
   `Direction::Output` appears as a real recording device:
   `pactl list sources short` shows
   `pheme-mic-probe  PipeWire  s16le 2ch 48000Hz`.
2. **The demand signal is free.** `Paused ↔ Streaming` tracks consumers
   exactly, and it arrives through the `state_changed` listener
   `linux_pipewire.rs` already registers for its other streams. No
   registry walking, no new API.
3. Samples pass through intact. The probe's 440 Hz tone came back at
   440.1 Hz with the programmed amplitude, both channels identical, and
   the longest run of zero samples in the capture was **1** — no
   dropouts. The recorder received 44.1 kHz despite the node running at
   48 kHz, so the graph does rate conversion for consumers on its own.

A level meter counts as a consumer: opening the sound settings' input
page, or `pavucontrol`, moves the node to `Streaming`. That is correct —
something really is listening — and it is worth knowing when reading M2.

## 4. Deduplication, before any mic code

Both of these are prerequisites, not cleanup. Sub-project 2's
whole-branch review recorded the first as this sub-project's first task
specifically so the mic direction would not add copies five and six.

### 4.1 `pheme-audio/src/device.rs` — the device-thread handshake

Each of the four existing backends re-implements the same lifecycle: a
`ready` channel, a start timeout, a timeout path that **detaches rather
than joins**, an `AliveGuard` that flips `healthy()` when the thread's
stack unwinds, and an idempotent `stop()` that joins. This is where the
readiness contract has been broken twice — WASAPI signalled readiness
only after its session ended, so every Windows `start` timed out and all
Windows audio was dead while the build and lints stayed green; and
PipeWire's `healthy()` could never return false, making its entire
rebuild path unreachable. Sub-project 3 would take it to six copies.

```rust
/// The start/stop/healthy handshake every device backend needs, once.
pub struct DeviceThread { /* … */ }

/// Handed to a device thread; it calls `ok()` or `fail(e)` exactly once.
pub struct Ready(/* … */);

impl DeviceThread {
    /// Spawns `body` on a named thread and blocks until it reports readiness or
    /// `timeout` elapses. On timeout, `stop` is invoked and the thread is
    /// **detached, never joined** — a thread hung in device construction may
    /// never observe a stop request, and joining it would turn `start`'s bounded
    /// wait into an unbounded one.
    pub fn start(
        &mut self,
        name: &str,
        timeout: Duration,
        stop: impl Fn() + Send + 'static,
        body: impl FnOnce(Ready) + Send + 'static,
    ) -> Result<()>;

    /// False once the device thread's stack has been torn down, however it died.
    pub fn healthy(&self) -> bool;

    /// Idempotent; joins the device thread.
    pub fn stop(&mut self);
}
```

`start` installs the `AliveGuard` around `body` itself, so `healthy()`
becomes correct for every backend by construction rather than by each
backend remembering to do it. That is the specific defect this
extraction is designed to make impossible.

### 4.2 `pheme-app/src/audio/` — one supervisor per direction

`AudioOut` and `AudioIn` are near-identical: spawn, retry every five
seconds, `FailureLog`, `nap`, `Drop`. Split the 858-line `audio.rs`:

```
crates/pheme-app/src/audio/mod.rs    Supervisor, FailureLog, nap, RETRY, TICK
crates/pheme-app/src/audio/send.rs   SendSide  capture → Packer → datagram
crates/pheme-app/src/audio/recv.rs   RecvSide  datagram → JitterBuffer → Drift → playback
```

Each role builds one of each, differing only in the stream tag and the
backend detected:

| | `SendSide` | `RecvSide` |
|---|---|---|
| Client | `AudioStream::Playback`, virtual sink / loopback | virtual source ("Pheme Mic") |
| Server | `AudioStream::Mic`, real microphone | speakers |

```rust
impl SendSide {
    pub fn spawn(source: CaptureSource, stream: AudioStream, counters: Arc<OutCounters>) -> SendSide;
    pub fn set_peer(&self, sender: Option<PeerSender>);
    /// Gates the **device**, not just the sending: `false` stops the backend so the
    /// operating system shows the microphone as closed and its indicator goes out.
    /// A side that is never gated is simply never called.
    pub fn set_wanted(&self, wanted: bool);
    pub fn stop(&mut self);
}

impl RecvSide {
    pub fn spawn(source: PlaybackSource, stats: Arc<InStats>) -> RecvSide;
    pub fn push(&self, f: Frame);
    /// Debounced demand, updated by the worker thread. The client's session loop
    /// selects on this rather than polling, so a consumer attaching is noticed in
    /// milliseconds rather than on the next one-second tick.
    pub fn wanted(&self) -> watch::Receiver<bool>;
    /// Drops everything buffered and re-prefills (§3.4).
    pub fn reset(&self);
    pub fn stop(&mut self);
}
```

`set_wanted` requires a backend that can be started more than once.
`CaptureSource::Backend` is currently documented as start-once, which
was adequate when nothing ever stopped a running backend; it has to
support restarting, and the mock backends with it.

A welcome consequence: adding the second direction makes `audio.rs`
**smaller**, not larger, and every existing jitter, drift and
silence-suppression test covers both directions without being rewritten.

## 5. Linux backends (`linux_pipewire.rs`)

### 5.1 Client — the virtual source "Pheme Mic"

The mirror of the existing virtual sink, and verified by the probe in
§3.5.

- `media.class = Audio/Source`, `media.category = Playback`,
  `media.role = Communication`, `node.name = pheme-mic`,
  `node.description = Pheme Mic`, `node.latency = 240/48000`, connected
  with `Direction::Output`.
- Channel positions are declared explicitly as front-left/front-right.
  Sub-project 2 learned this the hard way: an unpositioned stereo format
  negotiated against a positioned peer segfaults inside
  `libspa-audioconvert`.
- `rate()` returns 48 000 unconditionally. We own this node, so the base
  resample ratio is exactly 1.0 and drift control works against the
  graph clock alone.
- `demand()` is derived from the `state_changed` listener: `Streaming` →
  `Wanted`, anything else → `Idle`.
- The node stays in the device list while the server is unreachable and
  emits silence, matching the architecture document's rule that virtual
  devices do not vanish — otherwise the operating system would move
  recording applications to another microphone the moment a connection
  dropped.

### 5.2 Server — microphone capture

A `Stream/Input/Audio` stream against the default source, or against
`audio.mic_device` matched as `target.object`. This is the mirror of the
existing `PipewirePlayback` and inherits its failure handling.

The stream asks for 48 kHz / 2 channels / S16LE and lets the PipeWire
graph convert. This machine's microphones are `s32le 2ch 48000`, so
sample-format conversion is exercised immediately. **Mono → stereo
upmixing through the graph is expected but not yet measured**, and many
microphones are mono, so verifying it is a required step in the first
task that builds this backend — not a note at the end of the spec.

## 6. Windows backend — server microphone only (`windows/wasapi.rs`)

### 6.1 Capture from a real `eCapture` endpoint

Event-driven shared-mode capture on the default capture endpoint, or on
`audio.mic_device` matched by friendly name. Unlike the loopback capture
sub-project 2 built, this attaches to a capture endpoint and can be
driven by an event rather than polled, so it does not need the 2.5 ms
poll loop. Format conversion reuses `ToWire`.

### 6.2 `ToWire` needs a mono path

`ToWire` currently **truncates** channels beyond the first two, which is
the right rule for sub-project 2's case — a surround output feeding a
stereo wire format, where truncating avoids inventing a downmix nobody
asked for. It is wrong in the other direction. A microphone commonly
reports **one** channel, and truncation then indexes a source channel
that does not exist.

A single-channel source must be **duplicated** into both wire channels.
This is a real bug being fixed, not a new feature: the existing rule was
written for sources with more channels than the wire, and nothing has
ever fed it a source with fewer.

## 7. `pheme-app` integration

### 7.1 Config

```toml
[audio]
playback_device = ""       # server: where the client's audio is played
capture_device = ""        # Windows client: loopback source
mic_device = ""            # server: which microphone to capture (new)
```

`mic_device` is empty by default, meaning the platform default. There is
no key to enable or disable the mic direction; §3 makes one unnecessary.

### 7.2 Failure policy

Unchanged from sub-project 2, and it now has to hold for four
supervisors instead of two: nothing in the audio path can make
`run_client` or `run_server` return an error, a failing backend is
rebuilt every five seconds, and `FailureLog` keeps a permanently failing
machine from writing ~17 000 warning lines a day.

One new distinction the supervisor must respect: **a microphone closed
by demand is not a failure.** It must not be logged as one and must not
drive the retry cycle. Only a backend that failed to start, or died
while wanted, is a failure.

### 7.3 Stats

Both roles now have both directions. The existing `audio_*` names keep
meaning the playback stream, so the lines the user already reads do not
change meaning; the mic stream takes `mic_*`.

- Client: `audio_sent`, `audio_suppressed`, `mic_depth_ms`, `mic_lost`,
  `mic_underruns`, `mic_late`, `mic_resets`, `mic_dropped`,
  `mic_overflows`
- Server: `audio_depth_ms`, `audio_lost`, `audio_underruns`,
  `audio_late`, `audio_resets`, `audio_dropped`, `audio_overflows`,
  `mic_sent`, `mic_suppressed`, `mic_open`

`mic_open` is a boolean: whether the server's microphone is currently
held open. It is the one counter that makes the demand mechanism
observable from a log.

**The stats interval itself is wrong and is fixed here.** The line is
emitted under `if stats && last_stats.elapsed() >= Duration::from_secs(1)`
inside a one-second tick, so a tick that lands a few milliseconds early
skips its report and the next line presents two seconds of counters
under a per-second label. It was observed live as an alternation between
200 and 400 frames per second when the true rate was a steady 200. The
counters are right; only the interval they are divided by is wrong.

## 8. Latency budget

One way, server microphone to client application:

| Stage | Linux server → Linux client | Windows server → Linux client |
|---|---|---|
| Microphone device buffer | 5 ms | 10 ms (WASAPI shared mode) |
| Packer thread wake-up | ≤ 2 ms (`TICK`) | ≤ 2 ms |
| Network (wired LAN) | ~0.5 ms | ~0.5 ms |
| Playback worker wake-up | ≤ 2 ms (`TICK`) | ≤ 2 ms |
| Jitter buffer target | 10 ms | 10 ms |
| Resampler (sinc_len 64) | 0.7 ms | 0.7 ms |
| Virtual source ring (1–2 frames, ~1.5 average) | ~7.5 ms | ~7.5 ms |
| Virtual source buffer (`node.latency 240/48000`) | 5 ms | 5 ms |
| **Total** | **~33 ms** | **~38 ms** |

The same shape as sub-project 2's budget and for the same reasons. The
client end is cheaper than sub-project 2's playback end because we own
the virtual source node and set its latency, rather than inheriting a
shared-mode device buffer.

This budget does **not** cover the gap between a consumer attaching and
the first frame arriving (§3.4), which is a round trip plus a device
open — a few hundred milliseconds, and the reason M2 allows 500 ms.
That is startup latency, not stream latency; once running, the table
above applies.

## 9. Debts carried from sub-project 2

Agreed with that sub-project's whole-branch reviewer and recorded so they
are not silently deferred again. Debt (a), the start/stop handshake, is
§4.1 and §4.2; (b), the shared incoming channel, is §2.5; (f), the
one-way `AudioParams` exchange, is §2.3. The rest:

- **(c) Publish pre-pop jitter depth.** `audio_depth_ms` samples the
  buffer just *after* a pop, so it reads one frame lower than the depth
  §8's budget describes. Publish the pre-pop depth so the number and the
  budget refer to the same thing.
- **(d) The `median >= 5` depth assertion has no headroom.** At 44.1 kHz
  the margin vanishes and the test becomes a latent flake. Assert
  something the pacing actually guarantees.
- **(e) Nothing bounds the maximum sample.** A clipping regression —
  anything that overflows or wraps a sample — would pass the whole
  suite. Add a test that bounds peak amplitude through the resampler.

## 10. Definition of done

- On a Linux client, "Pheme Mic" appears in the sound settings and an
  application recording from it hears the server's microphone, with a
  Windows server and with a Linux server.
- With nothing recording on the client, the server's microphone is
  closed: `mic_open = false`, and the operating system shows it as not in
  use.
- Recording starts producing audio within 500 ms of an application
  opening the device, **with no silent gap at the start** (§3.4).
- Ten minutes of continuous recording with `mic_underruns = 0` and
  `mic_depth_ms` level rather than climbing.
- Measured end-to-end mic latency below 40 ms.
- A client with no virtual microphone backend never causes the server to
  open its microphone, and logs nothing repetitive.
- An audio backend failure in either direction leaves keyboard and mouse
  fully working.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` green on both the Ubuntu
  and Windows CI jobs.
- README documents selecting "Pheme Mic" on a Linux client, that the
  server's microphone opens only on demand, and that a Windows client has
  no virtual microphone yet.

## 11. Manual test matrix (added to `docs/testing.md`)

Runnable with this machine as the Linux client and the QEMU VM
(`~/pheme-vm/start-vm.sh`) as the Windows server, and again Linux → Linux.

| # | Check |
|---|---|
| M1 | "Pheme Mic" appears in the client's sound settings; recording from it plays the server's microphone |
| M2 | Nothing recording → server microphone closed (`mic_open = false`, OS shows it unused); start recording → audio within 500 ms |
| M3 | Stop recording → microphone closes after ~3 s; an application that merely enumerates devices does not make it flap |
| M4 | **No silence at the start of a recording** — the §3.4 regression, audible rather than counted |
| M5 | A mono microphone on the server arrives as two channels on the client |
| M6 | Ten minutes continuous: `mic_underruns = 0`, `mic_depth_ms` steady |
| M7 | Unplug the network for 3 s and reconnect: the mic resumes by itself |
| M8 | Unplug the server's microphone mid-session: recovery within the retry cycle, keyboard and mouse unaffected |
| M9 | A Windows client: the server's microphone never opens, and the client's log stays clean |
| M10 | Latency: clap near the server's microphone while recording on the client, measure the offset — under 40 ms |

## 12. Known risks

- **Mono → stereo upmixing through the PipeWire graph is assumed, not
  measured.** Most microphones are mono, so if the graph will not do it
  the backend must. Verified in the first task that builds the Linux mic
  capture, not left to manual testing.
- **Windows `eCapture` has never been executed.** Sub-project 2 shipped
  Windows code that was cross-compiled and linted but never run, and
  three real defects survived to the VM session. The VM is the gate here
  too; rows M1, M2, M5 and M10 run against a Windows server.
- **A level meter counts as a consumer.** Sound settings or `pavucontrol`
  left open on an input page holds the server's microphone open. Correct
  behaviour, surprising the first time it is seen.
- **The linger is a fixed 3 seconds.** An application that probes at a
  slower cadence than that will still cause the microphone to cycle.
  Accepted for v1; the alternative is a heuristic with no evidence behind
  it yet.
- **Feedback loops are possible through user routing.** Routing "Pheme
  Mic" into "Pheme Speaker" on the client sends the server's microphone
  back to the server. It is user error rather than something the design
  can prevent, but it is worth a line in the README.

## 13. Deferred: a virtual microphone on a Windows client

Recorded so the analysis does not have to be redone.

Windows has no user-mode API that creates an audio endpoint. A virtual
microphone requires a **signed kernel driver** — an EV certificate and
Microsoft's signing process — which is a project of its own, not a
sub-project. The shortcuts do not work: a DirectShow source such as
`virtual-audio-capture-grabber` is invisible to applications that
enumerate through WASAPI/MMDevice, which is nearly every browser, and
browsers are the main reason to want the feature. OBS has the same
constraint and tells its users to install VB-CABLE.

So the eventual implementation is VB-CABLE, and the design is:

- The client renders into `CABLE Input` (the *render* half — loopback and
  virtual-mic rendering both attach to render endpoints); applications
  select `CABLE Output` as their microphone.
- **Exact device matching, no fallback.** `open_render_device` currently
  falls back to the default output when a name does not match. For a
  virtual microphone that would play the server's microphone out of the
  client's speakers — wrong and startling. A missing device must be an
  error, which the five-second retry then picks up if the user installs
  VB-CABLE later.
- **Cable priority.** The free VB-CABLE package provides one cable.
  Sub-project 2's optional "silent client" setup points
  `capture_device` at `CABLE Input`, and a virtual microphone renders
  into the same endpoint — so the client's loopback would capture the
  server's own microphone and send it back: an echo loop. The resolution
  is not to refuse, but to rank: the virtual microphone **keeps** the
  cable, because it cannot exist without one, while the silent-client
  setup is a convenience whose loss only means the client keeps playing
  audio through its own speakers. On detecting the collision, name both
  configuration keys in an error and ignore `capture_device`.
- **Consumer detection** would come from `IAudioSessionManager2` on the
  `CABLE Output` capture endpoint, enumerating sessions and looking for
  one in `AudioSessionStateActive`. This is unverified. Under §3.1 it
  does not need to be verified before shipping: leaving `demand()` at its
  `Unknown` default gives a correct, if unoptimised, always-open
  microphone.
