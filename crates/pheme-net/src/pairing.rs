//! One-time pairing: SPAKE2 over a short code, then mutual fingerprint confirmation.

use std::net::SocketAddr;
use std::time::Duration;

use hmac::{Hmac, Mac};
use quinn::Connection;
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use tracing::{info, warn};

use crate::identity::Identity;
use crate::transport::{framing, Endpoint, Incoming};
use crate::trust::SharedTrust;
use crate::{NetError, Result};

const SPAKE_MSG_LEN: usize = 33;
const SPAKE_ID: &[u8] = b"pheme";

pub fn generate_code() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

fn spake_start(code: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
    Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &SpakeId::new(SPAKE_ID),
    )
}

fn confirm(key: &[u8], first_fp: &str, second_fp: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(first_fp.as_bytes());
    mac.update(second_fp.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn split_first_frame(bytes: &[u8]) -> Result<(Vec<u8>, String)> {
    if bytes.len() < SPAKE_MSG_LEN {
        return Err(NetError::Pairing("short SPAKE2 message".into()));
    }
    let (msg, name) = bytes.split_at(SPAKE_MSG_LEN);
    let name = std::str::from_utf8(name)
        .map_err(|_| NetError::Pairing("peer name is not UTF-8".into()))?;
    if name.is_empty() || name.len() > 64 {
        return Err(NetError::Pairing("bad peer name length".into()));
    }
    Ok((msg.to_vec(), name.to_string()))
}

/// Server side of one pairing attempt on an already-accepted pairing connection.
pub async fn server_pair(
    conn: Connection,
    client_fp: String,
    code: &str,
    id: &Identity,
    trust: SharedTrust,
) -> Result<String> {
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| NetError::Pairing(e.to_string()))?;
    let mut buf = Vec::new();
    let first = framing::read_raw(&mut recv, &mut buf)
        .await?
        .ok_or_else(|| NetError::Pairing("client hung up".into()))?;
    let (client_msg, client_name) = split_first_frame(first)?;

    let (state, my_msg) = spake_start(code);
    let mut out = my_msg;
    out.extend_from_slice(id.name.as_bytes());
    framing::write_raw(&mut send, &out).await?;
    let key = state
        .finish(&client_msg)
        .map_err(|_| NetError::Pairing("SPAKE2 finish".into()))?;

    let theirs = framing::read_raw(&mut recv, &mut buf)
        .await?
        .ok_or_else(|| NetError::Pairing("client hung up".into()))?;
    if theirs != confirm(&key, &client_fp, &id.fingerprint).as_slice() {
        conn.close(2u32.into(), b"bad code");
        return Err(NetError::Pairing("wrong code".into()));
    }
    framing::write_raw(&mut send, &confirm(&key, &id.fingerprint, &client_fp)).await?;
    framing::write_raw(&mut send, b"ok").await?;
    let _ = send.finish();

    {
        let mut t = trust.write().unwrap();
        t.add(&client_name, &client_fp);
        t.save()?;
    }
    info!(name = %client_name, fp = %client_fp, "paired client");
    // Give the client time to read before closing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    conn.close(0u32.into(), b"paired");
    Ok(client_name)
}

/// Client side: connects with the pairing ALPN and runs the exchange.
pub async fn client_pair(
    endpoint: &Endpoint,
    addr: SocketAddr,
    code: &str,
    id: &Identity,
    trust: SharedTrust,
) -> Result<String> {
    let (conn, server_fp) = endpoint.connect_raw(addr).await?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| NetError::Pairing(e.to_string()))?;
    let (state, my_msg) = spake_start(code);
    let mut out = my_msg;
    out.extend_from_slice(id.name.as_bytes());
    framing::write_raw(&mut send, &out).await?;

    let mut buf = Vec::new();
    let first = framing::read_raw(&mut recv, &mut buf)
        .await?
        .ok_or_else(|| NetError::Pairing("server refused pairing".into()))?;
    let (server_msg, server_name) = split_first_frame(first)?;
    let key = state
        .finish(&server_msg)
        .map_err(|_| NetError::Pairing("SPAKE2 finish".into()))?;

    framing::write_raw(&mut send, &confirm(&key, &id.fingerprint, &server_fp)).await?;
    let theirs = framing::read_raw(&mut recv, &mut buf)
        .await?
        .ok_or_else(|| NetError::Pairing("wrong code".into()))?;
    if theirs != confirm(&key, &server_fp, &id.fingerprint).as_slice() {
        return Err(NetError::Pairing("server confirmation mismatch".into()));
    }
    let ok = framing::read_raw(&mut recv, &mut buf)
        .await?
        .ok_or_else(|| NetError::Pairing("no final ack".into()))?;
    if ok != b"ok" {
        return Err(NetError::Pairing("unexpected final frame".into()));
    }
    {
        let mut t = trust.write().unwrap();
        t.add(&server_name, &server_fp);
        t.save()?;
    }
    info!(name = %server_name, fp = %server_fp, "paired with server");
    conn.close(0u32.into(), b"paired");
    Ok(server_name)
}

/// Enables pairing mode on `endpoint`, serves attempts until one succeeds, `max_failures`
/// wrong codes are seen, or `timeout` passes. Pairing mode is always disabled on return.
pub async fn run_server_pairing(
    endpoint: &Endpoint,
    code: &str,
    id: &Identity,
    trust: SharedTrust,
    max_failures: u32,
    timeout: Duration,
) -> Result<String> {
    endpoint.set_pairing(true);
    let result = tokio::time::timeout(timeout, async {
        let mut failures = 0;
        loop {
            match endpoint.accept().await? {
                Incoming::Pairing { conn, fingerprint } => {
                    match server_pair(conn, fingerprint, code, id, trust.clone()).await {
                        Ok(name) => return Ok(name),
                        Err(e) => {
                            failures += 1;
                            warn!("pairing attempt failed ({failures}/{max_failures}): {e}");
                            if failures >= max_failures {
                                return Err(NetError::Pairing("too many failed attempts".into()));
                            }
                        }
                    }
                }
                Incoming::Peer(peer) => {
                    peer.close("pairing in progress");
                }
            }
        }
    })
    .await;
    endpoint.set_pairing(false);
    match result {
        Ok(r) => r,
        Err(_) => Err(NetError::Pairing("timed out".into())),
    }
}
