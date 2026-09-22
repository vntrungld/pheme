//! WASAPI loopback capture and shared-mode render.
//!
//! Both directions run a dedicated thread that initialises COM on entry and uninitialises
//! it on exit, and both report readiness through a `std::sync::mpsc` channel so `start`
//! is synchronous.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::{debug, warn};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVE_FORMAT_PCM,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::{AudioCapture, AudioPlayback, Error, Result, CHANNELS, RATE};

/// Device buffer, in 100 ns units. 20 ms absorbs any slip in the 2.5 ms poll.
const BUFFER_100NS: i64 = 200_000;
/// How long `start` waits for its thread to open the device.
const START_TIMEOUT: Duration = Duration::from_secs(2);
/// Loopback cannot be driven by an event, so the capture thread polls.
const POLL: Duration = Duration::from_micros(2_500);
/// How often the capture thread re-reads the default endpoint, to notice a silent switch.
const DEFAULT_RECHECK: Duration = Duration::from_secs(2);

/// Initialises COM for the current thread and uninitialises it on drop.
pub struct ComGuard;

impl ComGuard {
    pub fn new() -> Result<ComGuard> {
        // SAFETY: called once per thread; the matching CoUninitialize is in `drop`.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| Error::Backend(format!("CoInitializeEx: {e}")))?;
        }
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx in `new`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// The parts of a `WAVEFORMATEX` this crate cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatInfo {
    pub rate: u32,
    pub channels: usize,
    /// True for 32-bit float samples, false for 16-bit integer.
    pub float: bool,
}

/// Reads a device mix format, rejecting anything this crate cannot convert.
///
/// # Safety
/// `p` must point at a valid `WAVEFORMATEX`, and at a `WAVEFORMATEXTENSIBLE` when
/// `wFormatTag` says so.
pub unsafe fn parse_format(p: *const WAVEFORMATEX) -> Result<FormatInfo> {
    if p.is_null() {
        return Err(Error::Device("the device reported no mix format".into()));
    }
    let w = &*p;
    const EXTENSIBLE: u16 = 0xFFFE;
    let (bits, float) = if w.wFormatTag == EXTENSIBLE {
        let ext = &*(p as *const WAVEFORMATEXTENSIBLE);
        let subformat = ext.SubFormat;
        // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT and _PCM differ only in their first field.
        let float = subformat.data1 == WAVE_FORMAT_IEEE_FLOAT;
        let pcm = subformat.data1 == WAVE_FORMAT_PCM;
        if !float && !pcm {
            return Err(Error::Device(
                "the device uses a sample format pheme cannot convert".into(),
            ));
        }
        (w.wBitsPerSample, float)
    } else {
        (
            w.wBitsPerSample,
            w.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16,
        )
    };
    match (bits, float) {
        (32, true) | (16, false) => Ok(FormatInfo {
            rate: w.nSamplesPerSec,
            channels: w.nChannels as usize,
            float,
        }),
        _ => Err(Error::Device(format!(
            "unsupported device format: {bits} bits, float={float}"
        ))),
    }
}

/// The friendly name Windows shows for a device, or a placeholder.
pub fn friendly_name(device: &IMMDevice) -> String {
    // SAFETY: `device` is a live COM object; every call below is a plain property read.
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
            return "unknown device".into();
        };
        let Ok(value) = store.GetValue(&PKEY_Device_FriendlyName) else {
            return "unknown device".into();
        };
        value
            .Anonymous
            .Anonymous
            .Anonymous
            .pwszVal
            .to_string()
            .unwrap_or_else(|_| "unknown device".into())
    }
}

/// Opens a render endpoint: the one whose friendly name matches `name`, or the default.
///
/// A name that matches nothing logs a warning and falls back to the default, so a typo
/// in the config costs the user a log line rather than silence.
pub fn open_render_device(name: Option<&str>) -> Result<IMMDevice> {
    // SAFETY: standard MMDevice enumeration; every raw pointer stays inside this block.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| Error::Device(format!("creating the device enumerator: {e}")))?;
        if let Some(wanted) = name {
            let collection = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| Error::Device(format!("enumerating render endpoints: {e}")))?;
            let count = collection
                .GetCount()
                .map_err(|e| Error::Device(format!("counting render endpoints: {e}")))?;
            for i in 0..count {
                if let Ok(dev) = collection.Item(i) {
                    if friendly_name(&dev).eq_ignore_ascii_case(wanted) {
                        return Ok(dev);
                    }
                }
            }
            warn!(device = wanted, "no such audio device; using the default");
        }
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| Error::Device(format!("opening the default render endpoint: {e}")))
    }
}

/// Converts device samples to the wire format: stereo i16 at 48 kHz.
///
/// Extra channels are dropped and a mono device is duplicated, because the wire format
/// is always stereo. A device that does not run at 48 kHz is resampled here so that
/// everything downstream stays at one rate.
struct ToWire {
    fmt: FormatInfo,
    resampler: Option<rubato::SincFixedIn<f32>>,
    planar: Vec<Vec<f32>>,
}

/// Input frames per resampler call. Small enough to keep latency negligible, large
/// enough that the sinc filter is not the dominant cost.
const RESAMPLE_CHUNK: usize = 256;

impl ToWire {
    fn new(fmt: FormatInfo) -> Result<ToWire> {
        let resampler = if fmt.rate == RATE {
            None
        } else {
            let params = rubato::SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.95,
                interpolation: rubato::SincInterpolationType::Cubic,
                oversampling_factor: 128,
                window: rubato::WindowFunction::BlackmanHarris2,
            };
            Some(
                rubato::SincFixedIn::<f32>::new(
                    f64::from(RATE) / f64::from(fmt.rate),
                    1.1,
                    params,
                    RESAMPLE_CHUNK,
                    CHANNELS,
                )
                .map_err(|e| Error::Device(format!("building the capture resampler: {e}")))?,
            )
        };
        Ok(ToWire {
            fmt,
            resampler,
            planar: (0..CHANNELS)
                .map(|_| Vec::with_capacity(RESAMPLE_CHUNK * 2))
                .collect(),
        })
    }

    /// Feeds one WASAPI packet into `sink`. Returns how many samples had to be dropped
    /// because the ring was full.
    ///
    /// # Safety
    /// `data` must point at `frames * channels` samples in `self.fmt`'s layout, or be
    /// null when `silent` is true.
    unsafe fn push(
        &mut self,
        data: *const u8,
        frames: usize,
        silent: bool,
        sink: &mut rtrb::Producer<i16>,
    ) -> u64 {
        let ch = self.fmt.channels.max(1);
        for f in 0..frames {
            for (c, plane) in self.planar.iter_mut().enumerate() {
                let src_ch = if ch == 1 { 0 } else { c.min(ch - 1) };
                let v = if silent || data.is_null() {
                    0.0
                } else if self.fmt.float {
                    let p = (data as *const f32).add(f * ch + src_ch);
                    *p
                } else {
                    let p = (data as *const i16).add(f * ch + src_ch);
                    f32::from(*p) / 32_768.0
                };
                plane.push(v);
            }
        }
        self.flush(sink)
    }

    fn flush(&mut self, sink: &mut rtrb::Producer<i16>) -> u64 {
        let mut dropped = 0u64;
        match self.resampler.as_mut() {
            None => {
                let n = self.planar[0].len();
                for i in 0..n {
                    for plane in &self.planar {
                        dropped += push_sample(sink, plane[i]);
                    }
                }
                for plane in &mut self.planar {
                    plane.clear();
                }
            }
            Some(r) => {
                use rubato::Resampler;
                while self.planar[0].len() >= RESAMPLE_CHUNK {
                    let chunk: Vec<Vec<f32>> = self
                        .planar
                        .iter_mut()
                        .map(|p| p.drain(..RESAMPLE_CHUNK).collect())
                        .collect();
                    match r.process(&chunk, None) {
                        Ok(out) => {
                            let n = out.first().map(|c| c.len()).unwrap_or(0);
                            for i in 0..n {
                                for plane in &out {
                                    dropped += push_sample(sink, plane[i]);
                                }
                            }
                        }
                        Err(e) => warn!("capture resampling failed: {e}"),
                    }
                }
            }
        }
        dropped
    }
}

fn push_sample(sink: &mut rtrb::Producer<i16>, v: f32) -> u64 {
    let s = (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
    u64::from(sink.push(s).is_err())
}

/// What `WasapiCapture` and `WasapiPlayback` keep once their device thread is up. Split
/// out from the struct itself so a `start` that fails or times out simply never
/// populates it, and `healthy` and `stop` have nothing to do when there is no running
/// thread.
struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// Cleared when the device thread returns, so the supervisor can rebuild.
    alive: Arc<AtomicBool>,
}

/// Records what the default (or configured) output endpoint is playing.
pub struct WasapiCapture {
    device: Option<String>,
    name: Arc<Mutex<String>>,
    running: Option<Running>,
}

impl WasapiCapture {
    pub fn new(device: Option<String>) -> WasapiCapture {
        WasapiCapture {
            device,
            name: Arc::new(Mutex::new("not started".into())),
            running: None,
        }
    }
}

impl AudioCapture for WasapiCapture {
    fn start(&mut self, sink: rtrb::Producer<i16>) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let device = self.device.clone();
        let name = self.name.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pheme-wasapi-cap".into())
            .spawn(move || {
                capture_thread(device, sink, thread_stop, name, ready_tx);
                thread_alive.store(false, Ordering::SeqCst);
            })
            .map_err(|e| Error::Backend(format!("spawning the WASAPI thread: {e}")))?;
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    stop,
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
                // The thread may be stuck inside a blocking WASAPI/COM call (a
                // misbehaving driver never returning from `IAudioClient::Initialize`,
                // say), so joining here could block `start` forever — exactly the
                // unbounded wait its contract forbids. Ask it to stop and then
                // deliberately do not join: dropping the `JoinHandle` detaches the
                // thread, so it runs to completion (or hangs) on its own instead of
                // `start` hanging with it.
                stop.store(true, Ordering::SeqCst);
                warn!(
                    "WASAPI capture thread did not report readiness within 2 s; abandoning it \
                     detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(
                    "the WASAPI capture thread did not open a device within 2 s".into(),
                ))
            }
        }
    }

    fn device_name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    fn healthy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::SeqCst);
            let _ = r.thread.join();
        }
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_thread(
    device: Option<String>,
    mut sink: rtrb::Producer<i16>,
    stop: Arc<AtomicBool>,
    name: Arc<Mutex<String>>,
    ready: mpsc::Sender<Result<()>>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: a process-wide timer resolution request, released before the thread ends.
    unsafe { timeBeginPeriod(1) };
    let mut first = true;
    let mut dropped_total = 0u64;
    while !stop.load(Ordering::SeqCst) {
        match capture_session(
            device.as_deref(),
            &mut sink,
            &stop,
            &name,
            &mut dropped_total,
        ) {
            Ok(()) => {}
            Err(e) => {
                if first {
                    let _ = ready.send(Err(e));
                    // SAFETY: balances the timeBeginPeriod above.
                    unsafe { timeEndPeriod(1) };
                    return;
                }
                warn!("WASAPI capture session ended: {e}; reopening");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if first {
            let _ = ready.send(Ok(()));
            first = false;
        }
    }
    // SAFETY: balances the timeBeginPeriod above.
    unsafe { timeEndPeriod(1) };
    debug!(dropped_total, "WASAPI capture thread finished");
}

/// One device's worth of capture. Returns when the device is invalidated, the default
/// endpoint changes, or `stop` is set.
fn capture_session(
    device: Option<&str>,
    sink: &mut rtrb::Producer<i16>,
    stop: &AtomicBool,
    name: &Mutex<String>,
    dropped_total: &mut u64,
) -> Result<()> {
    // SAFETY: a standard WASAPI loopback session; all raw pointers stay in this block
    // and every buffer obtained with GetBuffer is released before the next call.
    unsafe {
        let dev = open_render_device(device)?;
        *name.lock().unwrap() = friendly_name(&dev);
        let client: IAudioClient = dev
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| Error::Device(format!("activating the audio client: {e}")))?;
        let mix = client
            .GetMixFormat()
            .map_err(|e| Error::Device(format!("reading the mix format: {e}")))?;
        // `mix` is freed exactly once below, after its last use by either call, so that
        // an unsupported format (an `Err` from `parse_format`) does not leak the
        // allocation `GetMixFormat` made. `mix` is the device's own mix format, so
        // `Initialize` accepting it does not depend on whether `parse_format` liked it.
        let fmt = parse_format(mix);
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            BUFFER_100NS,
            0,
            mix,
            None,
        );
        CoTaskMemFree(Some(mix as *const _));
        let fmt = fmt?;
        init.map_err(|e| Error::Device(format!("initialising loopback capture: {e}")))?;
        debug!(?fmt, "WASAPI loopback format");

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| Error::Device(format!("getting the capture service: {e}")))?;
        let mut conv = ToWire::new(fmt)?;
        client
            .Start()
            .map_err(|e| Error::Device(format!("starting loopback capture: {e}")))?;

        let mut last_check = std::time::Instant::now();
        let result = loop {
            if stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            loop {
                let packet = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    // A break here (rather than returning) would leave the outer loop
                    // spinning on the same invalidated `IAudioCaptureClient` forever, so
                    // this instead ends the session and lets `capture_thread` reopen a
                    // fresh device on its next iteration.
                    Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                        return Err(Error::Device("the audio device was invalidated".into()));
                    }
                    Err(e) => return Err(Error::Device(format!("GetNextPacketSize: {e}"))),
                };
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if let Err(e) = capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) {
                    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                        return Err(Error::Device("the audio device was invalidated".into()));
                    }
                    return Err(Error::Device(format!("GetBuffer: {e}")));
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                *dropped_total += conv.push(data, frames as usize, silent, sink);
                let _ = capture.ReleaseBuffer(frames);
            }
            // The user may have changed the default output without invalidating ours.
            if device.is_none() && last_check.elapsed() >= DEFAULT_RECHECK {
                last_check = std::time::Instant::now();
                if let Ok(current) = open_render_device(None) {
                    if friendly_name(&current) != *name.lock().unwrap() {
                        break Err(Error::Device("the default output device changed".into()));
                    }
                }
            }
            std::thread::sleep(POLL);
        };
        let _ = client.Stop();
        result
    }
}

/// Plays the worker's samples on the server's speakers.
///
/// The ring it drains is already at the device's rate — the playback worker resamples —
/// so this backend only converts stereo i16 into the device's sample format and channel
/// count.
pub struct WasapiPlayback {
    device: Option<String>,
    name: Arc<Mutex<String>>,
    rate: Arc<AtomicU32>,
    running: Option<Running>,
}

impl WasapiPlayback {
    pub fn new(device: Option<String>) -> WasapiPlayback {
        WasapiPlayback {
            device,
            name: Arc::new(Mutex::new("not started".into())),
            rate: Arc::new(AtomicU32::new(RATE)),
            running: None,
        }
    }
}

impl AudioPlayback for WasapiPlayback {
    fn start(&mut self, source: rtrb::Consumer<i16>) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let device = self.device.clone();
        let name = self.name.clone();
        let rate = self.rate.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pheme-wasapi-play".into())
            .spawn(move || {
                render_thread(device, source, thread_stop, name, rate, ready_tx);
                thread_alive.store(false, Ordering::SeqCst);
            })
            .map_err(|e| Error::Backend(format!("spawning the WASAPI thread: {e}")))?;
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = Some(Running {
                    stop,
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
                // As in `WasapiCapture::start`: the thread may be stuck inside a
                // blocking WASAPI/COM call, so joining here could block `start`
                // forever. Ask it to stop and detach it instead of joining, and leave
                // `self.running` as `None` so `healthy()` correctly reports false for
                // an abandoned start.
                stop.store(true, Ordering::SeqCst);
                warn!(
                    "WASAPI render thread did not report readiness within 2 s; abandoning it \
                     detached rather than blocking `start` further"
                );
                drop(thread);
                Err(Error::Backend(
                    "the WASAPI render thread did not open a device within 2 s".into(),
                ))
            }
        }
    }

    fn rate(&self) -> u32 {
        self.rate.load(Ordering::SeqCst)
    }

    fn device_name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    fn healthy(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.alive.load(Ordering::SeqCst))
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::SeqCst);
            let _ = r.thread.join();
        }
    }
}

impl Drop for WasapiPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

fn render_thread(
    device: Option<String>,
    mut source: rtrb::Consumer<i16>,
    stop: Arc<AtomicBool>,
    name: Arc<Mutex<String>>,
    rate: Arc<AtomicU32>,
    ready: mpsc::Sender<Result<()>>,
) {
    let _com = match ComGuard::new() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut first = true;
    let mut underruns = 0u64;
    while !stop.load(Ordering::SeqCst) {
        match render_session(
            device.as_deref(),
            &mut source,
            &stop,
            &name,
            &rate,
            &mut underruns,
        ) {
            Ok(()) => {}
            Err(e) => {
                if first {
                    let _ = ready.send(Err(e));
                    return;
                }
                warn!("WASAPI render session ended: {e}; reopening");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if first {
            let _ = ready.send(Ok(()));
            first = false;
        }
    }
    debug!(underruns, "WASAPI render thread finished");
}

fn render_session(
    device: Option<&str>,
    source: &mut rtrb::Consumer<i16>,
    stop: &AtomicBool,
    name: &Mutex<String>,
    rate: &AtomicU32,
    underruns: &mut u64,
) -> Result<()> {
    // SAFETY: a standard shared-mode, event-driven render session. Every GetBuffer is
    // matched by a ReleaseBuffer, and the event handle is closed on every exit path.
    unsafe {
        let dev = open_render_device(device)?;
        *name.lock().unwrap() = friendly_name(&dev);
        let client: IAudioClient = dev
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| Error::Device(format!("activating the audio client: {e}")))?;
        let mix = client
            .GetMixFormat()
            .map_err(|e| Error::Device(format!("reading the mix format: {e}")))?;
        // `mix` is freed exactly once below, after its last use by either call, so that
        // an unsupported format (an `Err` from `parse_format`) does not leak the
        // allocation `GetMixFormat` made. `mix` is the device's own mix format, so
        // `Initialize` accepting it does not depend on whether `parse_format` liked it.
        let fmt = parse_format(mix);
        // 0 asks for the device's default period, which is 10 ms in shared mode. A
        // shorter buffer needs IAudioClient3, which is out of scope.
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            mix,
            None,
        );
        CoTaskMemFree(Some(mix as *const _));
        let fmt = fmt?;
        init.map_err(|e| Error::Device(format!("initialising render: {e}")))?;
        rate.store(fmt.rate, Ordering::SeqCst);
        debug!(?fmt, "WASAPI render format");

        let event: HANDLE = CreateEventW(None, false, false, None)
            .map_err(|e| Error::Device(format!("creating the render event: {e}")))?;
        let result = render_loop(&client, event, source, stop, fmt, underruns);
        let _ = client.Stop();
        // SAFETY: `event` was created by `CreateEventW` just above and is not used
        // again after this call, on every exit path from this function.
        let _ = CloseHandle(event);
        result
    }
}

/// # Safety
/// `client` must be initialised in event-driven shared mode and `event` must be the
/// handle passed to `SetEventHandle`.
unsafe fn render_loop(
    client: &IAudioClient,
    event: HANDLE,
    source: &mut rtrb::Consumer<i16>,
    stop: &AtomicBool,
    fmt: FormatInfo,
    underruns: &mut u64,
) -> Result<()> {
    client
        .SetEventHandle(event)
        .map_err(|e| Error::Device(format!("SetEventHandle: {e}")))?;
    let render: IAudioRenderClient = client
        .GetService()
        .map_err(|e| Error::Device(format!("getting the render service: {e}")))?;
    let buffer_frames = client
        .GetBufferSize()
        .map_err(|e| Error::Device(format!("GetBufferSize: {e}")))?;
    client
        .Start()
        .map_err(|e| Error::Device(format!("starting render: {e}")))?;

    let bytes_per_sample = if fmt.float { 4 } else { 2 };
    let ch = fmt.channels.max(1);
    while !stop.load(Ordering::SeqCst) {
        // A 200 ms wait rather than INFINITE so `stop` is noticed promptly.
        if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
            continue;
        }
        let padding = match client.GetCurrentPadding() {
            Ok(p) => p,
            Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(Error::Device("the render device was invalidated".into()))
            }
            Err(e) => return Err(Error::Device(format!("GetCurrentPadding: {e}"))),
        };
        let frames = buffer_frames.saturating_sub(padding);
        if frames == 0 {
            continue;
        }
        let data = match render.GetBuffer(frames) {
            Ok(p) => p,
            Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(Error::Device("the render device was invalidated".into()))
            }
            Err(e) => return Err(Error::Device(format!("render GetBuffer: {e}"))),
        };
        for f in 0..frames as usize {
            // One stereo pair per device frame; a device with more channels gets
            // silence in the rest, a mono device gets the left channel only.
            let mut pair = [0i16; CHANNELS];
            for p in pair.iter_mut() {
                match source.pop() {
                    Ok(s) => *p = s,
                    Err(_) => *underruns += 1,
                }
            }
            for c in 0..ch {
                let v = pair.get(c).copied().unwrap_or(0);
                let slot = data.add((f * ch + c) * bytes_per_sample);
                if fmt.float {
                    let x = f32::from(v) / 32_768.0;
                    std::ptr::copy_nonoverlapping(x.to_le_bytes().as_ptr(), slot, 4);
                } else {
                    std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), slot, 2);
                }
            }
        }
        let _ = render.ReleaseBuffer(frames, 0);
    }
    Ok(())
}
