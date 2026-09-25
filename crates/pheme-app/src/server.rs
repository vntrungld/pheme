//! Server runtime: capture thread → core router → QUIC peer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use pheme_audio::frame::Frame;
use pheme_core::{Action, Active, CaptureEvent, ClientPlacement, Hotkeys, Layout, ServerCore};
use pheme_input::{CaptureEdge, CaptureMode, InputCapture};
use pheme_net::pairing::{generate_code, run_server_pairing};
use pheme_net::{Endpoint, Identity, Incoming, Peer, PeerSender, TrustStore};
use pheme_proto::{AudioParams, AudioStream, Msg, PROTOCOL_VERSION};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::audio::{CaptureSource, InStats, OutCounters, PlaybackSource, RecvSide, SendSide};
use crate::clipboard::ClipboardService;
use crate::config::{config_dir, Config};

/// How long shutdown waits for the router thread to notice that every `CaptureEvent`
/// sender has been dropped.
///
/// Comfortably more than `PortalCapture::stop()`'s own 3 s bound, so a backend that
/// shuts down slowly but correctly is still joined normally; a backend that detaches a
/// thread still holding a sender is abandoned instead of hanging the process.
const ROUTER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the shutdown path asks whether the router thread has finished. Short
/// enough that a normal shutdown is not visibly delayed, long enough that the wait
/// costs a few dozen wake-ups rather than a spinning core.
const ROUTER_JOIN_POLL: Duration = Duration::from_millis(20);

pub struct ServerDeps {
    pub name: String,
    pub capture: Box<dyn InputCapture>,
    pub endpoint: Endpoint,
    pub placements: Vec<ClientPlacement>,
    pub hotkeys: Hotkeys,
    /// The raw `hotkeys.lock` string from the configuration, before `Config::hotkeys()`
    /// converts it to a `KeyCode`. Used only on Wayland, to bind the lock hotkey
    /// through the GlobalShortcuts portal -- the portal wants a trigger, not a
    /// `KeyCode`, and in its own syntax rather than pheme's key-table names, which
    /// `portal::shortcuts::portal_trigger` translates. The compositor may still bind
    /// something else entirely. `None` when no lock hotkey is configured, or on
    /// X11/Windows where the existing key-watching path already reaches
    /// `ServerCore::toggle_lock()`.
    pub lock_hotkey_trigger: Option<String>,
    pub stats: bool,
    /// Where audio received from the client is played.
    pub audio: PlaybackSource,
    /// Counters the playback worker publishes. `None` allocates a private set, which is
    /// what production does; a test passes its own so it can assert on loss, lateness
    /// and buffer depth, which is the only way to tell a working audio path from one
    /// that discards most of what arrives and still sounds roughly right.
    pub audio_stats: Option<Arc<InStats>>,
    /// Where the audio sent to the client's virtual microphone comes from.
    pub mic: CaptureSource,
    /// Counters the mic packer thread publishes. `None` allocates a private set.
    pub mic_counters: Option<Arc<OutCounters>>,
    /// The clipboard worker, or `None` where no clipboard is reachable — GNOME
    /// Wayland, or a headless session. `None` disables clipboard sharing and
    /// nothing else.
    pub clipboard: Option<ClipboardService>,
}

/// The currently connected client, as seen by the router thread.
#[derive(Clone)]
struct Link {
    name: String,
    sender: PeerSender,
    control: mpsc::UnboundedSender<Msg>,
    /// The peer's own count of audio frames dropped because its audio channel was full.
    /// Carried here because the stats task outlives no `Peer` borrow of its own.
    audio_dropped: Arc<AtomicU64>,
}

#[derive(Default)]
struct Counters {
    control_sent: AtomicU64,
    datagrams_sent: AtomicU64,
    events: AtomicU64,
}

struct Shared {
    core: Mutex<ServerCore>,
    capture: Mutex<Box<dyn InputCapture>>,
    link: Mutex<Option<Link>>,
    counters: Counters,
    audio: RecvSide,
    mic: SendSide,
    clipboard: Option<ClipboardService>,
    /// Set when an edge set could not be pushed to the backend because the core was
    /// remote, and cleared by the push that finally happens on the way back to local.
    /// Always locked *inside* `core`, never the other way round, so the "is the core
    /// local?" test and the decision it leads to are one atomic step.
    edges_deferred: Mutex<bool>,
}

impl Shared {
    /// Executes a list of actions in order, then pushes any edge set that was deferred
    /// while the core was remote.
    ///
    /// A failed `Grab` aborts the switch: the core is reset to Local, its recovery
    /// actions run instead, and the rest of the list (`WarpCursor{centre}`,
    /// `SendControl(Enter)`) is dropped so the client never hears of a switch that did
    /// not happen. A failed `Ungrab` is logged and the list continues;
    /// `InputCapture::release`'s contract guarantees the pointer is still warped back
    /// even when the underlying ungrab failed.
    fn execute(&self, actions: Vec<Action>) {
        self.run_actions(actions);
        // Every return to local passes through here. The core has exactly four ways
        // back — `client_disconnected`, `release_remote`, `abort_switch` and the
        // leaving branch of `on_remote_event` — each answers the transition with a
        // non-empty action list, and every one of those lists is executed here
        // (`abort_switch`'s through the recursive call below). A deferred withdrawal
        // therefore cannot be missed on any of them, which a guard placed on one path
        // would not guarantee.
        self.publish_deferred_edges();
    }

    fn run_actions(&self, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        let link = self.link.lock().unwrap().clone();
        for a in actions {
            match a {
                Action::Grab => {
                    let r = self.capture.lock().unwrap().set_mode(CaptureMode::Grab);
                    if let Err(e) = r {
                        error!("grab failed; aborting switch: {e}");
                        let recovery = self.core.lock().unwrap().abort_switch();
                        self.execute(recovery);
                        return;
                    }
                }
                Action::SendControl(m) => {
                    if let Some(l) = &link {
                        // The clipboard crosses with the pointer (§3.1). Reading
                        // it happens on the clipboard thread and the send happens
                        // on the runtime, so this call returns at once and the
                        // handover below is not delayed by either.
                        if matches!(m, Msg::Enter { .. }) {
                            if let Some(c) = &self.clipboard {
                                c.send_to(l.sender.clone());
                            }
                        }
                        let _ = l.control.send(m);
                        self.counters.control_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Action::SendDatagram(m) => {
                    if let Some(l) = &link {
                        l.sender.send_datagram(&m);
                        self.counters.datagrams_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Action::Ungrab { x, y } => self.capture_call(|c| c.release(x, y)),
                Action::WarpCursor { x, y } => self.capture_call(|c| c.warp_cursor(x, y)),
                Action::SetLocked(locked) => {
                    info!(locked, "input lock toggled");
                    // Spec §7: locking removes the barriers, and unlocking puts them
                    // back. `capture_edges()` already answers with nothing while
                    // locked; without this call nothing would ever ask it again, and
                    // on Wayland the compositor would go on capturing at an edge the
                    // core is now guaranteed to decline.
                    //
                    // While the core is remote this is *deferred* rather than done —
                    // see `sync_edges`. The whole point of §7 is a lock taken on the
                    // server screen that could otherwise never be undone; a lock taken
                    // while the input is on the client is not that, and withdrawing
                    // the barriers there would `Disable()` a capture the compositor is
                    // actively running.
                    self.publish_edges();
                }
            }
        }
    }

    fn capture_call(&self, f: impl FnOnce(&mut dyn InputCapture) -> pheme_input::Result<()>) {
        if let Err(e) = f(self.capture.lock().unwrap().as_mut()) {
            error!("capture backend error: {e}");
        }
    }

    /// Pushes the current edge set to the capture backend. Called whenever the set of
    /// connected clients changes, and whenever the lock changes: barriers are declared
    /// only for edges that lead somewhere (see `ServerCore::capture_edges`).
    ///
    /// Deferred to the return to local if the core is remote right now.
    ///
    /// Must never be called while a `core` lock guard is still held: the lock is taken
    /// inside just long enough to read the edge set, then dropped before the capture
    /// backend (a separate lock) is called into.
    fn publish_edges(&self) {
        self.sync_edges(false);
    }

    /// The other half of `publish_edges`: pushes the edge set only if a push was
    /// deferred while the core was remote, and only once it is local again. Called at
    /// the end of every `execute`.
    fn publish_deferred_edges(&self) {
        self.sync_edges(true);
    }

    /// Pushes the current edge set, unless the core is remote.
    ///
    /// The exclusion is the point. Declaring or withdrawing barriers is not a passive
    /// bookkeeping call on every backend: under the InputCapture portal, an empty set
    /// means `SetPointerBarriers([])` followed by `Disable()`, and the specification
    /// lets the compositor answer a `Disable` during an active capture by ending it.
    /// That would surface as `Deactivated` → `CaptureEnded` → `release_remote()`, so a
    /// lock pressed while the pointer is on the client would dump the user back on the
    /// server screen — where the core's own documented semantics (`ServerCore`'s
    /// `lock_blocks_switching_and_leaving`: "only the datagram; no Leave while locked")
    /// are to stay exactly where they are, as X11 and Windows do.
    ///
    /// Nothing is lost by waiting. A barrier only matters while local, because only a
    /// local pointer can reach one; the deferred push happens on the way back, so a
    /// lock that outlives the return still withdraws the barriers then.
    ///
    /// `only_deferred` is what distinguishes the end-of-`execute` call, which must do
    /// nothing unless a push is actually outstanding, from a caller asking for a push
    /// now.
    fn sync_edges(&self, only_deferred: bool) {
        let edges: Vec<CaptureEdge> = {
            // `core` first, then `edges_deferred`, always in this order (the only place
            // the two are held together). Taking both is what makes the state test and
            // the flag update atomic: with two independent locks, a push deferred by
            // one thread could land just after another thread had already checked the
            // flag on its way back to local, and stay deferred until the next batch.
            let core = self.core.lock().unwrap();
            let mut deferred = self.edges_deferred.lock().unwrap();
            if only_deferred && !*deferred {
                return;
            }
            if !matches!(core.active(), Active::Local) {
                *deferred = true;
                return;
            }
            *deferred = false;
            core.capture_edges()
                .into_iter()
                .map(|(side, span)| CaptureEdge { side, span })
                .collect()
        };
        self.capture_call(|c| c.set_edges(&edges));
    }
}

/// Binds the lock hotkey through `org.freedesktop.portal.GlobalShortcuts` when this
/// is a Wayland session and a lock hotkey is configured. `None` otherwise --
/// including every failure inside the bind, which only logs a `warn!`: a lock
/// hotkey that could not be bound must never take keyboard and mouse sharing down
/// with it.
///
/// The returned value must be kept alive for as long as the server runs; dropping
/// it unbinds the shortcut and stops its thread.
#[cfg(target_os = "linux")]
fn bind_lock_shortcut(
    configured: Option<String>,
    shared: &Arc<Shared>,
) -> Option<pheme_input::portal::shortcuts::LockShortcut> {
    if !pheme_input::is_wayland_session() {
        // X11 already reaches `toggle_lock()` through the key-watching path in
        // `on_event`; binding the portal shortcut too would double-toggle.
        return None;
    }
    let configured = configured?;
    let (toggle_tx, toggle_rx) = crossbeam_channel::bounded(4);
    // The raw configuration value: `bind` translates it into the portal's trigger
    // syntax, because the two name their keys differently.
    let shortcut = pheme_input::portal::shortcuts::LockShortcut::bind(configured, toggle_tx);
    let toggle_shared = shared.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("pheme-lock-toggle".into())
        .spawn(move || {
            while toggle_rx.recv().is_ok() {
                let actions = toggle_shared.core.lock().unwrap().toggle_lock();
                toggle_shared.execute(actions);
            }
        })
    {
        warn!("could not spawn the lock-toggle thread; the lock hotkey is unavailable: {e}");
        // Drops `shortcut`, which unbinds it and joins its thread -- nothing must be
        // left sending toggles that nothing will ever receive.
        return None;
    }
    Some(shortcut)
}

/// Wayland, and therefore the GlobalShortcuts portal, exists only on Linux. The
/// key-watching path already handles the lock hotkey on every other platform.
#[cfg(not(target_os = "linux"))]
fn bind_lock_shortcut(_configured: Option<String>, _shared: &Arc<Shared>) -> Option<()> {
    None
}

pub async fn run_server(
    deps: ServerDeps,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let ServerDeps {
        name,
        mut capture,
        endpoint,
        placements,
        hotkeys,
        lock_hotkey_trigger,
        stats,
        audio,
        audio_stats,
        mic,
        mic_counters,
        clipboard,
    } = deps;
    let audio_stats = audio_stats.unwrap_or_default();
    let audio_in = RecvSide::spawn(audio, audio_stats.clone());
    let mic_counters = mic_counters.unwrap_or_default();
    // Closed until a client says something is recording. A server with no client has no
    // consumer, so there is nothing for an open microphone to be open for. This is the
    // spawn's initial state, not a correction applied after: a store made once the
    // thread is already running is not guaranteed to be seen before its first read.
    let mic = SendSide::spawn(mic, AudioStream::Mic, mic_counters.clone(), false);
    let (ev_tx, ev_rx) = crossbeam_channel::bounded::<CaptureEvent>(4096);
    capture.start(ev_tx).context("starting input capture")?;
    let screens = capture.screens();
    info!(?screens, "server screens");
    let core = ServerCore::new(
        Layout {
            server_screens: screens,
            clients: placements,
        },
        hotkeys,
    );
    let shared = Arc::new(Shared {
        core: Mutex::new(core),
        capture: Mutex::new(capture),
        link: Mutex::new(None),
        counters: Counters::default(),
        audio: audio_in,
        mic,
        clipboard,
        edges_deferred: Mutex::new(false),
    });

    // Binds the lock hotkey through the GlobalShortcuts portal on Wayland, where no
    // key event ever reaches the router thread below. `None` on X11 and Windows,
    // where the existing key-watching path already reaches `toggle_lock()` through
    // `on_event` -- a second mechanism there would double-toggle. Held for the life
    // of the server: dropping it unbinds the shortcut.
    let _lock_shortcut = bind_lock_shortcut(lock_hotkey_trigger, &shared);

    // Router thread: blocking receive from the capture backend, no async hop for datagrams.
    // It owns `router_alive`; dropping it when the loop ends (the backend closed the event
    // channel, i.e. its thread died) is the signal the async side selects on below.
    let (router_alive, mut router_dead) = watch::channel(());
    let router_shared = shared.clone();
    let router = std::thread::Builder::new()
        .name("pheme-router".into())
        .spawn(move || {
            while let Ok(ev) = ev_rx.recv() {
                router_shared
                    .counters
                    .events
                    .fetch_add(1, Ordering::Relaxed);
                let actions = router_shared.core.lock().unwrap().on_event(ev);
                router_shared.execute(actions);
            }
            drop(router_alive);
        })
        .context("spawning router thread")?;

    if stats {
        let s = shared.clone();
        let astats = audio_stats.clone();
        let mic_counters = mic_counters.clone();
        let mut stats_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut last = (0u64, 0u64, 0u64);
            // Shadow of the peer's monotonic drop counter, so this line reports the
            // change since the last one like everything else on it. A reconnect installs
            // a fresh counter that starts at zero, which the saturating subtraction turns
            // into a delta of zero rather than an underflow.
            let mut last_audio_channel_dropped = 0u64;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stats_shutdown.changed() => break,
                }
                let now = (
                    s.counters.events.load(Ordering::Relaxed),
                    s.counters.control_sent.load(Ordering::Relaxed),
                    s.counters.datagrams_sent.load(Ordering::Relaxed),
                );
                // A frame the transport dropped is loss the jitter buffer never sees,
                // so it gets its own field rather than being folded into `audio_dropped`
                // — but it is differenced like every other figure here, because the line
                // is labelled per second and a lifetime total beside a delta reads as a
                // rate it is not.
                let (connected, total_audio_channel_dropped) = match s.link.lock().unwrap().as_ref()
                {
                    Some(l) => (true, l.audio_dropped.load(Ordering::Relaxed)),
                    None => (false, 0),
                };
                let audio_channel_dropped =
                    total_audio_channel_dropped.saturating_sub(last_audio_channel_dropped);
                last_audio_channel_dropped = total_audio_channel_dropped;
                let a = astats.snapshot_delta();
                let mic_sent = mic_counters.sent.swap(0, Ordering::Relaxed);
                let mic_suppressed = mic_counters.suppressed.swap(0, Ordering::Relaxed);
                let mic_open = s.mic.is_open();
                info!(
                    events = now.0 - last.0,
                    control = now.1 - last.1,
                    datagrams = now.2 - last.2,
                    connected,
                    audio_depth_ms = astats.depth_ms.load(Ordering::Relaxed),
                    audio_lost = a.lost,
                    audio_underruns = a.underruns,
                    audio_late = a.late,
                    audio_resets = a.resets,
                    audio_dropped = a.dropped,
                    audio_channel_dropped,
                    audio_overflows = a.overflows,
                    mic_sent,
                    mic_suppressed,
                    mic_open,
                    "stats/s"
                );
                last = now;
            }
        });
    }

    // `Ok(())` if the accept loop ended by request, `Err` if the router thread exited
    // on its own (capture backend gone) before shutdown was requested.
    let mut outcome = Ok(());
    loop {
        let incoming = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            // Nothing is ever sent on this watch, so `changed()` only completes (with an
            // error) once the router thread has dropped its sender.
            _ = router_dead.changed() => {
                error!("router thread exited: input capture backend stopped");
                outcome = Err(anyhow!("input capture backend stopped unexpectedly"));
                break;
            }
            r = endpoint.accept() => r,
        };
        match incoming {
            Ok(Incoming::Peer(peer)) => {
                let session = handle_peer(
                    peer,
                    &name,
                    shared.clone(),
                    shutdown.clone(),
                    router_dead.clone(),
                );
                if let Err(e) = session.await {
                    warn!("peer session ended with error: {e}");
                }
            }
            Ok(Incoming::Pairing { conn, .. }) => conn.close(1u32.into(), b"not pairing"),
            Err(e) => {
                error!("accept failed: {e}");
                break;
            }
        }
    }

    // Shutdown: release any grab, stop the capture and the router thread.
    let actions = {
        let mut core = shared.core.lock().unwrap();
        let active = core.active();
        match active {
            Active::Remote(n) => core.client_disconnected(&n),
            Active::Local => Vec::new(),
        }
    };
    shared.execute(actions);
    // `InputCapture::stop` must stop the backend thread and drop every clone of the event
    // `Sender` before returning (see the trait contract in pheme-input::InputCapture); only
    // then does the router thread's `ev_rx.recv()` observe disconnection and return `Err`,
    // ending its loop below.
    shared.capture.lock().unwrap().stop();
    endpoint.close();
    endpoint.wait_idle().await;
    drop(shared);
    // Wait for the router thread to end, but never block on it: a backend that violates
    // the stop() contract would hang `router.join()` forever, because the router sits in
    // `ev_rx.recv()` until the last `Sender` clone is dropped. That is not hypothetical
    // — `PortalCapture::stop()` gives up after 3 s and *detaches* its session thread,
    // which still owns its `Sender`.
    //
    // The wait is a poll of `is_finished()` rather than a bounded `spawn_blocking(||
    // router.join())`, which would not actually let the process leave: tokio's
    // documentation is explicit that "blocking functions spawned through
    // `Runtime::spawn_blocking` keep running until they return... The `Drop`
    // implementation waits forever for this". Abandoning that task only moves the hang
    // from here into the runtime's drop, after `main` has returned — the same hang, with
    // a warning in front of it. A `std::thread` left unjoined has no such property, so
    // dropping this handle really does detach it, exactly as `pheme_audio::device`
    // detaches a capture thread it could not stop.
    let joined = tokio::time::timeout(ROUTER_JOIN_TIMEOUT, async {
        // Polled, not busy-waited: a spin here would burn a core for the whole timeout
        // on precisely the shutdown path that is already going wrong.
        while !router.is_finished() {
            tokio::time::sleep(ROUTER_JOIN_POLL).await;
        }
    })
    .await
    .is_ok();
    if joined {
        // Finished: this returns immediately.
        let _ = router.join();
    } else {
        warn!(
            "the router thread did not exit within {ROUTER_JOIN_TIMEOUT:?}; the capture \
             backend still holds an event sender. Detaching the thread rather than \
             blocking shutdown on it"
        );
        drop(router);
    }
    outcome
}

/// The playback frame in `m`, if it is one this server should play.
///
/// A server receives `AudioStream::Playback` and sends `AudioStream::Mic`; a frame tagged
/// the other way is not ours.
fn playback_frame(m: &Msg) -> Option<Frame> {
    match m {
        Msg::Audio {
            stream: AudioStream::Playback,
            seq,
            ts_us,
            samples,
        } if samples.len() == pheme_audio::FRAME_BYTES => Some(Frame {
            seq: *seq,
            ts_us: *ts_us,
            bytes: samples.clone(),
        }),
        _ => None,
    }
}

/// Runs one client session to completion (disconnect or shutdown).
async fn handle_peer(
    mut peer: Peer,
    server_name: &str,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    mut router_dead: watch::Receiver<()>,
) -> anyhow::Result<()> {
    let mut rx = peer.take_incoming();
    let mut audio_rx = peer.take_audio();
    let mut clip_rx = peer.take_clipboard();
    let hello = tokio::select! {
        _ = shutdown.changed() => {
            peer.close("server shutting down");
            return Ok(());
        }
        r = tokio::time::timeout(Duration::from_secs(5), rx.recv()) => {
            r.context("waiting for Hello")?
        }
    };
    let Some(Msg::Hello {
        version,
        name,
        os,
        screens,
        audio,
    }) = hello
    else {
        peer.close("expected Hello");
        bail!("first message was not Hello");
    };
    if version != PROTOCOL_VERSION {
        peer.sender()
            .send_control(&Msg::Bye {
                reason: format!("protocol {version} != {PROTOCOL_VERSION}"),
            })
            .await?;
        peer.close("version mismatch");
        bail!("client {name} uses protocol {version}");
    }
    if name != peer.remote_name() {
        warn!(hello = %name, trusted = %peer.remote_name(), "client name differs from paired name; using paired name");
    }
    let name = peer.remote_name().to_string();
    peer.sender()
        .send_control(&Msg::HelloAck {
            version: PROTOCOL_VERSION,
            name: server_name.to_string(),
            audio: AudioParams::DEFAULT,
        })
        .await?;
    info!(client = %name, ?os, addr = %peer.remote_addr(), "client connected");

    let audio_ok = audio == AudioParams::DEFAULT;
    if !audio_ok {
        error!(
            ?audio,
            client = %name,
            "the client speaks an audio format pheme does not; running this session \
             without audio in either direction"
        );
    }
    if audio_ok {
        shared.mic.set_peer(Some(peer.sender()));
    }

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Msg>();
    {
        let mut link = shared.link.lock().unwrap();
        if let Some(existing) = link.as_ref() {
            warn!(existing = %existing.name, "replacing existing client link");
        }
        *link = Some(Link {
            name: name.clone(),
            sender: peer.sender(),
            control: control_tx,
            audio_dropped: peer.audio_dropped_counter(),
        });
    }
    let actions = shared.core.lock().unwrap().client_connected(&name, screens);
    shared.execute(actions);
    shared.publish_edges();

    // Control writer: preserves ordering of Key/Button/Enter/Leave.
    let writer_sender = peer.sender();
    let writer = tokio::spawn(async move {
        while let Some(m) = control_rx.recv().await {
            if let Err(e) = writer_sender.send_control(&m).await {
                warn!("control send failed: {e}");
                break;
            }
        }
    });

    let result = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Ping(n)) => { let _ = peer.sender().send_control(&Msg::Pong(n)).await; }
                Some(Msg::Bye { reason }) => { info!(%reason, "client said bye"); break Ok(()); }
                Some(Msg::MicWanted { wanted }) => {
                    if audio_ok {
                        shared.mic.set_wanted(wanted);
                    }
                }
                Some(other) => tracing::debug!(?other, "ignoring message from client"),
                None => break Ok(()),
            },
            m = audio_rx.recv() => match m {
                Some(m) => {
                    if audio_ok {
                        if let Some(f) = playback_frame(&m) {
                            shared.audio.push(f);
                        }
                    }
                }
                None => break Ok(()),
            },
            m = clip_rx.recv() => match m {
                Some(m) => {
                    if let Some(c) = &shared.clipboard {
                        c.apply(&m);
                    }
                }
                None => break Ok(()),
            },
            _ = shutdown.changed() => {
                let _ = peer.sender().send_control(&Msg::Bye { reason: "server shutting down".into() }).await;
                break Ok(());
            }
            // The capture backend died mid-session: end the session so the accept loop
            // can observe the same signal and fail `run_server`.
            _ = router_dead.changed() => {
                let _ = peer.sender().send_control(&Msg::Bye { reason: "server input capture stopped".into() }).await;
                break Err(anyhow!("input capture backend stopped unexpectedly"));
            }
        }
    };

    writer.abort();
    {
        let mut link = shared.link.lock().unwrap();
        if link.as_ref().map(|l| l.name == name).unwrap_or(false) {
            *link = None;
        }
    }
    shared.mic.set_peer(None);
    // No client means no consumer. Clearing the peer alone would leave the device open
    // for the life of the process, with its indicator lit and nothing listening.
    shared.mic.set_wanted(false);
    let actions = shared.core.lock().unwrap().client_disconnected(&name);
    shared.execute(actions);
    shared.publish_edges();
    peer.close("session ended");
    info!(client = %name, "client disconnected");
    result
}

/// Entry point for `pheme server`.
pub async fn main(cfg: Config, pair: bool, stats: bool) -> anyhow::Result<()> {
    let dir = config_dir();
    let identity = Identity::load_or_create(&dir, &cfg.name)?;
    let trust = TrustStore::load(&dir)?.shared();
    let endpoint = Endpoint::server(cfg.listen, &identity, trust.clone())?;
    info!(name = %cfg.name, listen = %cfg.listen, fingerprint = %identity.fingerprint, "pheme server");

    if pair {
        let code = generate_code();
        println!("Pairing code: {code}   (valid for 120 s, run `pheme pair <this-host> {code}` on the client)");
        match run_server_pairing(
            &endpoint,
            &code,
            &identity,
            trust.clone(),
            3,
            Duration::from_secs(120),
        )
        .await
        {
            Ok(name) => println!("Paired with {name}."),
            Err(e) => bail!("pairing failed: {e}"),
        }
    }
    if cfg.clients.is_empty() {
        warn!("no [[clients]] configured; nothing will ever switch screens");
    }

    let capture = pheme_input::detect_capture().context("input capture backend")?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
        let _ = shutdown_tx.send(true);
    });
    let clipboard = ClipboardService::spawn(pheme_clip::open);
    run_server(
        ServerDeps {
            name: cfg.name.clone(),
            capture,
            endpoint,
            placements: cfg.placements()?,
            hotkeys: cfg.hotkeys()?,
            lock_hotkey_trigger: cfg.hotkeys.lock.clone(),
            stats,
            audio: PlaybackSource::Detect(cfg.audio.playback_device.clone()),
            audio_stats: None,
            mic: CaptureSource::Detect(cfg.audio.mic_device.clone()),
            mic_counters: None,
            clipboard,
        },
        shutdown_rx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playback_frame_is_not_for_the_server_to_send() {
        // Review Focus 3, the server's half: it receives Playback and sends Mic.
        assert!(playback_frame(&Msg::Audio {
            stream: AudioStream::Playback,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        })
        .is_some());
        assert!(playback_frame(&Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        })
        .is_none());
    }
}
