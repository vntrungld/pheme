//! quinn endpoints and peers with length-prefixed control frames and raw datagrams.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pheme_proto::{decode, encode, Msg, CLIP_FRAME_SLACK, MAX_CLIP_BYTES};
use quinn::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use quinn::{Connection, RecvStream, SendStream};
use rustls_pki_types::CertificateDer;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::identity::{fingerprint, Identity};
use crate::trust::SharedTrust;
use crate::verifier::PinnedVerifier;
use crate::{NetError, Result, ALPN_MAIN, ALPN_PAIR};

const CONTROL_BUFFER: usize = 256;
/// Audio frames the receiver may queue before dropping them.
///
/// Small on purpose. `JitterBuffer` downstream has a ceiling of 24 frames, so a deeper
/// queue here can only add latency that the buffer will then discard; and a full channel
/// dropping a frame is a loss the jitter buffer conceals, whereas a full *shared* channel
/// used to mean a mouse event waiting behind 1.28 s of audio.
const AUDIO_BUFFER: usize = 32;
/// Clipboard messages queued for the application. Shallow on purpose: the
/// clipboard is exchanged when the pointer crosses, so more than a couple in
/// flight means something is wrong, and dropping the oldest is correct — only
/// the newest clipboard matters.
const CLIP_BUFFER: usize = 4;
/// How long `accept()` waits for a connected, trusted peer to open its control stream before
/// giving up on it and moving on to the next connection.
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

pub mod framing {
    use super::*;

    /// Writes `len(u16 LE) || bytes`.
    pub async fn write_raw(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
        let len = u16::try_from(bytes.len())
            .map_err(|_| NetError::Connection("frame too large".into()))?;
        let mut frame = Vec::with_capacity(2 + bytes.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(bytes);
        send.write_all(&frame)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))
    }

    /// Reads one frame into `buf`; `Ok(None)` when the stream finished cleanly.
    pub async fn read_raw<'a>(
        recv: &mut RecvStream,
        buf: &'a mut Vec<u8>,
    ) -> Result<Option<&'a [u8]>> {
        let mut len = [0u8; 2];
        match recv.read_exact(&mut len).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
            Err(e) => return Err(NetError::Connection(e.to_string())),
        }
        let n = u16::from_le_bytes(len) as usize;
        buf.resize(n, 0);
        recv.read_exact(buf)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        Ok(Some(&buf[..]))
    }

    pub async fn write_frame(send: &mut SendStream, m: &Msg, scratch: &mut Vec<u8>) -> Result<()> {
        encode(m, scratch);
        write_raw(send, scratch).await
    }

    pub async fn read_frame(recv: &mut RecvStream, buf: &mut Vec<u8>) -> Result<Option<Msg>> {
        match read_raw(recv, buf).await? {
            Some(bytes) => Ok(Some(decode(bytes)?)),
            None => Ok(None),
        }
    }
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(5)).expect("valid timeout"),
    ));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.initial_rtt(Duration::from_millis(1));
    // `Connection::send_datagram` queues with `drop = true`: once this buffer is full it
    // silently evicts the *oldest* unsent datagram to make room for the new one, with no
    // error returned to the caller (see quinn-proto's `Datagrams::send`). Each queued entry
    // costs `size_of::<Datagram>()` (~32 bytes, dominated by `Bytes`'s header) plus its
    // payload, so the previous 16 KiB budget held only ~390 of our ~9-12 byte `MouseMove`
    // datagrams. A legitimate high-rate burst (e.g. a fast mouse swipe, or this crate's own
    // integration test pushing 10 000 relative moves back to back) produces them faster than
    // the connection driver task can flush the socket, so most of the burst was silently
    // dropped even though every call to `send_datagram` reported success. 1 MiB comfortably
    // covers a burst an order of magnitude larger than that (~440 KiB) without materially
    // increasing per-connection memory use.
    t.datagram_send_buffer_size(1024 * 1024);
    // Mirrors the send-side budget: under backpressure from a slow consumer (e.g. the
    // integration test's mock inject path briefly losing the scheduler to CPU contention),
    // `DatagramState::received` evicts the *oldest* buffered-but-undelivered datagram once
    // this window is exceeded (quinn-proto, "dropping stale datagram"), which is just as
    // silent to the application as the send-side eviction above.
    t.datagram_receive_buffer_size(Some(1024 * 1024));
    t.max_concurrent_bidi_streams(4u32.into());
    // Quinn's default is 100 concurrent uni streams, and each accepted stream
    // spawns a task that buffers up to `MAX_CLIP_BYTES + CLIP_FRAME_SLACK`
    // bytes before it is decoded or dropped (see `accept_uni` below). Left at
    // the default, one authenticated peer could hold ~100 MiB and 100 tasks
    // on the runtime that also carries input. The clipboard only ever needs
    // one stream open at a time -- a new crossing opens a new one after the
    // last one finished -- so this matches the bidi cap above and bounds
    // what a peer can make this side hold.
    t.max_concurrent_uni_streams(4u32.into());
    Arc::new(t)
}

fn tls_err(e: impl std::fmt::Display) -> NetError {
    NetError::Tls(e.to_string())
}

fn conn_err(e: impl std::fmt::Display) -> NetError {
    NetError::Connection(e.to_string())
}

#[derive(Debug)]
pub enum CloseReason {
    LocallyClosed,
    ApplicationClosed(String),
    TimedOut,
    Reset,
    Other(String),
}

impl From<quinn::ConnectionError> for CloseReason {
    fn from(e: quinn::ConnectionError) -> Self {
        use quinn::ConnectionError as E;
        match e {
            E::LocallyClosed => CloseReason::LocallyClosed,
            E::ApplicationClosed(c) => {
                CloseReason::ApplicationClosed(String::from_utf8_lossy(&c.reason).into_owned())
            }
            E::TimedOut => CloseReason::TimedOut,
            E::Reset => CloseReason::Reset,
            other => CloseReason::Other(other.to_string()),
        }
    }
}

pub struct Endpoint {
    inner: quinn::Endpoint,
    trust: SharedTrust,
    pairing: Arc<AtomicBool>,
    is_server: bool,
}

pub enum Incoming {
    Peer(Peer),
    Pairing {
        conn: Connection,
        fingerprint: String,
    },
}

impl Endpoint {
    pub fn server(listen: SocketAddr, id: &Identity, trust: SharedTrust) -> Result<Endpoint> {
        let pairing = Arc::new(AtomicBool::new(false));
        let verifier = PinnedVerifier::new(trust.clone(), pairing.clone());
        let provider = verifier.provider();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls_err)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![id.cert.clone()], id.clone_key())
            .map_err(tls_err)?;
        crypto.alpn_protocols = vec![ALPN_MAIN.to_vec(), ALPN_PAIR.to_vec()];
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).map_err(tls_err)?,
        ));
        cfg.transport_config(transport_config());
        let inner = quinn::Endpoint::server(cfg, listen)?;
        Ok(Endpoint {
            inner,
            trust,
            pairing,
            is_server: true,
        })
    }

    fn client_with(
        id: &Identity,
        trust: SharedTrust,
        accept_any: bool,
        alpn: &[u8],
    ) -> Result<Endpoint> {
        let pairing = Arc::new(AtomicBool::new(accept_any));
        let verifier = PinnedVerifier::new(trust.clone(), pairing.clone());
        let provider = verifier.provider();
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls_err)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![id.cert.clone()], id.clone_key())
            .map_err(tls_err)?;
        crypto.alpn_protocols = vec![alpn.to_vec()];
        let mut cfg = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto).map_err(tls_err)?,
        ));
        cfg.transport_config(transport_config());
        // v1 is IPv4-only on the client side (Windows binds IPv6 sockets v6-only by default,
        // which would break IPv4 targets on a dual-stack wildcard).
        let mut inner = quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
        inner.set_default_client_config(cfg);
        Ok(Endpoint {
            inner,
            trust,
            pairing,
            is_server: false,
        })
    }

    /// Main client endpoint: only trusted servers are accepted.
    pub fn client(id: &Identity, trust: SharedTrust) -> Result<Endpoint> {
        Self::client_with(id, trust, false, ALPN_MAIN)
    }

    /// Pairing client endpoint: any server certificate is accepted; pairing verifies it.
    pub fn pairing_client(id: &Identity, trust: SharedTrust) -> Result<Endpoint> {
        Self::client_with(id, trust, true, ALPN_PAIR)
    }

    pub fn set_pairing(&self, on: bool) {
        self.pairing.store(on, Ordering::SeqCst);
    }

    pub fn pairing(&self) -> bool {
        self.pairing.load(Ordering::SeqCst)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    pub fn close(&self) {
        self.inner.close(0u32.into(), b"shutdown");
    }

    /// Waits until all connections are drained so the UDP port can be rebound.
    pub async fn wait_idle(&self) {
        self.inner.wait_idle().await;
    }

    /// Accepts the next connection. Untrusted peers on the main ALPN are closed and skipped.
    ///
    /// QUIC streams are invisible to the peer until data is written on them, so a connected,
    /// trusted peer that never opens its control stream (or opens one but never writes to it)
    /// would otherwise wedge this loop for every other connection; `accept()` gives such a peer
    /// a fixed grace period (currently 5 seconds) to send its first control frame (its `Hello`)
    /// before closing the connection and moving on. Callers of [`Endpoint::connect`] must send
    /// that first control frame promptly after connecting for the same reason.
    pub async fn accept(&self) -> Result<Incoming> {
        debug_assert!(self.is_server);
        loop {
            let incoming = self
                .inner
                .accept()
                .await
                .ok_or_else(|| NetError::Connection("endpoint closed".into()))?;
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    debug!("handshake failed: {e}");
                    continue;
                }
            };
            let fp = peer_fingerprint(&conn)?;
            let alpn = alpn_of(&conn);
            if alpn.as_deref() == Some(ALPN_PAIR) {
                if !self.pairing() {
                    conn.close(1u32.into(), b"pairing disabled");
                    continue;
                }
                return Ok(Incoming::Pairing {
                    conn,
                    fingerprint: fp,
                });
            }
            let name = self.trust.read().unwrap().name_of(&fp);
            let Some(name) = name else {
                warn!(%fp, "rejecting untrusted peer");
                conn.close(1u32.into(), b"untrusted");
                continue;
            };
            let (send, recv) =
                match tokio::time::timeout(CONTROL_STREAM_TIMEOUT, conn.accept_bi()).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        debug!("no control stream: {e}");
                        continue;
                    }
                    Err(_) => {
                        conn.close(1u32.into(), b"no control stream");
                        continue;
                    }
                };
            return Ok(Incoming::Peer(Peer::new(conn, name, fp, send, recv)));
        }
    }

    /// Connects to a trusted server and opens the control stream.
    ///
    /// QUIC streams are invisible to the peer until data is written on them: the returned
    /// `Peer` must have its first control frame (its `Hello`) sent promptly, or the server's
    /// `accept()` will time out waiting for it and close the connection.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Peer> {
        let (conn, fp) = self.connect_raw(addr).await?;
        let name = self
            .trust
            .read()
            .unwrap()
            .name_of(&fp)
            .ok_or_else(|| NetError::Untrusted(fp.clone()))?;
        let (send, recv) = conn.open_bi().await.map_err(conn_err)?;
        Ok(Peer::new(conn, name, fp, send, recv))
    }

    /// Connects and returns the raw connection plus the server's fingerprint (used by pairing).
    pub async fn connect_raw(&self, addr: SocketAddr) -> Result<(Connection, String)> {
        if addr.is_ipv6() {
            return Err(NetError::Connection(
                "IPv6 targets are not supported in v1".into(),
            ));
        }
        let conn = self
            .inner
            .connect(addr, "pheme")
            .map_err(conn_err)?
            .await
            .map_err(|e| match e {
                // `PinnedVerifier::check` rejects an untrusted certificate with
                // `rustls::CertificateError::ApplicationVerificationFailure`, which rustls
                // carries over the wire as a TLS `access_denied` alert (code 0x31 / 49, per
                // `impl From<CertificateError> for AlertDescription` and RFC 8446 §6.2 — *not*
                // `bad_certificate` (0x2a / 42), which rustls reserves for malformed/expired/
                // untrusted-CA certificates). quinn-proto surfaces the alert as a QUIC
                // `TransportErrorCode::crypto(alert)`. Empirically confirmed via
                // `untrusted_server_is_rejected_by_client`.
                quinn::ConnectionError::TransportError(ref te)
                    if te.code == quinn::TransportErrorCode::crypto(0x31) =>
                {
                    NetError::Untrusted("server certificate is not in the trust store".into())
                }
                other => conn_err(other),
            })?;
        let fp = peer_fingerprint(&conn)?;
        Ok((conn, fp))
    }
}

fn peer_fingerprint(conn: &Connection) -> Result<String> {
    let certs = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or_else(|| NetError::Tls("peer presented no certificate".into()))?;
    certs
        .first()
        .map(fingerprint)
        .ok_or_else(|| NetError::Tls("empty certificate chain".into()))
}

fn alpn_of(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()
        .and_then(|d| d.downcast::<HandshakeData>().ok())
        .and_then(|d| d.protocol)
}

#[derive(Debug, Clone)]
pub struct PeerSender {
    conn: Connection,
    send: Arc<Mutex<SendStream>>,
}

impl PeerSender {
    pub async fn send_control(&self, m: &Msg) -> Result<()> {
        let mut scratch = Vec::with_capacity(64);
        let mut send = self.send.lock().await;
        framing::write_frame(&mut send, m, &mut scratch).await
    }

    /// Fire-and-forget; failures are logged at debug level.
    pub fn send_datagram(&self, m: &Msg) {
        let mut buf = Vec::with_capacity(32);
        encode(m, &mut buf);
        if let Err(e) = self.conn.send_datagram(buf.into()) {
            debug!("datagram dropped: {e}");
        }
    }

    /// Sends one clipboard message on a unidirectional stream of its own.
    ///
    /// Never the control stream. A one-megabyte payload written there would hold
    /// every `Key` and `Button` behind it until the transfer finished, because
    /// QUIC delivers one stream in order — head-of-line blocking on the path
    /// this project optimises before all others.
    ///
    /// The stream boundary is the message boundary, so nothing is
    /// length-prefixed: a stream that carries exactly one message needs no
    /// framing of its own.
    ///
    /// `async` rather than self-spawning because its caller is the clipboard
    /// worker, an ordinary OS thread with no reactor; that caller spawns this
    /// onto the runtime, so nothing on the input path ever waits for it.
    pub async fn send_clipboard(&self, m: &Msg) -> Result<()> {
        let mut send = self.conn.open_uni().await.map_err(conn_err)?;
        let mut buf = Vec::with_capacity(256);
        encode(m, &mut buf);
        send.write_all(&buf)
            .await
            .map_err(|e| NetError::Connection(e.to_string()))?;
        send.finish()
            .map_err(|e| NetError::Connection(e.to_string()))?;
        Ok(())
    }
}

/// A connected, authenticated peer. Dropping a `Peer` closes its underlying QUIC connection
/// (see the `Drop` impl below) — otherwise its reader tasks (and the connection itself) could
/// outlive the `Peer` indefinitely under mutual keep-alives, since neither side's idle timeout
/// would ever fire.
#[derive(Debug)]
pub struct Peer {
    conn: Connection,
    name: String,
    fingerprint: String,
    sender: PeerSender,
    incoming: Option<mpsc::Receiver<Msg>>,
    audio: Option<mpsc::Receiver<Msg>>,
    clipboard: Option<mpsc::Receiver<Msg>>,
    audio_dropped: Arc<AtomicU64>,
}

impl Peer {
    fn new(
        conn: Connection,
        name: String,
        fingerprint: String,
        send: SendStream,
        mut recv: RecvStream,
    ) -> Peer {
        let (tx, rx) = mpsc::channel(CONTROL_BUFFER);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_BUFFER);
        let control_tx = tx.clone();
        let control_audio_tx = audio_tx.clone();
        // A frame dropped here is the one audio loss nothing downstream can attribute:
        // the jitter buffer never sees it, so its own `dropped` and `overflows` read
        // clean while the listener hears a gap. Spec §2.5 requires it to be counted.
        let audio_dropped = Arc::new(AtomicU64::new(0));
        let control_audio_dropped = audio_dropped.clone();
        let dgram_audio_dropped = audio_dropped.clone();
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(256);
            loop {
                match framing::read_frame(&mut recv, &mut buf).await {
                    // Audio can also arrive on the control stream if a peer misbehaves, or a
                    // future version routes it there; keep it out of the input channel either
                    // way, so `take_incoming()` never yields a `Msg::Audio`.
                    Ok(Some(m @ Msg::Audio { .. })) => {
                        if control_audio_tx.try_send(m).is_err() {
                            control_audio_dropped.fetch_add(1, Ordering::Relaxed);
                            debug!("audio channel full; dropping a frame");
                        }
                    }
                    Ok(Some(m)) => {
                        if control_tx.send(m).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        debug!("control stream ended: {e}");
                        break;
                    }
                }
            }
        });
        let dgram_conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(bytes) = dgram_conn.read_datagram().await {
                match decode(&bytes) {
                    Ok(m @ Msg::Audio { .. }) => {
                        // Never block the datagram reader on a slow audio consumer:
                        // a dropped frame is concealed, a stalled reader delays input.
                        if audio_tx.try_send(m).is_err() {
                            dgram_audio_dropped.fetch_add(1, Ordering::Relaxed);
                            debug!("audio channel full; dropping a frame");
                        }
                    }
                    Ok(m) => {
                        if tx.send(m).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => debug!("bad datagram: {e}"),
                }
            }
        });
        let (clip_tx, clip_rx) = mpsc::channel(CLIP_BUFFER);
        let clip_conn = conn.clone();
        tokio::spawn(async move {
            loop {
                let mut recv = match clip_conn.accept_uni().await {
                    Ok(r) => r,
                    Err(e) => {
                        debug!("unidirectional stream reader ended: {e}");
                        break;
                    }
                };
                let tx = clip_tx.clone();
                // Each stream is read in its own task so one oversized or slow
                // sender cannot hold up the stream behind it.
                tokio::spawn(async move {
                    let limit = MAX_CLIP_BYTES + CLIP_FRAME_SLACK;
                    let bytes = match recv.read_to_end(limit).await {
                        Ok(b) => b,
                        Err(e) => {
                            debug!("clipboard stream refused: {e}");
                            return;
                        }
                    };
                    match decode(&bytes) {
                        Ok(m @ Msg::Clipboard { .. }) => {
                            // Dropping the oldest is right here: only the newest
                            // clipboard is worth having.
                            if tx.try_send(m).is_err() {
                                debug!("clipboard channel full; dropping a message");
                            }
                        }
                        Ok(other) => {
                            debug!("a unidirectional stream carried {other:?}, not a clipboard")
                        }
                        Err(e) => debug!("undecodable clipboard message: {e}"),
                    }
                });
            }
        });
        let sender = PeerSender {
            conn: conn.clone(),
            send: Arc::new(Mutex::new(send)),
        };
        Peer {
            conn,
            name,
            fingerprint,
            sender,
            incoming: Some(rx),
            audio: Some(audio_rx),
            clipboard: Some(clip_rx),
            audio_dropped,
        }
    }

    pub fn remote_name(&self) -> &str {
        &self.name
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn sender(&self) -> PeerSender {
        self.sender.clone()
    }

    /// Takes the control and non-audio datagram receiver. Panics if called twice.
    pub fn take_incoming(&mut self) -> mpsc::Receiver<Msg> {
        self.incoming
            .take()
            .expect("incoming receiver already taken")
    }

    /// Takes the receiver carrying `Msg::Audio` and nothing else. Panics if called twice.
    ///
    /// Audio is deliberately not merged with the input channel: they share a connection
    /// but not a deadline. Input must never wait behind audio.
    pub fn take_audio(&mut self) -> mpsc::Receiver<Msg> {
        self.audio.take().expect("audio receiver already taken")
    }

    /// Takes the receiver carrying `Msg::Clipboard` and nothing else. Panics if
    /// called twice.
    pub fn take_clipboard(&mut self) -> mpsc::Receiver<Msg> {
        self.clipboard
            .take()
            .expect("clipboard receiver already taken")
    }

    /// Audio frames dropped because this peer's audio channel was full, cumulative for
    /// the life of the peer.
    ///
    /// `AUDIO_BUFFER` is deliberately shallower than the jitter buffer downstream, so an
    /// overflow here is real loss that no later counter can see. Spec §2.5.
    pub fn audio_dropped(&self) -> u64 {
        self.audio_dropped.load(Ordering::Relaxed)
    }

    /// The same counter as a handle, for readers that outlive a borrow of the `Peer` —
    /// the server's stats task reads it through its `Link`.
    pub fn audio_dropped_counter(&self) -> Arc<AtomicU64> {
        self.audio_dropped.clone()
    }

    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    pub async fn closed(&self) -> CloseReason {
        self.conn.closed().await.into()
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }
}

impl Drop for Peer {
    /// Closing an already-closed `quinn::Connection` is a no-op, so an earlier explicit
    /// `close(reason)` is unaffected; this only matters for `Peer`s that were simply dropped.
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"peer dropped");
    }
}
