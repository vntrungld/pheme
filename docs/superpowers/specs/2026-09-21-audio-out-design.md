# Sub-project 2 — Audio out (client → server)

Date: 2026-09-21
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-21-kvm-core-design.md`
Expected outcome: on the client, the user selects "Pheme Speaker" (Linux)
or keeps the default output (Windows) in the OS sound settings; whatever
the client plays comes out of the server's speakers with under 40 ms of
latency and no dropouts. Works in both directions of the OS matrix
(Windows client → Linux server and Linux client → Windows server).

## 1. Scope

In:

- New crate `pheme-audio`: capture/playback traits, frame packing with
  silence suppression, jitter buffer, drift control, mock backend.
- Linux backend on PipeWire: a virtual `Audio/Sink` node named
  "Pheme Speaker" on the client, and a playback stream on the server.
- Windows backend on WASAPI: loopback capture of the default render
  endpoint on the client, shared-mode event-driven render on the server.
- `pheme-app` wiring: an audio task per role, a `[audio]` config section,
  audio counters in `--stats`, and a retry policy that never disturbs the
  KVM session.
- CI: install the PipeWire development files on the Ubuntu job.

Out (later sub-projects):

- Server microphone → client virtual mic (sub-project 3).
- macOS, compressed codecs, more than one client, in-app volume control,
  per-application routing.

Audio is always on. There is no enable/disable flag: the user selects the
devices in the OS and that is the whole control surface.

## 2. Wire format

`Msg::Audio` already exists in `pheme-proto` and does not change:

```rust
Msg::Audio { stream: AudioStream, seq: u32, ts_us: u64, samples: Vec<u8> }
```

Sub-project 2 uses `AudioStream::Playback` only (client → server).
`AudioStream::Mic` stays unused until sub-project 3.

- `samples` is interleaved little-endian i16 PCM: 240 samples per channel,
  2 channels, 960 bytes. One frame is 5 ms at 48 kHz. Encoded as a byte
  vector so postcard writes it verbatim (≈ 975 B per datagram, well under
  the 1200 B datagram budget).
- `seq` increments by exactly 1 for every 5 ms of client audio time,
  **including frames that silence suppression drops**. Gaps therefore
  always mean "this much audio time is missing", whether the cause was
  packet loss or suppression.
- `ts_us` is the client's monotonic capture timestamp. It is diagnostic
  only; the two machines have no shared clock and nothing in the pipeline
  synchronises on it.
- Audio travels in QUIC datagrams (`Msg::is_datagram()` already returns
  true for it). Lost frames are concealed, never retransmitted.
- `AudioParams` is already exchanged in `Hello`/`HelloAck`. If the peer
  reports anything other than 48000 / 2 / 240, log an error once and run
  the session without audio; keyboard and mouse are unaffected.

### Silence suppression

Audio is sent only while there is something to hear.

- A frame is *silent* when every sample is exactly zero. The test is
  exact, not a threshold, so quiet content is never clipped — digital
  silence from an idle OS mixer is exactly zero.
- After 40 consecutive silent frames (200 ms), the client stops sending.
  `seq` keeps counting.
- The first non-silent frame resumes sending immediately. The server sees
  a gap larger than its reset threshold and re-prefills, which costs
  10 ms at the exact moment audio starts — inaudible.

## 3. `pheme-audio`

```
crates/pheme-audio/src/
  lib.rs              constants, Error, traits, detect_capture/detect_playback
  frame.rs            Frame, i16 <-> LE byte conversion
  pack.rs             Packer: samples -> Frame, seq, silence suppression
  jitter.rs           JitterBuffer: reorder, conceal, adaptive target
  drift.rs            DriftController: clock drift -> resample ratio
  mock.rs             MockCapture / MockPlayback
  linux_pipewire.rs   Linux capture (virtual sink) and playback
  windows/mod.rs      Windows module root
  windows/wasapi.rs   Windows loopback capture and render
```

### Constants

```rust
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_SAMPLES: usize = 240;                              // per channel
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS;     // 480
pub const FRAME_BYTES: usize = FRAME_INTERLEAVED * 2;              // 960
pub const FRAME_US: u64 = 5_000;
```

### Traits

```rust
pub trait AudioCapture: Send {
    /// Opens the device and starts writing interleaved i16 samples into `sink`.
    /// The device callback must never block: when `sink` is full the backend
    /// drops the oldest samples and increments its overrun counter.
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<(), Error>;
    /// Human-readable name of the device actually in use, for logs.
    fn device_name(&self) -> String;
    /// Idempotent. Must join the device thread before returning.
    fn stop(&mut self);
}

pub trait AudioPlayback: Send {
    /// Opens the device and starts draining interleaved i16 samples from
    /// `source`. On underrun the backend writes silence and increments its
    /// underrun counter rather than blocking.
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<(), Error>;
    /// The device's actual sample rate, valid only after `start` succeeded.
    /// The playback worker uses it as the base resample ratio.
    fn rate(&self) -> u32;
    fn device_name(&self) -> String;
    fn stop(&mut self);
}

pub enum Error {
    Unsupported(String),   // no backend for this platform/session
    Device(String),        // device open/format failure
    Backend(String),       // backend-internal failure
}

pub fn detect_capture(device: Option<&str>) -> Result<Box<dyn AudioCapture>, Error>;
pub fn detect_playback(device: Option<&str>) -> Result<Box<dyn AudioPlayback>, Error>;
```

`detect_*` picks PipeWire on Linux and WASAPI on Windows, and returns
`Error::Unsupported` elsewhere. `device` is an optional device name from
the config; `None` means the platform default.

Both traits follow the `pheme-input` contract: `start` is synchronous and
returns only once the device is running or has failed, and `stop` joins
the device thread before returning.

### `frame.rs`

```rust
pub struct Frame { pub seq: u32, pub ts_us: u64, pub bytes: Vec<u8> }

pub fn samples_to_bytes(src: &[i16], dst: &mut Vec<u8>);
pub fn bytes_to_samples(src: &[u8], dst: &mut Vec<i16>);
```

`bytes` is always `FRAME_BYTES` long. `bytes_to_samples` rejects a slice
whose length is not a multiple of 4 (one stereo sample pair).

### `pack.rs`

```rust
pub struct Packer { /* seq, silent_run */ }

impl Packer {
    pub fn new() -> Self;
    /// `samples` must be FRAME_INTERLEAVED long. Advances `seq` on every
    /// call. Returns None while the suppression window is open.
    pub fn push(&mut self, samples: &[i16], ts_us: u64) -> Option<Frame>;
    pub fn suppressed(&self) -> bool;
}
```

Suppression rule: `silent_run` counts consecutive all-zero frames; while
`silent_run > 40` the method returns `None`. A non-silent frame resets the
run to 0 and is always sent.

### `jitter.rs`

```rust
pub struct JitterBuffer { /* map seq->Frame, target, last_popped, ... */ }

pub enum Pop {
    Data(Vec<i16>),      // a real frame
    Conceal(Vec<i16>),   // faded copy of the previous frame
    Idle,                // prefilling, suppressed, or concealment exhausted
}

#[derive(Default, Clone, Copy)]
pub struct JitterStats {
    pub depth: usize, pub target: usize,
    pub lost: u64, pub late: u64, pub dup: u64,
    pub underruns: u64, pub resets: u64, pub malformed: u64,
}

impl JitterBuffer {
    pub fn new() -> Self;              // target = 2 frames (10 ms)
    pub fn push(&mut self, f: Frame);
    pub fn pop(&mut self) -> Pop;      // called once per output frame
    pub fn stats(&self) -> JitterStats;
}
```

Behaviour:

| Situation | Result |
|---|---|
| In-order frame | stored |
| Out-of-order frame not yet passed | stored — this is what the buffer is for |
| `seq` <= last popped | dropped, `late += 1` |
| `seq` already stored | dropped, `dup += 1` |
| Payload length is not exactly 960 bytes | dropped, `malformed += 1` |
| A gap > 200 frames (1 s), ahead or behind | flush, `resets += 1`, re-prefill |
| Pop while depth < target (startup or after reset) | `Idle` |
| Pop with the next `seq` present | `Data`, `depth` recomputed |
| Pop with the next `seq` missing, last real frame **not** silent | `lost += 1`, `underruns += 1`, `Conceal` |
| Pop with the next `seq` missing, last real frame **silent** | `Idle`, nothing counted — see below |
| 5th consecutive missing frame onwards | `Idle` |

Concealment repeats the last real frame with a linear gain of 1.0, 0.75,
0.5, 0.25 across four frames, then silence. The gain is applied per sample
with saturating i16 arithmetic.

A gap is only treated as loss when the last real frame carried sound. A
client that enters silence suppression has by definition just sent at
least 40 all-zero frames, so the missing frames that follow are the
suppression window, not the network. Counting them would inflate `lost`
and — far worse — ratchet the adaptive target up to 40 ms every time the
user pauses their music. Real packet loss during silence is ignored too,
which costs nothing: the concealed content would have been silence.

Adaptive target: every underrun raises `target` by 1 frame, capped at 8
(40 ms). 2000 consecutive pops (10 s) with no underrun lowers `target` by
1, floored at 2 (10 ms). Latency grows fast and shrinks slowly so the
target does not oscillate.

### `drift.rs`

The two sound cards' clocks differ by tens of ppm, so without correction
the buffer drains or overflows after a few minutes even with zero packet
loss. Correction is a tiny, continuous change to the playback resample
ratio.

```rust
pub struct DriftController { /* base, depth_ema, ratio, tick */ }

impl DriftController {
    pub fn new(base_ratio: f64) -> Self;   // device_rate / 48000
    /// Called once per popped frame. Returns the ratio to use now.
    pub fn tick(&mut self, depth: usize, target: usize) -> f64;
}
```

- `depth_ema = 0.99 * depth_ema + 0.01 * depth`, seeded with the first
  observed depth.
- Every 20 ticks (100 ms):
  `adj = clamp(-0.002 * (depth_ema - target) / target, -0.001, 0.001)` and
  `ratio = base_ratio * (1.0 + adj)`.
- The sign is negative on purpose. The resampler turns one 480-sample
  input frame into `480 * ratio` output samples, and the device consumes
  output samples at a fixed rate, so a *smaller* ratio makes the worker
  pull input frames faster and drains the buffer. A buffer deeper than
  target therefore lowers the ratio.
- The ±0.1 % clamp is inaudible and drains one excess frame in about five
  seconds. `tick` takes no clock, so tests are deterministic.

The ratio is handed to `rubato::SincFixedIn::set_resample_ratio_relative`.
Resampler settings: `sinc_len = 64`, cubic interpolation, `f_cutoff` 0.95,
oversampling 128 — about 0.7 ms of added delay and negligible CPU at
48 kHz stereo.

### `mock.rs`

`MockCapture` pushes a caller-supplied sample sequence into the sink at a
caller-driven pace (a handle method, not a timer, so tests are
deterministic). `MockPlayback` drains the source into a recorded `Vec<i16>`
on demand and reports a configurable `rate()`. Both expose a handle for
assertions, mirroring `pheme_input::mock`.

### Unit tests (CI, no sound card)

`frame.rs`: round-trip including `i16::MIN` and `i16::MAX`; odd-length
input rejected.

`pack.rs`: silence for 39 frames still sends; the 41st is suppressed; the
first non-silent frame after suppression is sent and its `seq` reflects
every skipped frame; a frame with a single non-zero sample is not silent.

`jitter.rs`: a gap following a silent frame yields `Idle` and leaves
`lost`, `underruns` and `target` untouched; in-order playback; one frame
reordered by two positions is
still delivered; a single loss yields `Conceal` with the faded copy; six
consecutive losses yield four `Conceal` then `Idle`; a duplicate is
counted and ignored; a frame arriving after its slot passed counts as
`late`; a 300-frame gap resets and re-prefills; `target` rises after an
underrun; `target` falls after 2000 clean pops.

`drift.rs`: a source running 50 ppm fast for 60 s of simulated time drives
`depth_ema` back to `target` and never exceeds ±0.1 %; the same for 50 ppm
slow; a base ratio of 0.91875 (44 100 / 48 000) is preserved as the centre
of the correction.

## 4. Linux backend — PipeWire (`linux_pipewire.rs`)

One file holds both directions; both run a `pw::MainLoop` on a dedicated
thread and take commands through a `pw::channel`, the same shape as the
X11 capture backend. `start` waits for a readiness acknowledgement from
that thread (1 s timeout) before returning, so a failure to create the
node is reported as an error rather than silently dropped.

### Client capture — the virtual sink

A `pw::Stream` in `Direction::Input` whose properties declare it a sink.
PipeWire then presents a node that every application sees as a speaker,
and the audio written to it arrives in our `on_process` callback.

```
node.name        = "pheme-speaker"
node.description = "Pheme Speaker"
media.class      = "Audio/Sink"
media.role       = "Music"
audio.rate       = 48000
audio.channels   = 2
audio.format     = "S16LE"
node.latency     = "240/48000"
```

PipeWire converts whatever format an application uses, so the backend
never writes a converter. `on_process` dequeues the buffer, copies the
samples into the `rtrb` producer and returns; it allocates nothing and
takes no lock.

The node exists for the lifetime of the process, connected or not. When
there is no session the samples are simply dropped, which is what the
architecture requires: the OS must not fall back to another default
device just because the peer went away.

### Server playback

A `pw::Stream` in `Direction::Output`, `media.class = "Stream/Output/Audio"`,
same format, `node.latency = "240/48000"`. (An earlier draft of this spec said
`Audio/Playback`, which is not a PipeWire media class at all; it made
`libspa-audioconvert` segfault as soon as the stream reached real hardware.
The format must also carry explicit channel positions.) With no target it follows the
default sink. With `audio.playback_device` set, the backend passes the
value to PipeWire as `target.object`, which matches a node name or
serial; an unknown value falls back to the default sink. Matching a
human-readable description would need a registry walk and is not worth
the code for v1 — `pactl list sinks short` prints the node names.
`rate()` returns 48000 because PipeWire performs any device-rate
conversion itself.

`on_process` fills the buffer from the `rtrb` consumer and pads with
silence on underrun.

### Failure handling

If the PipeWire daemon is missing, `detect_*` returns
`Error::Unsupported("PipeWire is not available")`. If the daemon restarts
mid-session the stream errors; the backend reports it and `pheme-app`
rebuilds the whole backend on its five-second retry cycle.

## 5. Windows backend — WASAPI (`windows/wasapi.rs`)

Uses `windows-rs`, already a dependency of `pheme-input`. Both directions
run a dedicated thread that calls `CoInitializeEx(COINIT_MULTITHREADED)`
on entry and `CoUninitialize` on exit.

### Client capture — loopback

`IMMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)` →
`IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED,
AUDCLNT_STREAMFLAGS_LOOPBACK, ...)` with a 20 ms buffer, then
`IAudioCaptureClient`.

- Loopback cannot be driven reliably by the event callback, so the thread
  polls every 2.5 ms. It calls `timeBeginPeriod(1)` on start and
  `timeEndPeriod(1)` on stop; the 20 ms buffer absorbs any remaining
  timer slip.
- The mix format is normally 32-bit float, 48 kHz, stereo. The backend
  converts float32 → i16 with saturation. A device running at another
  rate is resampled to 48 kHz with `rubato` before entering the ring, so
  everything downstream stays at 48 kHz.
- A mono device is duplicated to stereo; more than two channels are
  downmixed to the first two.
- `AUDCLNT_BUFFERFLAGS_SILENT` is honoured by writing zeros, which the
  packer's exact-zero test then suppresses.
- `AUDCLNT_E_DEVICE_INVALIDATED` (the user changed the default output or
  unplugged headphones) closes and reopens against the new default
  without disturbing the session. The thread also re-reads the default
  endpoint every 2 s to catch silent switches.
- `audio.capture_device` selects a specific render endpoint by friendly
  name instead of the default. Because loopback capture attaches to a
  **render** endpoint, the value names the playback half of the cable —
  `CABLE Input` — not its recording half; naming `CABLE Output` matches
  nothing and silently falls back to the default. This is how a user
  makes the client completely silent locally; the default path keeps the
  audio audible on the client as well.

### Server playback — render

`GetDefaultAudioEndpoint(eRender, eConsole)` →
`IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED,
AUDCLNT_STREAMFLAGS_EVENTCALLBACK, ...)` with the device's default period
(10 ms on shared mode; `IAudioClient3` low-latency mode is out of scope),
`SetEventHandle`, then `IAudioRenderClient`. The thread waits on the
event, asks for the padding, and writes that many frames from the `rtrb`
consumer, converting i16 → float32 (or i16 pass-through when the mix
format is 16-bit). `rate()` returns the mix format's rate, which becomes
the playback worker's base resample ratio. `audio.playback_device`
selects an endpoint by friendly name. `AUDCLNT_E_DEVICE_INVALIDATED` is
handled the same way as on the capture side.

## 6. `pheme-app` integration

New file `crates/pheme-app/src/audio.rs`.

### Config

```toml
[audio]
playback_device = "Speakers (Realtek)"   # optional, server side
capture_device  = "CABLE Input"          # optional, Windows client only
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct AudioCfg {
    pub playback_device: Option<String>,
    pub capture_device: Option<String>,
}
```

`Config` gains `#[serde(default)] pub audio: AudioCfg`. An absent
`[audio]` section is valid and means "defaults everywhere". There is no
enable flag by design.

### Client side (capture → network)

```
device callback --> rtrb ring (100 ms) --> packer thread --> PeerSender::send_datagram
```

The packer thread wakes every 2.5 ms, and while at least
`FRAME_INTERLEAVED` samples are available it pops one frame, timestamps it
from a monotonic frame counter (`seq * FRAME_US`, not wall time), runs
`Packer::push`, and on `Some(frame)` sends `Msg::Audio` as a datagram.
`send_datagram` is non-blocking and lossy by design, which is correct for
audio.

The thread is started when a session is established and stopped when it
ends, but the capture backend itself — and therefore the virtual sink —
is created once at startup and lives for the whole process.

### Server side (network → playback)

```
net task --> crossbeam bounded(256) --> worker thread --> rubato --> rtrb (4 frames) --> device callback
```

- The tokio task that already drains `Peer::take_incoming()` matches
  `Msg::Audio { stream: Playback, .. }`, builds a `Frame` and `try_send`s
  it. A full channel means the worker is wedged; drop the frame and count
  it. The net task never blocks and never locks.
- The worker thread owns the `JitterBuffer`, the `DriftController` and the
  resampler. Each iteration drains the channel into the buffer, then
  **while the ring holds fewer than 2 frames**: `pop()` → resample at
  `drift.tick(depth, target)` → push. It sleeps 2 ms between iterations.
  The ring's capacity is 4 frames, but the worker deliberately keeps it
  around one frame deep: a ring kept full would add its whole capacity to
  the end-to-end latency for no benefit.
  It is not a real-time thread: the device callback always has a ring to
  drain from.
- `Pop::Idle` writes a frame of silence so the ring never starves.

### Failure policy

Audio never interferes with keyboard and mouse. Any backend error —
missing PipeWire, device open failure, a daemon restart, an invalidated
endpoint that cannot be reopened — is logged at `warn` once, the backend
is dropped, and a retry is attempted every 5 s. `Error::Unsupported` is
logged once at `info` and not retried. `run_server` and `run_client` never
return an error because of audio.

### Stats

With `--stats`, the client's per-second line gains `audio_sent` (frames)
and `audio_suppressed` (frames skipped by silence suppression). The
server's line gains `audio_depth_ms`, `audio_lost`, `audio_late`,
`audio_underruns` and `audio_resets`, all read from `JitterStats` and
reset per interval except `audio_depth_ms`, which is the last observed
depth.

### Integration tests (`crates/pheme-app/tests/`)

Over a real QUIC connection with mock input and mock audio backends:

1. `audio_flows_client_to_server`: the client's mock capture emits a
   440 Hz sine; the server's mock playback recording carries a tone of
   the same amplitude. The comparison is on level and length, not sample
   for sample, because the playback resampler filters even at a ratio of
   1.0.
2. `silence_stops_the_traffic_and_resuming_restores_it`: after 200 ms of
   silence the client stops sending and `audio_suppressed` rises while
   `audio_sent` stops; resuming the tone makes playback resume.
3. `audio_failure_does_not_break_kvm`: the mock capture backend fails to
   start; an edge crossing still switches control and keys still arrive.

Packet loss is not one of these: QUIC on loopback does not drop
datagrams, and a harness that dropped them would be testing itself. Loss
handling is covered by the `jitter.rs` unit tests, which can create any
loss pattern exactly.

## 7. Latency budget

Every stage that holds audio, measured one way:

| Stage | Linux → Linux | With Windows on either end |
|---|---|---|
| Capture device buffer / poll | 5 ms (`node.latency 240/48000`) | 2.5–5 ms (2.5 ms loopback poll) |
| Packer thread wake-up | ≤ 2.5 ms | ≤ 2.5 ms |
| Network (wired LAN) | ~0.5 ms | ~0.5 ms |
| Jitter buffer target | 10 ms | 10 ms |
| Resampler (sinc_len 64) | 0.7 ms | 0.7 ms |
| Playback ring (kept ~1 frame) | ~5 ms | ~5 ms |
| Playback device buffer | 5 ms | 10 ms (WASAPI shared mode) |
| **Total** | **~29 ms** | **~33 ms** |

The architecture document's 20–25 ms estimate assumed a 5 ms device
buffer on both ends, which only holds on PipeWire. Shared-mode WASAPI
costs roughly 10 ms that we cannot remove without `IAudioClient3`. The
definition of done therefore uses 40 ms, which leaves headroom for the
adaptive jitter target to reach 15–20 ms on a noisy link before the
figure is missed.

## 8. Definition of done

- Music played on the client is audible on the server's speakers in both
  Windows → Linux and Linux → Windows directions.
- Measured end-to-end latency below 40 ms (test A7 below).
- Ten minutes of continuous playback with `audio_underruns = 0` and
  `audio_depth_ms` steady between 10 and 15.
- Disconnecting and reconnecting the network resumes audio without a
  restart.
- An audio backend failure leaves keyboard and mouse fully working.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace` green on both the Ubuntu
  and Windows CI jobs.
- README documents selecting "Pheme Speaker" on Linux, the Windows
  loopback default and the optional VB-CABLE setup, and the
  `libpipewire-0.3-dev` + `clang` build requirement.

## 9. Manual test matrix (added to `docs/testing.md`)

Every row runs twice: Windows client → Linux server, and Linux client →
Windows server.

| # | Check |
|---|---|
| A1 | Select "Pheme Speaker" / default output on the client, play music: audible on the server, no crackle, no stutter |
| A2 | Ten minutes continuous: `--stats` reports `audio_underruns = 0`, `audio_depth_ms` steady at 10–15 |
| A3 | Unplug the network for 3 s and reconnect: audio resumes by itself |
| A4 | Change the default output device on the client mid-stream (Windows): audio continues on the new device |
| A5 | Pause playback for 30 s: no audio traffic; resuming produces sound within 100 ms |
| A6 | Stop the server: the client still offers "Pheme Speaker", logs no errors and does not hang |
| A7 | Latency: play a click track on the client, record both speakers with a phone, measure the offset — under 40 ms |

## 10. Known risks

- **`pipewire-rs` needs bindgen.** Builds fail on systems without
  `libpipewire-0.3-dev`, `clang` and `pkg-config`. Mitigation: document it
  in the README, add it to the Ubuntu CI job, and make the runtime error
  say exactly which package is missing.
- **Windows timer resolution.** Without `timeBeginPeriod(1)` a 2.5 ms
  poll can slip to 15 ms. Mitigation: call it, and size the WASAPI buffer
  at 20 ms so a slip costs no samples.
- **Devices that do not run at 48 kHz.** The base resample ratio then
  differs from 1.0 on a path that gets little real-world testing.
  Mitigation: a unit test pinning the 44 100 / 48 000 base ratio, and a
  log line stating the device rate at startup.
- **PipeWire daemon restarts** kill the stream mid-session; covered by the
  five-second retry cycle, but the virtual sink disappears briefly and
  applications may fall back to another device. Accepted for v1.
- **Bandwidth.** Uncompressed stereo is 1.54 Mbit/s, trivial on wired LAN
  but noticeable on a weak Wi-Fi link shared with other traffic. Silence
  suppression removes it while nothing is playing. A codec stays out of
  scope; the stated priority is audio quality.
