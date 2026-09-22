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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamState};
use tracing::{debug, warn};

use crate::{AudioCapture, AudioPlayback, Error, Result, CHANNELS, RATE};

/// How long `start` waits for the PipeWire thread to report success or failure.
const START_TIMEOUT: Duration = Duration::from_secs(1);

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(pw::init);
}

enum Cmd {
    Stop,
}

/// Builds the SPA pod describing our one and only format: S16LE, 48 kHz, stereo.
fn format_pod() -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    // `AudioInfoRaw::new()` leaves the channel-position array unpositioned (all-zero,
    // `AudioInfoRawFlags::UNPOSITIONED`). That is fine for the capture side, whose peer
    // is always another PipeWire stream on the *input* side of the conversion. For
    // playback, `StreamFlags::AUTOCONNECT` links this stream's output straight into the
    // system's real (positioned) hardware sink, and negotiating a stereo mix from an
    // unpositioned source into a positioned sink crashes `libspa-audioconvert.so`
    // (segfault inside the channel-mix code, reproduced with a minimal scratch
    // reproduction and a core dump backtrace through
    // `pw_main_loop_run -> ... -> libspa-audioconvert.so`, with no Rust frames beyond
    // `MainLoop::run`). Declaring the two channels explicitly as front-left/front-right
    // — exactly what CHANNELS = 2 always means in this crate — avoids that code path
    // entirely and matches how the upstream `tone.rs` example builds its format.
    let mut position = [0u32; pw::spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
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

struct Running {
    cmd: pw::channel::Sender<Cmd>,
    thread: JoinHandle<()>,
    /// Cleared by `AliveGuard` when the PipeWire thread's stack is torn down, which is
    /// how a daemon restart becomes visible to the supervisor in `pheme-app`.
    alive: Arc<AtomicBool>,
}

/// Clears a backend's `alive` flag when the PipeWire thread's stack unwinds or returns.
///
/// This is a guard rather than a statement after the call so that a panic inside a
/// PipeWire callback — which unwinds the thread without ever reaching that statement —
/// also flips `healthy()` to false. A thread that has died is a thread that has died,
/// however it died, and the supervisor's rebuild is the only thing that brings audio
/// back either way.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
pub struct PipewireCapture {
    running: Option<Running>,
}

impl Default for PipewireCapture {
    fn default() -> Self {
        PipewireCapture::new()
    }
}

impl PipewireCapture {
    pub fn new() -> PipewireCapture {
        PipewireCapture { running: None }
    }
}

impl AudioCapture for PipewireCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        init();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread = std::thread::Builder::new()
            .name("pheme-pw-sink".into())
            .spawn(move || {
                let _alive = AliveGuard(thread_alive);
                capture_thread(sink, ready_tx, cmd_rx);
            })
            .map_err(|e| Error::Backend(format!("spawning the PipeWire thread: {e}")))?;

        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    cmd: cmd_tx,
                    thread,
                    alive,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // `Cmd::Stop` only becomes observable once the thread reaches
                // `cmd_rx.attach(..)`, which is after the main loop, context, core,
                // stream and listener have all been constructed. If construction itself
                // is what's hanging (rather than merely being slow), that message may
                // never be picked up, so `thread.join()` here could block forever —
                // exactly the unbounded wait `start`'s contract promises not to be. We
                // ask the thread to stop and then deliberately do not join it: dropping
                // the `JoinHandle` detaches it, so it runs to completion (or hangs) on
                // its own instead of `start` hanging with it.
                let _ = cmd_tx.send(Cmd::Stop);
                warn!(
                    "Pheme Speaker thread did not report readiness within 1 s; abandoning it \
                     detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(
                    "the PipeWire thread did not report readiness within 1 s".into(),
                ))
            }
        }
    }

    fn device_name(&self) -> String {
        "Pheme Speaker".into()
    }

    fn healthy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.cmd.send(Cmd::Stop);
            let _ = r.thread.join();
        }
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

fn capture_thread(
    sink: rtrb::Producer<i16>,
    ready: mpsc::Sender<Result<()>>,
    cmd_rx: pw::channel::Receiver<Cmd>,
) {
    if let Err(e) = run(sink, &ready, cmd_rx) {
        warn!("Pheme Speaker could not start: {e}");
        // `run` only returns `Err` before it has sent a readiness reply, so this is the
        // one and only reply in the failure path.
        let _ = ready.send(Err(e));
    }
}

/// Builds the node, runs the main loop until told to stop, and tears the node down.
///
/// On success this sends `Ok(())` on `ready` once the node is connected and then blocks
/// in `mainloop.run()` until a `Cmd::Stop` arrives. On failure it returns `Err` without
/// sending anything, leaving the reply to the caller in `capture_thread`.
fn run(
    sink: rtrb::Producer<i16>,
    ready: &mpsc::Sender<Result<()>>,
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

    // `Direction::Input` plus `media.class = Audio/Sink` is what makes this a sink that
    // other applications can select, rather than a recording stream.
    let stream = pw::stream::StreamBox::new(
        &core,
        "pheme-speaker",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "pheme-speaker",
            *pw::keys::NODE_DESCRIPTION => "Pheme Speaker",
            *pw::keys::AUDIO_RATE => "48000",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => "240/48000",
        },
    )
    .map_err(|e| Error::Device(format!("creating the Pheme Speaker node: {e}")))?;

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
            move |_, _, old, new| {
                debug!(?old, ?new, "Pheme Speaker state");
                if let Some(why) = stream_died(&mut streamed, &new) {
                    warn!("Pheme Speaker is gone: {why}; ending the PipeWire thread");
                    quit_loop.quit();
                }
            }
        })
        .register()
        .map_err(|e| Error::Device(format!("registering the stream listener: {e}")))?;

    let bytes = format_pod()?;
    let mut params = [Pod::from_bytes(&bytes)
        .ok_or_else(|| Error::Backend("the audio format pod is malformed".into()))?];
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| Error::Device(format!("connecting the Pheme Speaker node: {e}")))?;

    let quit_loop = mainloop.clone();
    let _receiver = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        Cmd::Stop => quit_loop.quit(),
    });

    // The node exists and is connected: tell `start` it can return. If the caller has
    // already given up (the 1 s timeout in `start` elapsed), there is nothing to notify
    // and we fall through to `mainloop.run()`, which will exit as soon as the `Cmd::Stop`
    // that `start` sent on timeout is delivered.
    let _ = ready.send(Ok(()));

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// The server's playback stream: samples in, speakers out.
pub struct PipewirePlayback {
    device: Option<String>,
    running: Option<Running>,
}

impl PipewirePlayback {
    /// `device` is matched by PipeWire as `target.object`, i.e. against a node name or
    /// serial. `pactl list sinks short` prints the node names. An unknown value falls
    /// back to the default sink.
    pub fn new(device: Option<String>) -> PipewirePlayback {
        PipewirePlayback {
            device,
            running: None,
        }
    }
}

impl AudioPlayback for PipewirePlayback {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        init();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (cmd_tx, cmd_rx) = pw::channel::channel::<Cmd>();
        let device = self.device.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread = std::thread::Builder::new()
            .name("pheme-pw-play".into())
            .spawn(move || {
                let _alive = AliveGuard(thread_alive);
                playback_thread(source, device, ready_tx, cmd_rx);
            })
            .map_err(|e| Error::Backend(format!("spawning the PipeWire thread: {e}")))?;

        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    cmd: cmd_tx,
                    thread,
                    alive,
                });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // Same reasoning as `PipewireCapture::start`: `Cmd::Stop` only becomes
                // observable once the thread reaches `cmd_rx.attach(..)` inside `run`,
                // which is after the main loop, context, core, stream and listener have
                // all been constructed. If construction itself hangs, joining here could
                // block forever, which would break `start`'s 1 s-bound contract. Ask the
                // thread to stop, then deliberately do not join it: dropping the
                // `JoinHandle` detaches it, so it runs to completion (or hangs) on its
                // own instead of blocking `start`.
                let _ = cmd_tx.send(Cmd::Stop);
                warn!(
                    "Pheme playback thread did not report readiness within 1 s; abandoning it \
                     detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(
                    "the PipeWire thread did not report readiness within 1 s".into(),
                ))
            }
        }
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
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.cmd.send(Cmd::Stop);
            let _ = r.thread.join();
        }
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
    ready: mpsc::Sender<Result<()>>,
    cmd_rx: pw::channel::Receiver<Cmd>,
) {
    if let Err(e) = play_run(source, device, &ready, cmd_rx) {
        warn!("Pheme playback could not start: {e}");
        // `play_run` only returns `Err` before it has sent a readiness reply, so this is
        // the one and only reply in the failure path.
        let _ = ready.send(Err(e));
    }
}

/// Builds the playback node, runs the main loop until told to stop, and tears the node
/// down. Same shape as `run` above: everything that touches a PipeWire object lives as a
/// local in this one function, because the 0.10 ownership model ties the attached
/// channel receiver's lifetime to the `MainLoopRc` local that produced it.
fn play_run(
    source: rtrb::Consumer<i16>,
    device: Option<String>,
    ready: &mpsc::Sender<Result<()>>,
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

    let bytes = format_pod()?;
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
    let _ = ready.send(Ok(()));

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}
