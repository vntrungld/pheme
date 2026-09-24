//! PipeWire backends: the client's virtual sink and the server's playback stream.
//!
//! The PipeWire main loop runs on a dedicated thread and takes commands through a
//! `pw::channel`, the same shape as the X11 capture backend in `pheme-input`. `start`
//! waits for the thread to acknowledge that the node exists, so a failure is reported to
//! the caller instead of disappearing into a log line.
//!
//! Everything that touches a `pw::stream`, `pw::context` or `pw::main_loop` object is
//! built and torn down inside a single function (`run`, below) instead of being handed
//! back to the caller. The 0.10 API ties non-`'static` PipeWire wrappers (and anything
//! borrowed from them, such as the attached channel receiver) to the lifetime of the
//! object that created them, so splitting construction into a helper that *returns*
//! those pieces does not type-check; keeping them all as locals in one function sidesteps
//! that entirely and matches how the upstream `pipewire` crate's own examples do it.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamState};
use tracing::{debug, warn};

use crate::device::{DeviceThread, Ready};
use crate::{AudioCapture, AudioPlayback, Demand, Error, Result, CHANNELS, RATE};

/// How long `start` waits for the PipeWire thread to report success or failure.
const START_TIMEOUT: Duration = Duration::from_secs(1);

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(pw::init);
}

enum Cmd {
    Stop,
}

/// Builds the SPA pod describing S16LE at 48 kHz with `channels` channels.
///
/// Channel positions are declared explicitly rather than left unpositioned: negotiating
/// a stereo mix from an unpositioned source into a positioned sink segfaults inside
/// `libspa-audioconvert`, which cost a core dump to find.
fn format_pod(channels: usize) -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(channels as u32);
    // `AudioInfoRaw::new()` leaves the channel-position array unpositioned (all-zero,
    // `AudioInfoRawFlags::UNPOSITIONED`). That is fine for the capture side, whose peer
    // is always another PipeWire stream on the *input* side of the conversion. For
    // playback, `StreamFlags::AUTOCONNECT` links this stream's output straight into the
    // system's real (positioned) hardware sink, and negotiating a stereo mix from an
    // unpositioned source into a positioned sink crashes `libspa-audioconvert.so`
    // (segfault inside the channel-mix code, reproduced with a minimal scratch
    // reproduction and a core dump backtrace through
    // `pw_main_loop_run -> ... -> libspa-audioconvert.so`, with no Rust frames beyond
    // `MainLoop::run`). Declaring channel positions explicitly — front-left/front-right
    // for stereo, or the single mono position for a one-channel node — avoids that code
    // path entirely and matches how the upstream `tone.rs` example builds its format.
    let mut position = [0u32; pw::spa::param::audio::MAX_CHANNELS];
    if channels == 1 {
        position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    } else {
        position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(position);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| Error::Backend(format!("building the audio format pod: {e}")))?
        .0
        .into_inner();
    Ok(bytes)
}

/// Decides, from a stream state change, whether the stream is gone for good.
///
/// `Error` is unambiguous. `Unconnected` is not: every stream starts there, so it only
/// means the stream has died once it has been `Streaming` at least once — which is what
/// `streamed` tracks. Everything else (`Connecting`, `Paused`) is ordinary: a sink with
/// no application playing into it sits in `Paused` indefinitely.
fn stream_died(streamed: &mut bool, new: &StreamState) -> Option<String> {
    match new {
        StreamState::Streaming => {
            *streamed = true;
            None
        }
        StreamState::Error(e) => Some(format!("the stream reported an error: {e}")),
        StreamState::Unconnected if *streamed => {
            Some("the stream was disconnected after running".into())
        }
        _ => None,
    }
}

/// The client's virtual sink: applications play into "Pheme Speaker" and we read it.
#[derive(Default)]
pub struct PipewireCapture {
    thread: DeviceThread,
}

impl PipewireCapture {
    pub fn new() -> PipewireCapture {
        PipewireCapture::default()
    }
}

impl AudioCapture for PipewireCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        self.thread.start(
            "pheme-pw-sink",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| {
                // Built on the device thread, not before it is spawned: `PropertiesBox`
                // wraps a raw `pw_properties` pointer and is not `Send`.
                //
                // `Direction::Input` plus `media.class = Audio/Sink` is what makes this
                // a sink that other applications can select, rather than a recording
                // stream.
                let props = pw::properties::properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_CLASS => "Audio/Sink",
                    *pw::keys::MEDIA_ROLE => "Music",
                    *pw::keys::NODE_NAME => "pheme-speaker",
                    *pw::keys::NODE_DESCRIPTION => "Pheme Speaker",
                    *pw::keys::AUDIO_RATE => "48000",
                    *pw::keys::AUDIO_CHANNELS => "2",
                    *pw::keys::NODE_LATENCY => "240/48000",
                };
                if let Err(e) = capture_run(props, sink, &ready, cmd_rx, "Pheme Speaker") {
                    warn!("Pheme Speaker could not start: {e}");
                    // `capture_run` only returns `Err` before it has sent a readiness
                    // reply, so this is the one and only reply in the failure path.
                    ready.fail(e);
                }
            },
        )
    }

    fn device_name(&self) -> String {
        "Pheme Speaker".into()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for PipewireCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything the process callback touches. It runs on PipeWire's real-time thread, so
/// it allocates nothing and takes no lock: `Producer::push` on an `rtrb` ring buffer is
/// lock-free and never allocates, and a full ring simply drops the sample.
struct CaptureData {
    sink: rtrb::Producer<i16>,
}

/// The capture side of both PipeWire backends: build a node from `props`, drain its
/// buffers into `sink` until asked to stop.
///
/// On success this sends `Ok(())` on `ready` once the node is connected and then blocks
/// in `mainloop.run()` until a `Cmd::Stop` arrives. On failure it returns `Err` without
/// sending anything, leaving the caller to reply on `ready`. `label` names the node in
/// log messages; the virtual sink and the microphone differ only in `props` and `label`.
fn capture_run(
    props: pw::properties::PropertiesBox,
    sink: rtrb::Producer<i16>,
    ready: &Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
    label: &str,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| Error::Device(format!("creating the PipeWire main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| Error::Device(format!("creating the PipeWire context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| Error::Device(format!("connecting to PipeWire: {e}")))?;

    // A stream listener only sees what happens to *our* node. When the daemon itself
    // goes away the connection to it is what breaks, and the core reports that here.
    // `error` is documented as fatal and non-recoverable, so there is nothing to do but
    // end the loop and let the supervisor build a fresh backend against the new daemon.
    let _core_listener = core
        .add_listener_local()
        .error({
            let quit_loop = mainloop.clone();
            move |id, seq, res, message| {
                warn!(
                    id,
                    seq, res, message, "the PipeWire connection failed; ending the PipeWire thread"
                );
                quit_loop.quit();
            }
        })
        .register();

    let stream = pw::stream::StreamBox::new(&core, label, props)
        .map_err(|e| Error::Device(format!("creating the {label} node: {e}")))?;

    let _listener = stream
        .add_local_listener_with_user_data(CaptureData { sink })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else {
                return;
            };
            let offset = d.chunk().offset() as usize;
            let size = d.chunk().size() as usize;
            let Some(slice) = d.data() else {
                return;
            };
            let start = offset.min(slice.len());
            let end = (offset + size).min(slice.len());
            for c in slice[start..end].chunks_exact(2) {
                let s = i16::from_le_bytes([c[0], c[1]]);
                let _ = data.sink.push(s);
            }
        })
        .state_changed({
            let quit_loop = mainloop.clone();
            let mut streamed = false;
            let label = label.to_string();
            move |_, _, old, new| {
                debug!(?old, ?new, label, "capture stream state");
                if let Some(why) = stream_died(&mut streamed, &new) {
                    warn!("{label} is gone: {why}; ending the PipeWire thread");
                    quit_loop.quit();
                }
            }
        })
        .register()
        .map_err(|e| Error::Device(format!("registering the stream listener: {e}")))?;

    let bytes = format_pod(CHANNELS)?;
    let mut params = [Pod::from_bytes(&bytes)
        .ok_or_else(|| Error::Backend("the audio format pod is malformed".into()))?];
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| Error::Device(format!("connecting the {label} node: {e}")))?;

    let quit_loop = mainloop.clone();
    let _receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Cmd::Stop => quit_loop.quit(),
    });

    // The node exists and is connected: tell `start` it can return. If the caller has
    // already given up (the 1 s timeout in `start` elapsed), there is nothing to notify
    // and we fall through to `mainloop.run()`, which will exit as soon as the `Cmd::Stop`
    // that `start` sent on timeout is delivered.
    ready.ok();

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// The server's playback stream: samples in, speakers out.
pub struct PipewirePlayback {
    device: Option<String>,
    thread: DeviceThread,
}

impl PipewirePlayback {
    /// `device` is matched by PipeWire as `target.object`, i.e. against a node name or
    /// serial. `pactl list sinks short` prints the node names. An unknown value falls
    /// back to the default sink.
    pub fn new(device: Option<String>) -> PipewirePlayback {
        PipewirePlayback {
            device,
            thread: DeviceThread::new(),
        }
    }
}

impl AudioPlayback for PipewirePlayback {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let device = self.device.clone();
        self.thread.start(
            "pheme-pw-play",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| playback_thread(source, device, ready, cmd_rx),
        )
    }

    /// Always 48 kHz: we ask PipeWire for our format and it converts to whatever the
    /// hardware runs at, so the playback worker never needs a base resample ratio here.
    fn rate(&self) -> u32 {
        RATE
    }

    fn device_name(&self) -> String {
        self.device.clone().unwrap_or_else(|| "default sink".into())
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for PipewirePlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything the process callback touches. It runs on PipeWire's real-time thread, so
/// it allocates nothing and takes no lock: `Consumer::pop` on an `rtrb` ring buffer is
/// lock-free and never allocates, and an empty ring simply yields silence.
struct PlaybackData {
    source: rtrb::Consumer<i16>,
}

fn playback_thread(
    source: rtrb::Consumer<i16>,
    device: Option<String>,
    ready: Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
) {
    if let Err(e) = play_run(source, device, &ready, cmd_rx) {
        warn!("Pheme playback could not start: {e}");
        // `play_run` only returns `Err` before it has sent a readiness reply, so this is
        // the one and only reply in the failure path.
        ready.fail(e);
    }
}

/// Builds the playback node, runs the main loop until told to stop, and tears the node
/// down. Same shape as `run` above: everything that touches a PipeWire object lives as a
/// local in this one function, because the 0.10 ownership model ties the attached
/// channel receiver's lifetime to the `MainLoopRc` local that produced it.
fn play_run(
    source: rtrb::Consumer<i16>,
    device: Option<String>,
    ready: &Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| Error::Device(format!("creating the PipeWire main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| Error::Device(format!("creating the PipeWire context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| Error::Device(format!("connecting to PipeWire: {e}")))?;

    // A stream listener only sees what happens to *our* node. When the daemon itself
    // goes away the connection to it is what breaks, and the core reports that here.
    // `error` is documented as fatal and non-recoverable, so there is nothing to do but
    // end the loop and let the supervisor build a fresh backend against the new daemon.
    let _core_listener = core
        .add_listener_local()
        .error({
            let quit_loop = mainloop.clone();
            move |id, seq, res, message| {
                warn!(
                    id,
                    seq, res, message, "the PipeWire connection failed; ending the PipeWire thread"
                );
                quit_loop.quit();
            }
        })
        .register();

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        // "Audio/Playback" (the brief's listing) is not a real PipeWire media class —
        // "Audio/Sink"/"Audio/Source"/"Audio/Duplex" name *device* nodes, and an
        // application-side stream like this one is "Stream/Output/Audio". Setting the
        // device-shaped class here made WirePlumber link this stream into the real
        // hardware sink in a way that segfaulted inside `libspa-audioconvert.so` as soon
        // as the stream connected (reproduced with a minimal `tone.rs`-based scratch
        // program and a core-dump backtrace showing the crash entirely inside
        // libspa/libpipewire, below `pw_main_loop_run`, with no Rust frames of ours on
        // the stack). The standard class fixed it.
        *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "pheme-playback",
        *pw::keys::NODE_DESCRIPTION => "Pheme Playback",
        *pw::keys::AUDIO_RATE => "48000",
        *pw::keys::AUDIO_CHANNELS => "2",
        *pw::keys::NODE_LATENCY => "240/48000",
    };
    if let Some(d) = device.as_deref() {
        // `pw::keys::TARGET_OBJECT` exists only under the "v0_3_44" feature, which this
        // crate deliberately does not enable (see the Cargo.toml comment on the
        // `pipewire` dependency). The property name itself is a stable part of the
        // PipeWire protocol regardless of which pipewire-rs binding exposes a constant
        // for it, so it is spelled out here as a plain string.
        props.insert("target.object", d);
    }

    let stream = pw::stream::StreamBox::new(&core, "pheme-playback", props)
        .map_err(|e| Error::Device(format!("creating the playback node: {e}")))?;

    let _listener = stream
        .add_local_listener_with_user_data(PlaybackData { source })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let stride = 2 * CHANNELS;
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else {
                return;
            };
            let frames = match d.data() {
                Some(slice) => {
                    let frames = slice.len() / stride;
                    for i in 0..frames * CHANNELS {
                        // An empty ring (nothing produced yet, or the producer fell
                        // behind) writes silence rather than stalling the callback.
                        let s = data.source.pop().unwrap_or(0);
                        slice[i * 2..i * 2 + 2].copy_from_slice(&s.to_le_bytes());
                    }
                    frames
                }
                None => 0,
            };
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .state_changed({
            let quit_loop = mainloop.clone();
            let mut streamed = false;
            move |_, _, old, new| {
                debug!(?old, ?new, "Pheme playback state");
                if let Some(why) = stream_died(&mut streamed, &new) {
                    warn!("Pheme playback is gone: {why}; ending the PipeWire thread");
                    quit_loop.quit();
                }
            }
        })
        .register()
        .map_err(|e| Error::Device(format!("registering the stream listener: {e}")))?;

    let bytes = format_pod(CHANNELS)?;
    let mut params = [Pod::from_bytes(&bytes)
        .ok_or_else(|| Error::Backend("the audio format pod is malformed".into()))?];
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| Error::Device(format!("connecting the playback node: {e}")))?;

    let quit_loop = mainloop.clone();
    let _receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Cmd::Stop => quit_loop.quit(),
    });

    // The node exists and is connected: tell `start` it can return. See `run`'s comment
    // above for what happens if the caller has already given up.
    ready.ok();

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// The server's microphone: a recording stream against a real capture device.
pub struct PipewireMic {
    device: Option<String>,
    thread: DeviceThread,
}

impl PipewireMic {
    /// `device` is matched by PipeWire as `target.object`, i.e. against a node name or
    /// serial, exactly as `PipewirePlayback` matches an output. `pactl list sources
    /// short` prints the node names. An unknown value falls back to the default source.
    pub fn new(device: Option<String>) -> PipewireMic {
        PipewireMic {
            device,
            thread: DeviceThread::new(),
        }
    }
}

impl AudioCapture for PipewireMic {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        init();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let device = self.device.clone();
        self.thread.start(
            "pheme-pw-mic-cap",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| {
                // Built on the device thread, not before it is spawned: `PropertiesBox`
                // wraps a raw `pw_properties` pointer and is not `Send`.
                let mut props = pw::properties::properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
                    *pw::keys::MEDIA_ROLE => "Communication",
                    *pw::keys::NODE_NAME => "pheme-mic-capture",
                    *pw::keys::NODE_DESCRIPTION => "Pheme microphone capture",
                    *pw::keys::AUDIO_RATE => "48000",
                    *pw::keys::AUDIO_CHANNELS => "2",
                    *pw::keys::NODE_LATENCY => "240/48000",
                };
                if let Some(d) = device.as_deref() {
                    // `pw::keys::TARGET_OBJECT` exists only under the "v0_3_44"
                    // feature, which this crate deliberately does not enable (see the
                    // Cargo.toml comment on the `pipewire` dependency, and
                    // `play_run`'s matching device property above). The property name
                    // is a stable part of the PipeWire protocol regardless of which
                    // pipewire-rs binding exposes a constant for it, so it is spelled
                    // out here as a plain string.
                    props.insert("target.object", d);
                }
                if let Err(e) = capture_run(props, sink, &ready, cmd_rx, "Pheme microphone") {
                    warn!("Pheme microphone could not start: {e}");
                    // `capture_run` only returns `Err` before it has sent a readiness
                    // reply, so this is the one and only reply in the failure path.
                    ready.fail(e);
                }
            },
        )
    }

    fn device_name(&self) -> String {
        self.device
            .clone()
            .unwrap_or_else(|| "default source".into())
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    fn stop(&mut self) {
        self.thread.stop();
    }
}

impl Drop for PipewireMic {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The client's virtual microphone: we write samples, applications record them.
///
/// The mirror of `PipewireCapture`. `Direction::Output` plus `media.class = Audio/Source`
/// is what makes this a recording device other applications can select, rather than a
/// playback stream.
pub struct PipewireVirtualSource {
    node: String,
    description: String,
    channels: usize,
    demand: Arc<AtomicU8>,
    thread: DeviceThread,
}

/// `Demand` as an atomic, because the stream listener sets it from the PipeWire thread.
const DEMAND_UNKNOWN: u8 = 0;
const DEMAND_WANTED: u8 = 1;
const DEMAND_IDLE: u8 = 2;

impl Default for PipewireVirtualSource {
    fn default() -> Self {
        PipewireVirtualSource::new()
    }
}

impl PipewireVirtualSource {
    pub fn new() -> PipewireVirtualSource {
        PipewireVirtualSource::with_channels("pheme-mic", "Pheme Mic", CHANNELS)
    }

    /// The general form. Only tests pass anything but `CHANNELS`: production has one
    /// audio format and it is stereo.
    pub fn with_channels(node: &str, description: &str, channels: usize) -> PipewireVirtualSource {
        PipewireVirtualSource {
            node: node.to_string(),
            description: description.to_string(),
            channels,
            // Unknown until the node exists: until then we do not know, and not knowing
            // means keep the far end's microphone open.
            demand: Arc::new(AtomicU8::new(DEMAND_UNKNOWN)),
            thread: DeviceThread::new(),
        }
    }
}

impl AudioPlayback for PipewireVirtualSource {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        init();
        // A node that has just been created provably has no consumers yet, and the state
        // listener only starts reporting once the main loop is dispatching — after
        // `start` returns. Left at `Unknown`, the first demand polls would read "wanted",
        // publish it, and the `Idle` arriving milliseconds later would then have to wait
        // out the full linger: the server's microphone would open, and its indicator
        // light, at every session start with nothing recording. That is the exact
        // symptom this feature exists to remove. §3.1's rule protects against *failing
        // to know*; this is knowing the answer is no, the same justification `stop` uses.
        self.demand.store(DEMAND_IDLE, Ordering::SeqCst);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let demand = self.demand.clone();
        let node = self.node.clone();
        let description = self.description.clone();
        let channels = self.channels;
        self.thread.start(
            "pheme-pw-mic",
            START_TIMEOUT,
            move || {
                let _ = cmd_tx.send(Cmd::Stop);
            },
            move |ready| {
                virtual_source_thread(source, node, description, channels, demand, ready, cmd_rx)
            },
        )
    }

    /// Always 48 kHz: we create this node, so its rate is ours to choose and the base
    /// resample ratio is exactly 1.0.
    fn rate(&self) -> u32 {
        RATE
    }

    fn device_name(&self) -> String {
        self.description.clone()
    }

    fn healthy(&self) -> bool {
        self.thread.healthy()
    }

    /// Derived from the stream state, which tracks consumers exactly: a source with
    /// nothing recording from it sits in `Paused` and moves to `Streaming` when an
    /// application connects. Measured on PipeWire 1.6.8 before this was designed.
    ///
    /// A level meter counts as a consumer — an open sound-settings input page, or
    /// `pavucontrol` — which is correct: something really is listening.
    fn demand(&self) -> Demand {
        match self.demand.load(Ordering::SeqCst) {
            DEMAND_WANTED => Demand::Wanted,
            DEMAND_IDLE => Demand::Idle,
            _ => Demand::Unknown,
        }
    }

    fn stop(&mut self) {
        self.thread.stop();
        // A node that no longer exists has no consumers, and saying "unknown" here would
        // hold the far end's microphone open for a device that cannot deliver to anyone.
        self.demand.store(DEMAND_IDLE, Ordering::SeqCst);
    }
}

impl Drop for PipewireVirtualSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything the process callback touches. It runs on PipeWire's real-time thread, so
/// it allocates nothing and takes no lock: `Consumer::pop` on an `rtrb` ring buffer is
/// lock-free and never allocates, and an empty ring simply yields silence.
struct VirtualSourceData {
    source: rtrb::Consumer<i16>,
    channels: usize,
}

fn virtual_source_thread(
    source: rtrb::Consumer<i16>,
    node: String,
    description: String,
    channels: usize,
    demand: Arc<AtomicU8>,
    ready: Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
) {
    if let Err(e) = source_run(
        source,
        &node,
        &description,
        channels,
        demand,
        &ready,
        cmd_rx,
    ) {
        warn!("{description} could not start: {e}");
        // `source_run` only returns `Err` before it has sent a readiness reply, so this
        // is the one and only reply in the failure path.
        ready.fail(e);
    }
}

/// Builds the node, runs the main loop until told to stop, and tears the node down. Same
/// shape as `run` and `play_run` above: everything that touches a PipeWire object lives
/// as a local in this one function, because the 0.10 ownership model ties the attached
/// channel receiver's lifetime to the `MainLoopRc` local that produced it.
fn source_run(
    source: rtrb::Consumer<i16>,
    node: &str,
    description: &str,
    channels: usize,
    demand: Arc<AtomicU8>,
    ready: &Ready,
    cmd_rx: pw::channel::Receiver<Cmd>,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| Error::Device(format!("creating the PipeWire main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| Error::Device(format!("creating the PipeWire context: {e}")))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| Error::Device(format!("connecting to PipeWire: {e}")))?;

    // A stream listener only sees what happens to *our* node. When the daemon itself
    // goes away the connection to it is what breaks, and the core reports that here.
    // `error` is documented as fatal and non-recoverable, so there is nothing to do but
    // end the loop and let the supervisor build a fresh backend against the new daemon.
    let _core_listener = core
        .add_listener_local()
        .error({
            let quit_loop = mainloop.clone();
            move |id, seq, res, message| {
                warn!(
                    id,
                    seq, res, message, "the PipeWire connection failed; ending the PipeWire thread"
                );
                quit_loop.quit();
            }
        })
        .register();

    let stream = pw::stream::StreamBox::new(
        &core,
        node,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => node,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::AUDIO_RATE => "48000",
            *pw::keys::AUDIO_CHANNELS => channels.to_string(),
            *pw::keys::NODE_LATENCY => "240/48000",
        },
    )
    .map_err(|e| Error::Device(format!("creating the {description} node: {e}")))?;

    let _listener = stream
        .add_local_listener_with_user_data(VirtualSourceData { source, channels })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else {
                return;
            };
            let stride = 2 * data.channels;
            let Some(slice) = d.data() else {
                return;
            };
            let frames = slice.len() / stride;
            for f in 0..frames {
                for c in 0..data.channels {
                    // An empty ring plays silence rather than stalling the graph.
                    let v = data.source.pop().unwrap_or(0);
                    let at = f * stride + c * 2;
                    slice[at..at + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .state_changed({
            let quit_loop = mainloop.clone();
            let demand = demand.clone();
            let mut streamed = false;
            let description = description.to_string();
            move |_, _, old, new| {
                debug!(?old, ?new, description, "virtual source state");
                demand.store(
                    if matches!(new, StreamState::Streaming) {
                        DEMAND_WANTED
                    } else {
                        DEMAND_IDLE
                    },
                    Ordering::SeqCst,
                );
                if let Some(why) = stream_died(&mut streamed, &new) {
                    warn!("{description} is gone: {why}; ending the PipeWire thread");
                    quit_loop.quit();
                }
            }
        })
        .register()
        .map_err(|e| Error::Device(format!("registering the stream listener: {e}")))?;

    let bytes = format_pod(channels)?;
    let mut params = [Pod::from_bytes(&bytes)
        .ok_or_else(|| Error::Backend("the audio format pod is malformed".into()))?];
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| Error::Device(format!("connecting the {description} node: {e}")))?;

    let quit_loop = mainloop.clone();
    let _receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Cmd::Stop => quit_loop.quit(),
    });

    // The node exists and is connected: tell `start` it can return. See `run`'s comment
    // above for what happens if the caller has already given up.
    ready.ok();

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioCapture, AudioPlayback, Demand, CHANNELS, FRAME_INTERLEAVED, FRAME_SAMPLES};

    /// True when a PipeWire daemon is reachable. CI containers may not have one.
    fn have_pipewire() -> bool {
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").is_some()
            || std::env::var_os("XDG_RUNTIME_DIR")
                .is_some_and(|d| std::path::Path::new(&d).join("pipewire-0").exists())
    }

    #[test]
    fn the_virtual_source_starts_and_reports_no_consumers() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        let (_producer, consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 16);
        let mut src = PipewireVirtualSource::new();
        src.start(consumer).expect("the node must be created");
        assert!(src.healthy());
        assert_eq!(src.rate(), RATE, "we own this node, so it runs at 48 kHz");
        assert_eq!(src.device_name(), "Pheme Mic");

        // Nothing is recording from a node that has just been created, and reporting
        // otherwise would hold the far end's microphone open for no one.
        let mut saw_idle = false;
        for _ in 0..100 {
            if src.demand() == Demand::Idle {
                saw_idle = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_idle, "an unconsumed source must settle on Idle");
        src.stop();
        assert!(!src.healthy());
    }

    /// Drains `want` samples out of `consumer`, waiting up to two seconds.
    fn drain_at_least(consumer: &mut rtrb::Consumer<i16>, want: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(want);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while out.len() < want && std::time::Instant::now() < deadline {
            while let Ok(s) = consumer.pop() {
                out.push(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    }

    /// A single-channel `Audio/Source` node, so the mono upmix can be tested without
    /// mono hardware. Test-only: production never creates a mono node.
    fn mono_test_source() -> PipewireVirtualSource {
        PipewireVirtualSource::with_channels("pheme-mono-test", "Pheme Mono Test", 1)
    }

    #[test]
    fn the_mic_captures_from_our_own_virtual_source() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        // Feed a constant 6000 into Pheme Mic, then capture from it. This exercises the
        // virtual source, the microphone capture and the demand signal at once, and needs
        // no sound hardware at all.
        let (mut feed, src_ring) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut src = PipewireVirtualSource::new();
        src.start(src_ring).expect("virtual source");

        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut mic = PipewireMic::new(Some("pheme-mic".into()));
        mic.start(producer).expect("microphone capture");

        // Keep the source fed while the graph settles and runs.
        let feeder = std::thread::spawn(move || {
            for _ in 0..400 {
                while feed.slots() >= 2 {
                    let _ = feed.push(6000);
                    let _ = feed.push(6000);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let got = drain_at_least(&mut consumer, FRAME_INTERLEAVED * 8);
        assert!(
            got.len() >= FRAME_INTERLEAVED * 8,
            "only {} samples arrived from the virtual source",
            got.len()
        );
        // A linked consumer is exactly what the demand signal is meant to notice.
        assert_eq!(
            src.demand(),
            Demand::Wanted,
            "capturing from the node must show up as demand"
        );
        let peak = got.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        assert!(
            peak > 3000,
            "the signal arrived at level {peak}, expected ~6000"
        );

        mic.stop();
        src.stop();
        feeder.join().unwrap();
    }

    #[test]
    fn a_mono_source_arrives_as_two_identical_channels() {
        if !have_pipewire() {
            eprintln!("no PipeWire daemon; skipping");
            return;
        }
        // The risk the spec flags: most microphones are mono, and the wire format is
        // stereo. The upmix is the graph's job, and this is the test that says so.
        let (mut feed, src_ring) = rtrb::RingBuffer::<i16>::new(FRAME_SAMPLES * 64);
        let mut src = mono_test_source();
        src.start(src_ring).expect("mono source");

        let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 64);
        let mut mic = PipewireMic::new(Some("pheme-mono-test".into()));
        mic.start(producer).expect("microphone capture");

        let feeder = std::thread::spawn(move || {
            for _ in 0..400 {
                while feed.slots() >= 1 {
                    let _ = feed.push(5000);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let got = drain_at_least(&mut consumer, FRAME_INTERLEAVED * 8);
        assert!(got.len() >= FRAME_INTERLEAVED * 8);
        // Skip the first frames while the graph is still ramping.
        let body = &got[FRAME_INTERLEAVED * 2..];
        let peak = body.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        assert!(peak > 2500, "the mono signal arrived at level {peak}");
        let mismatched = body.chunks_exact(CHANNELS).filter(|p| p[0] != p[1]).count();
        assert_eq!(
            mismatched, 0,
            "a mono source must reach both wire channels identically"
        );

        mic.stop();
        src.stop();
        feeder.join().unwrap();
    }
}
