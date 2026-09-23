//! Server runtime: capture thread → core router → QUIC peer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use pheme_audio::frame::Frame;
use pheme_core::{Action, CaptureEvent, ClientPlacement, Hotkeys, Layout, ServerCore};
use pheme_input::{CaptureMode, InputCapture};
use pheme_net::pairing::{generate_code, run_server_pairing};
use pheme_net::{Endpoint, Identity, Incoming, Peer, PeerSender, TrustStore};
use pheme_proto::{AudioParams, AudioStream, Msg, PROTOCOL_VERSION};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::audio::{InStats, PlaybackSource, RecvSide};
use crate::config::{config_dir, Config};

pub struct ServerDeps {
    pub name: String,
    pub capture: Box<dyn InputCapture>,
    pub endpoint: Endpoint,
    pub placements: Vec<ClientPlacement>,
    pub hotkeys: Hotkeys,
    pub stats: bool,
    /// Where audio received from the client is played.
    pub audio: PlaybackSource,
    /// Counters the playback worker publishes. `None` allocates a private set, which is
    /// what production does; a test passes its own so it can assert on loss, lateness
    /// and buffer depth, which is the only way to tell a working audio path from one
    /// that discards most of what arrives and still sounds roughly right.
    pub audio_stats: Option<Arc<InStats>>,
}

/// The currently connected client, as seen by the router thread.
#[derive(Clone)]
struct Link {
    name: String,
    sender: PeerSender,
    control: mpsc::UnboundedSender<Msg>,
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
}

impl Shared {
    /// Executes a list of actions in order. A failed `Grab` aborts the switch: the core
    /// is reset to Local, its recovery actions run instead, and the rest of the list
    /// (`WarpCursor{centre}`, `SendControl(Enter)`) is dropped so the client never hears
    /// of a switch that did not happen. A failed `Ungrab` is logged and the list continues
    /// (the pointer is still warped back).
    fn execute(&self, actions: Vec<Action>) {
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
                Action::Ungrab => self.capture_call(|c| c.set_mode(CaptureMode::Observe)),
                Action::WarpCursor { x, y } => self.capture_call(|c| c.warp_cursor(x, y)),
                Action::SetLocked(locked) => info!(locked, "input lock toggled"),
            }
        }
    }

    fn capture_call(&self, f: impl FnOnce(&mut dyn InputCapture) -> pheme_input::Result<()>) {
        if let Err(e) = f(self.capture.lock().unwrap().as_mut()) {
            error!("capture backend error: {e}");
        }
    }
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
        stats,
        audio,
        audio_stats,
    } = deps;
    let audio_stats = audio_stats.unwrap_or_default();
    let audio_in = RecvSide::spawn(audio, audio_stats.clone());
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
    });

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
        let mut stats_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut last = (0u64, 0u64, 0u64);
            let mut alast = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
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
                let connected = s.link.lock().unwrap().is_some();
                let anow = (
                    astats.lost.load(Ordering::Relaxed),
                    astats.underruns.load(Ordering::Relaxed),
                    astats.late.load(Ordering::Relaxed),
                    astats.resets.load(Ordering::Relaxed),
                    astats.dropped.load(Ordering::Relaxed),
                    astats.overflows.load(Ordering::Relaxed),
                );
                info!(
                    events = now.0 - last.0,
                    control = now.1 - last.1,
                    datagrams = now.2 - last.2,
                    connected,
                    audio_depth_ms = astats.depth_ms.load(Ordering::Relaxed),
                    audio_lost = anow.0 - alast.0,
                    audio_underruns = anow.1 - alast.1,
                    audio_late = anow.2 - alast.2,
                    audio_resets = anow.3 - alast.3,
                    audio_dropped = anow.4 - alast.4,
                    audio_overflows = anow.5 - alast.5,
                    "stats/s"
                );
                last = now;
                alast = anow;
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
            pheme_core::Active::Remote(n) => core.client_disconnected(&n),
            pheme_core::Active::Local => Vec::new(),
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
    // Join on a blocking task: a backend that violates the stop() contract (event thread
    // still running, sender clone still alive) would hang `router.join()` forever, and doing
    // that directly here would stall a tokio worker instead of just this shutdown path.
    let _ = tokio::task::spawn_blocking(move || router.join()).await;
    outcome
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
        audio: _,
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
        });
    }
    let actions = shared.core.lock().unwrap().client_connected(&name, screens);
    shared.execute(actions);

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
                Some(other) => tracing::debug!(?other, "ignoring message from client"),
                None => break Ok(()),
            },
            // Temporary: Task 14 rewrites this arm with a stream-tag check and a format
            // gate. For now the `Msg::Audio` handling just moved here unchanged, since
            // `take_incoming()` no longer carries audio at all.
            msg = audio_rx.recv() => match msg {
                Some(Msg::Audio {
                    stream: AudioStream::Playback,
                    seq,
                    ts_us,
                    samples,
                }) => shared.audio.push(Frame {
                    seq,
                    ts_us,
                    bytes: samples,
                }),
                Some(other) => tracing::debug!(?other, "ignoring message from client"),
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
    let actions = shared.core.lock().unwrap().client_disconnected(&name);
    shared.execute(actions);
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
    run_server(
        ServerDeps {
            name: cfg.name.clone(),
            capture,
            endpoint,
            placements: cfg.placements()?,
            hotkeys: cfg.hotkeys()?,
            stats,
            audio: PlaybackSource::Detect(cfg.audio.playback_device.clone()),
            audio_stats: None,
        },
        shutdown_rx,
    )
    .await
}
