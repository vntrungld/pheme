//! Client runtime: QUIC peer → core → injection, with automatic reconnect.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use pheme_core::{ClientCore, InjectAction};
use pheme_input::InputInject;
use pheme_net::pairing::client_pair;
use pheme_net::{Endpoint, Identity, NetError, Peer, TrustStore};
use pheme_proto::{Msg, Os, PROTOCOL_VERSION};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::backoff::Backoff;
use crate::config::{config_dir, Config};

pub struct ClientDeps {
    pub name: String,
    pub inject: Box<dyn InputInject>,
    pub endpoint: Endpoint,
    pub server_addr: SocketAddr,
    pub stats: bool,
}

fn apply(inject: &mut dyn InputInject, a: InjectAction) {
    let r = match a {
        InjectAction::MoveAbs { x, y } => inject.mouse_move_abs(x, y),
        InjectAction::MoveRel { dx, dy } => inject.mouse_move_rel(dx, dy),
        InjectAction::Button { btn, down } => inject.button(btn, down),
        InjectAction::Wheel { dx, dy } => inject.wheel(dx, dy),
        InjectAction::Key { code, down } => inject.key(code, down),
    };
    if let Err(e) = r {
        error!("inject failed: {e}");
    }
}

pub async fn run_client(
    deps: ClientDeps,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let ClientDeps {
        name,
        mut inject,
        endpoint,
        server_addr,
        stats,
    } = deps;
    let mut backoff = Backoff::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let connect = tokio::select! {
            r = endpoint.connect(server_addr) => r,
            _ = shutdown.changed() => break,
        };
        match connect {
            Ok(peer) => {
                let started = Instant::now();
                match session(peer, &name, inject.as_mut(), stats, &mut shutdown).await {
                    Ok(()) => info!("disconnected from server"),
                    Err(e) => warn!("session ended: {e}"),
                }
                backoff.note_connected_for(started.elapsed());
            }
            Err(NetError::Untrusted(reason)) => {
                warn!("connect to {server_addr} rejected: untrusted server ({reason})")
            }
            Err(e) => debug!("connect to {server_addr} failed: {e}"),
        }
        if *shutdown.borrow() {
            break;
        }
        let delay = backoff.next();
        debug!(?delay, "reconnecting");
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => break,
        }
    }
    endpoint.close();
    Ok(())
}

async fn session(
    mut peer: Peer,
    name: &str,
    inject: &mut dyn InputInject,
    stats: bool,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let screens = inject.screens();
    let mut rx = peer.take_incoming();
    let sender = peer.sender();
    sender
        .send_control(&Msg::Hello {
            version: PROTOCOL_VERSION,
            name: name.to_string(),
            os: Os::current(),
            screens: screens.clone(),
        })
        .await?;
    let ack = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .context("waiting for HelloAck")?;
    match ack {
        Some(Msg::HelloAck {
            version,
            name: server_name,
            ..
        }) if version == PROTOCOL_VERSION => {
            info!(server = %server_name, addr = %peer.remote_addr(), "connected");
        }
        Some(Msg::Bye { reason }) => bail!("server refused: {reason}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    let mut core = ClientCore::new(screens);
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let mut ping_seq = 0u64;
    let mut received = 0u64;
    let mut last_stats = Instant::now();
    let result = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Ping(n)) => { let _ = sender.send_control(&Msg::Pong(n)).await; }
                Some(Msg::Pong(_)) => {}
                Some(Msg::Bye { reason }) => {
                    for a in core.on_msg(&Msg::Bye { reason: reason.clone() }) { apply(inject, a); }
                    break Ok(());
                }
                Some(m) => {
                    received += 1;
                    for a in core.on_msg(&m) { apply(inject, a); }
                }
                None => break Ok(()),
            },
            _ = ping.tick() => {
                ping_seq += 1;
                let _ = sender.send_control(&Msg::Ping(ping_seq)).await;
                if stats && last_stats.elapsed() >= Duration::from_secs(1) {
                    info!(rtt_us = peer.rtt().as_micros(), received, active = core.active(), "stats/s");
                    received = 0;
                    last_stats = Instant::now();
                }
            }
            _ = shutdown.changed() => {
                let _ = sender.send_control(&Msg::Bye { reason: "client shutting down".into() }).await;
                break Ok(());
            }
        }
    };
    for a in core.on_disconnect() {
        apply(inject, a);
    }
    peer.close("session ended");
    result
}

/// Entry point for `pheme client`.
pub async fn main(cfg: Config, host: Option<&str>, stats: bool) -> anyhow::Result<()> {
    let dir = config_dir();
    let identity = Identity::load_or_create(&dir, &cfg.name)?;
    let trust = TrustStore::load(&dir)?.shared();
    if trust.read().unwrap().peers().is_empty() {
        bail!("no paired server; run `pheme pair <host> <code>` first");
    }
    let server_addr = cfg.connect_addr(host)?;
    let endpoint = Endpoint::client(&identity, trust)?;
    let inject = pheme_input::detect_inject().context("input injection backend")?;
    info!(name = %cfg.name, server = %server_addr, fingerprint = %identity.fingerprint, "pheme client");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
        let _ = shutdown_tx.send(true);
    });
    run_client(
        ClientDeps {
            name: cfg.name.clone(),
            inject,
            endpoint,
            server_addr,
            stats,
        },
        shutdown_rx,
    )
    .await
}

/// Entry point for `pheme pair`.
pub async fn pair(cfg: Config, host: &str, code: &str) -> anyhow::Result<()> {
    let dir = config_dir();
    let identity = Identity::load_or_create(&dir, &cfg.name)?;
    let trust = TrustStore::load(&dir)?.shared();
    let addr = cfg.connect_addr(Some(host))?;
    let endpoint = Endpoint::pairing_client(&identity, trust.clone())?;
    let server_name = client_pair(&endpoint, addr, code.trim(), &identity, trust).await?;
    println!("Paired with {server_name} at {addr}. You can now run `pheme client {host}`.");
    Ok(())
}
