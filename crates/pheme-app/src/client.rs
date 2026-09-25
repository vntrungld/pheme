//! Client runtime: QUIC peer → core → injection, with automatic reconnect.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use pheme_audio::frame::Frame;
use pheme_core::{ClientCore, InjectAction};
use pheme_input::InputInject;
use pheme_net::pairing::client_pair;
use pheme_net::{Endpoint, Identity, NetError, Peer, TrustStore};
use pheme_proto::{AudioParams, AudioStream, Msg, Os, PROTOCOL_VERSION};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::audio::{CaptureSource, InStats, OutCounters, PlaybackSource, RecvSide, SendSide};
use crate::backoff::Backoff;
use crate::clipboard::ClipboardService;
use crate::config::{config_dir, Config};
use crate::target::Target;

pub struct ClientDeps {
    pub name: String,
    pub inject: Box<dyn InputInject>,
    pub endpoint: Endpoint,
    /// Where the server is, as a question rather than an answer.
    ///
    /// Resolved on every connection attempt, not once at startup: a server that
    /// took a new DHCP lease or restarted on another port is then reachable
    /// again within one backoff interval instead of needing the client
    /// restarted. §4.3.
    pub target: Target,
    pub stats: bool,
    /// Where the client's outgoing audio comes from.
    pub audio: CaptureSource,
    /// Counters the packer thread publishes. `None` allocates a private set, which is
    /// what production does; a test passes its own so it can assert that silence
    /// suppression really stops the traffic rather than merely sending quiet frames.
    pub audio_counters: Option<Arc<OutCounters>>,
    /// Where audio received from the server's microphone is played: the client's virtual
    /// microphone.
    pub mic: PlaybackSource,
    /// Counters the mic worker publishes. `None` allocates a private set.
    pub mic_stats: Option<Arc<InStats>>,
    /// The clipboard worker, or `None` where no clipboard is reachable.
    pub clipboard: Option<ClipboardService>,
}

/// The mic frame in `m`, if it is one this client should play.
///
/// A client receives `AudioStream::Mic` and sends `AudioStream::Playback`; a frame tagged
/// the other way is not ours and is dropped rather than routed into the mic buffer.
fn mic_frame(m: &Msg) -> Option<Frame> {
    match m {
        Msg::Audio {
            stream: AudioStream::Mic,
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

/// The two audio sides `session` needs, bundled so the function stays under the
/// argument-count lint rather than growing an eighth positional parameter.
struct SessionAudio<'a> {
    audio: &'a SendSide,
    counters: &'a OutCounters,
    mic: &'a RecvSide,
    mic_stats: &'a InStats,
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

/// Datagram loss accounting from `seq` gaps: given the highest `seq` seen so far and a
/// newly received one, returns how many datagrams were skipped and the new high-water
/// mark. A late or duplicate datagram (`seq` at or behind the mark, modulo wraparound)
/// counts as no loss and leaves the mark alone; a wrap from `u32::MAX` to `0` is a gap
/// of 0.
fn count_gap(last: Option<u32>, seq: u32) -> (u64, Option<u32>) {
    let Some(last) = last else {
        return (0, Some(seq));
    };
    let ahead = seq.wrapping_sub(last);
    if ahead == 0 || ahead > u32::MAX / 2 {
        (0, Some(last))
    } else {
        (u64::from(ahead - 1), Some(seq))
    }
}

/// The `seq` of an input message. The server numbers every input message (control and
/// datagram) from one counter per direction, so loss must be tracked across all of them:
/// tracking datagrams alone would report each interleaved `Key`/`Button` as a lost
/// datagram. Control messages cannot be lost, so a gap that survives reordering is a
/// lost datagram. `None` for handshake/keepalive and audio (its own per-stream numbering).
fn input_seq(m: &Msg) -> Option<u32> {
    match m {
        Msg::Key { seq, .. }
        | Msg::Button { seq, .. }
        | Msg::Enter { seq, .. }
        | Msg::Leave { seq, .. }
        | Msg::MouseMove { seq, .. }
        | Msg::MouseAbs { seq, .. }
        | Msg::Wheel { seq, .. } => Some(*seq),
        _ => None,
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
        target,
        stats,
        audio,
        audio_counters,
        mic,
        mic_stats,
        clipboard,
    } = deps;
    let counters = audio_counters.unwrap_or_default();
    // The client's speaker capture is never gated: it starts open.
    let mut audio = SendSide::spawn(audio, AudioStream::Playback, counters.clone(), true);
    let mic_stats = mic_stats.unwrap_or_default();
    let mut mic = RecvSide::spawn(mic, mic_stats.clone());
    let mut backoff = Backoff::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        match target.resolve().await {
            Ok(server_addr) => {
                let connect = tokio::select! {
                    r = endpoint.connect(server_addr) => r,
                    _ = shutdown.changed() => break,
                };
                match connect {
                    Ok(peer) => {
                        let started = Instant::now();
                        match session(
                            peer,
                            &name,
                            inject.as_mut(),
                            stats,
                            SessionAudio {
                                audio: &audio,
                                counters: &counters,
                                mic: &mic,
                                mic_stats: &mic_stats,
                            },
                            clipboard.clone(),
                            &mut shutdown,
                        )
                        .await
                        {
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
            }
            // The target may simply not be up yet (a fresh DHCP lease, an mDNS
            // name whose owner hasn't announced yet): this waits out the same
            // backoff as a failed connection, below, rather than being fatal.
            Err(e) => debug!("could not resolve {target:?}: {e}"),
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
    audio.stop();
    mic.stop();
    endpoint.close();
    Ok(())
}

async fn session(
    mut peer: Peer,
    name: &str,
    inject: &mut dyn InputInject,
    stats: bool,
    audio: SessionAudio<'_>,
    clipboard: Option<ClipboardService>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let SessionAudio {
        audio,
        counters,
        mic,
        mic_stats,
    } = audio;
    let screens = inject.screens();
    let mut rx = peer.take_incoming();
    let mut audio_rx = peer.take_audio();
    let mut clip_rx = peer.take_clipboard();
    let mut mic_wanted = mic.wanted();
    let sender = peer.sender();
    sender
        .send_control(&Msg::Hello {
            version: PROTOCOL_VERSION,
            name: name.to_string(),
            os: Os::current(),
            screens: screens.clone(),
            audio: AudioParams::DEFAULT,
        })
        .await?;
    let ack = tokio::select! {
        _ = shutdown.changed() => {
            peer.close("client shutting down");
            return Ok(());
        }
        r = tokio::time::timeout(Duration::from_secs(5), rx.recv()) => {
            r.context("waiting for HelloAck")?
        }
    };
    match ack {
        Some(Msg::HelloAck {
            version,
            name: server_name,
            audio: audio_params,
        }) if version == PROTOCOL_VERSION => {
            info!(server = %server_name, addr = %peer.remote_addr(), "connected");
            if audio_params == AudioParams::DEFAULT {
                audio.set_peer(Some(peer.sender()));
            } else {
                error!(
                    ?audio_params,
                    "the server wants an audio format pheme does not speak; \
                     running this session without audio"
                );
            }
        }
        Some(Msg::Bye { reason }) => bail!("server refused: {reason}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    // The server starts every session with its microphone closed, so without this the
    // first demand is never sent and the microphone never opens.
    let wanted = *mic_wanted.borrow_and_update();
    if wanted {
        mic.reset();
    }
    let _ = sender.send_control(&Msg::MicWanted { wanted }).await;

    let mut core = ClientCore::new(screens);
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    let mut ping_seq = 0u64;
    let mut received = 0u64;
    let mut lost = 0u64;
    // Shadow of the peer's monotonic count of audio frames the transport dropped, so the
    // stats line can report the change since the last one rather than a lifetime total.
    let mut last_mic_channel_dropped = 0u64;
    let mut last_seq: Option<u32> = None;
    let result = loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                let _ = sender.send_control(&Msg::Bye { reason: "client shutting down".into() }).await;
                break Ok(());
            }
            msg = rx.recv() => match msg {
                Some(Msg::Ping(n)) => { let _ = sender.send_control(&Msg::Pong(n)).await; }
                Some(Msg::Pong(_)) => {}
                Some(Msg::Bye { reason }) => {
                    // The pointer is going back to the server (or the server is
                    // gone), so the client's clipboard goes with it (§3.1).
                    if let Some(c) = &clipboard {
                        c.send_to(sender.clone());
                    }
                    for a in core.on_msg(&Msg::Bye { reason: reason.clone() }) { apply(inject, a); }
                    break Ok(());
                }
                Some(m) => {
                    // The pointer is going back to the server, so the client's
                    // clipboard goes with it (§3.1).
                    if matches!(m, Msg::Leave { .. }) {
                        if let Some(c) = &clipboard {
                            c.send_to(sender.clone());
                        }
                    }
                    received += 1;
                    if let Some(seq) = input_seq(&m) {
                        let (gap, next) = count_gap(last_seq, seq);
                        lost += gap;
                        last_seq = next;
                    }
                    for a in core.on_msg(&m) { apply(inject, a); }
                }
                None => break Ok(()),
            },
            m = audio_rx.recv() => match m {
                Some(m) => {
                    if let Some(f) = mic_frame(&m) {
                        mic.push(f);
                    }
                }
                None => break Ok(()),
            },
            m = clip_rx.recv() => match m {
                Some(m) => {
                    if let Some(c) = &clipboard {
                        c.apply(&m);
                    }
                }
                None => break Ok(()),
            },
            // `Ok(())`, not `_`: a dropped sender makes `changed()` return `Err`
            // immediately and for ever, and a `_` pattern would leave this arm
            // permanently ready. Since the arm sits ahead of the ping and the stats tick
            // in a `biased` select, that starves both and burns a core, with no symptom
            // but heat. `RecvSide` now holds its sender open so this cannot happen; the
            // refutable pattern makes `select!` disable the branch rather than spin if
            // some future path ever drops one again.
            Ok(()) = mic_wanted.changed() => {
                let wanted = *mic_wanted.borrow_and_update();
                if wanted {
                    // Reset *before* asking, not after the audio starts arriving. While
                    // the server's microphone was shut its numbering stood still and this
                    // buffer's read cursor kept advancing, so the resuming stream arrives
                    // behind the cursor; if that distance is under RESET_GAP nothing
                    // resets on its own and every frame is discarded as late. The client
                    // knows when it is asking, so it says so.
                    mic.reset();
                }
                let _ = sender.send_control(&Msg::MicWanted { wanted }).await;
            }
            _ = ping.tick() => {
                ping_seq += 1;
                let _ = sender.send_control(&Msg::Ping(ping_seq)).await;
            }
            _ = stats_tick.tick(), if stats => {
                let sent = counters.sent.swap(0, Ordering::Relaxed);
                let suppressed = counters.suppressed.swap(0, Ordering::Relaxed);
                let m = mic_stats.snapshot_delta();
                let total_mic_channel_dropped = peer.audio_dropped();
                let mic_channel_dropped =
                    total_mic_channel_dropped.saturating_sub(last_mic_channel_dropped);
                last_mic_channel_dropped = total_mic_channel_dropped;
                info!(
                    rtt_us = peer.rtt().as_micros(),
                    received,
                    lost,
                    audio_sent = sent,
                    audio_suppressed = suppressed,
                    mic_depth_ms = mic_stats.depth_ms.load(Ordering::Relaxed),
                    mic_lost = m.lost,
                    mic_underruns = m.underruns,
                    mic_late = m.late,
                    mic_resets = m.resets,
                    mic_dropped = m.dropped,
                    // Its own field, not folded into `mic_dropped`: a frame the
                    // transport drops never reaches the jitter buffer, so no counter
                    // beside it can account for the gap the listener hears. Spec §2.5.
                    // Differenced like the rest of the line, which is per second.
                    mic_channel_dropped,
                    mic_overflows = m.overflows,
                    active = core.active(),
                    "stats/s"
                );
                received = 0;
                lost = 0;
            }
        }
    };
    for a in core.on_disconnect() {
        apply(inject, a);
    }
    audio.set_peer(None);
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
    let target = cfg.connect_target(host)?;
    let endpoint = Endpoint::client(&identity, trust)?;
    let inject = pheme_input::detect_inject().context("input injection backend")?;
    info!(name = %cfg.name, target = ?target, fingerprint = %identity.fingerprint, "pheme client");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
        let _ = shutdown_tx.send(true);
    });
    let clipboard = ClipboardService::spawn(pheme_clip::open);
    run_client(
        ClientDeps {
            name: cfg.name.clone(),
            inject,
            endpoint,
            target,
            stats,
            audio: CaptureSource::Detect(cfg.audio.capture_device.clone()),
            audio_counters: None,
            mic: PlaybackSource::DetectVirtualMic(None),
            mic_stats: None,
            clipboard,
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
    let addr = cfg.connect_target(Some(host))?.resolve().await?;
    let endpoint = Endpoint::pairing_client(&identity, trust.clone())?;
    let server_name = client_pair(&endpoint, addr, code.trim(), &identity, trust).await?;
    println!("Paired with {server_name} at {addr}. You can now run `pheme client {host}`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{count_gap, input_seq, mic_frame};
    use pheme_proto::{AudioStream, Button, KeyCode, Modifiers, Msg};

    #[test]
    fn first_datagram_starts_tracking_without_loss() {
        assert_eq!(count_gap(None, 7), (0, Some(7)));
    }

    #[test]
    fn consecutive_sequence_numbers_lose_nothing() {
        assert_eq!(count_gap(Some(7), 8), (0, Some(8)));
    }

    #[test]
    fn a_gap_counts_the_missing_datagrams() {
        assert_eq!(count_gap(Some(5), 9), (3, Some(9)));
    }

    #[test]
    fn reordered_or_duplicate_datagrams_are_ignored() {
        assert_eq!(count_gap(Some(9), 7), (0, Some(9)), "late arrival");
        assert_eq!(count_gap(Some(9), 9), (0, Some(9)), "duplicate");
    }

    #[test]
    fn every_input_message_feeds_the_sequence_tracker() {
        let seqd = [
            Msg::Key {
                seq: 1,
                code: KeyCode(0x04),
                down: true,
            },
            Msg::Button {
                seq: 2,
                btn: Button::Left,
                down: true,
            },
            Msg::Enter {
                seq: 3,
                x: 0,
                y: 0,
                mods: Modifiers(0),
            },
            Msg::Leave { seq: 4 },
            Msg::MouseMove {
                seq: 5,
                dx: 0,
                dy: 0,
            },
            Msg::MouseAbs { seq: 6, x: 0, y: 0 },
            Msg::Wheel {
                seq: 7,
                dx: 0,
                dy: 0,
            },
        ];
        for (i, m) in seqd.iter().enumerate() {
            assert_eq!(input_seq(m), Some(i as u32 + 1), "{m:?}");
        }
        assert_eq!(input_seq(&Msg::Ping(1)), None);
        assert_eq!(
            input_seq(&Msg::Bye {
                reason: String::new()
            }),
            None
        );
    }

    #[test]
    fn wraparound_is_tolerated_as_no_loss() {
        assert_eq!(count_gap(Some(u32::MAX), 0), (0, Some(0)));
        assert_eq!(
            count_gap(Some(u32::MAX - 1), 1),
            (2, Some(1)),
            "gap across the wrap"
        );
    }

    #[test]
    fn a_mic_frame_is_for_the_client_and_a_playback_frame_is_not() {
        // Review Focus 3. Each side owns one direction. A frame tagged for the other one
        // is a confused or hostile peer, and must be ignored rather than fed into the
        // buffer for the direction this side does own - which would splice unrelated
        // audio into a live recording.
        let mic = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        };
        let playback = Msg::Audio {
            stream: AudioStream::Playback,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 960],
        };
        assert!(mic_frame(&mic).is_some());
        assert!(
            mic_frame(&playback).is_none(),
            "a client must ignore the direction it sends rather than receives"
        );
        assert!(mic_frame(&Msg::Ping(1)).is_none());
    }

    #[test]
    fn a_malformed_mic_frame_is_rejected_before_it_reaches_the_buffer() {
        let short = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 1,
            ts_us: 0,
            samples: vec![0; 10],
        };
        // The jitter buffer counts and drops these too, but rejecting here keeps a peer
        // from spending the audio channel on garbage.
        assert!(mic_frame(&short).is_none());
    }
}
