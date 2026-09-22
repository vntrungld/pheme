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
use pw::stream::StreamFlags;
use tracing::{debug, warn};

use crate::{AudioCapture, Error, Result, CHANNELS, RATE};

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
    /// Cleared by the PipeWire thread when its main loop returns, which is how a daemon
    /// restart becomes visible to the supervisor in `pheme-app`.
    alive: Arc<AtomicBool>,
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
                capture_thread(sink, ready_tx, cmd_rx);
                thread_alive.store(false, Ordering::SeqCst);
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
                let _ = cmd_tx.send(Cmd::Stop);
                let _ = thread.join();
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
        .state_changed(|_, _, old, new| {
            debug!(?old, ?new, "Pheme Speaker state");
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
