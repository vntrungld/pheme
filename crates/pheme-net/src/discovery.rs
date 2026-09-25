//! Finding a Pheme server on the local network.
//!
//! The server registers `_pheme._udp.local.` under the name from its
//! configuration; the client resolves that name. Discovery never chooses a
//! server: it answers the question "where is the machine called X", and the
//! user says which X. Sub-project 5 design §4.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::time::Instant;
use tracing::debug;

use crate::{NetError, Result};

/// The DNS-SD service type. QUIC runs over UDP, hence `_udp`.
pub const SERVICE_TYPE: &str = "_pheme._udp.local.";

/// The TXT key carrying the server's certificate fingerprint.
const TXT_FINGERPRINT: &str = "fp";
/// The TXT key carrying the advertisement format version.
const TXT_VERSION: &str = "v";

/// A server Pheme found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub addr: SocketAddr,
    /// The fingerprint the server published. Advisory only: it is shown so a
    /// person can compare it with what `pheme pair` prints. Trust comes from
    /// pairing, never from a TXT record.
    pub fingerprint: Option<String>,
}

/// The instance label back out of a full DNS-SD name, with escapes undone.
///
/// `mdns-sd` escapes a dot inside an instance label when it builds the full
/// name from the raw name passed to [`advertise`] (see
/// `ServiceInfo::new`/`escape_instance_name` in the installed crate). `browse`
/// and `resolve` match on the unescaped name, so this is the inverse of that
/// escaping.
fn name_from_instance(full: &str) -> String {
    let label = full
        .strip_suffix(&format!(".{SERVICE_TYPE}"))
        .unwrap_or(full);
    label.replace("\\.", ".")
}

/// A live advertisement. Dropping it unregisters the service, so a server that
/// exits cleanly stops answering at once instead of leaving a stale record for
/// other machines to time out.
///
/// **Dropping blocks the calling thread** for up to 500 ms, waiting for the
/// daemon to acknowledge the unregister before the daemon is shut down;
/// without that wait the goodbye packet can go unsent. The acknowledgement
/// comes from a thread inside this process, so the wait is normally
/// microseconds. It is still a blocking wait: an `Advertiser` dropped inside
/// an async task blocks that executor thread, so hold it somewhere that is
/// dropped at teardown rather than on a hot path.
pub struct Advertiser {
    daemon: ServiceDaemon,
    full_name: String,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Wait briefly for the unregister (goodbye packet) to actually go
        // out before shutting the daemon thread down, or the shutdown can
        // race the unregister and the goodbye never gets sent. The ack
        // comes from the daemon thread in this same process, not over the
        // network, so 500ms is already a generous bound for it.
        if let Ok(rx) = self.daemon.unregister(&self.full_name) {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
        let _ = self.daemon.shutdown();
    }
}

fn daemon() -> Result<ServiceDaemon> {
    ServiceDaemon::new().map_err(|e| NetError::Connection(format!("mdns: {e}")))
}

/// Publishes this server on the local network.
pub fn advertise(name: &str, port: u16, fingerprint: &str) -> Result<Advertiser> {
    let daemon = daemon()?;
    let props = [(TXT_FINGERPRINT, fingerprint), (TXT_VERSION, "1")];
    // `ServiceInfo::new` escapes dots in `name` itself when it builds the
    // full instance name, so the raw (unescaped) name is passed straight
    // through here.
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        name,
        &format!("{}.local.", name.replace('.', "-")),
        (),
        port,
        &props[..],
    )
    .map_err(|e| NetError::Connection(format!("mdns: {e}")))?
    .enable_addr_auto();
    let full_name = info.get_fullname().to_string();
    daemon
        .register(info)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;
    Ok(Advertiser { daemon, full_name })
}

/// Every Pheme server seen before `timeout` elapses.
pub async fn browse(timeout: Duration) -> Result<Vec<Found>> {
    let daemon = daemon()?;
    let rx = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;

    let deadline = Instant::now() + timeout;
    let mut out: Vec<Found> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                let Some(ip) = info.get_addresses_v4().into_iter().next() else {
                    continue;
                };
                let addr = SocketAddr::new(IpAddr::V4(ip), info.get_port());
                let name = name_from_instance(info.get_fullname());
                if out.iter().any(|f: &Found| f.name == name && f.addr == addr) {
                    continue;
                }
                out.push(Found {
                    name,
                    addr,
                    fingerprint: info
                        .get_property_val_str(TXT_FINGERPRINT)
                        .map(str::to_string),
                });
            }
            Ok(Ok(_)) => {}
            // The channel closed (daemon gone) or the timeout elapsed:
            // either way, there is nothing more to wait for.
            Ok(Err(_)) | Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    debug!(count = out.len(), "mdns browse finished");
    Ok(out)
}

/// The address of the server called `name`, or `None` if none answered in time.
///
/// Returns as soon as a match resolves rather than waiting out the timeout, so
/// a reconnect that finds its server costs one round trip, not three seconds.
pub async fn resolve(name: &str, timeout: Duration) -> Result<Option<SocketAddr>> {
    let daemon = daemon()?;
    let rx = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| NetError::Connection(format!("mdns: {e}")))?;

    let deadline = Instant::now() + timeout;
    let found = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break None;
        }
        match tokio::time::timeout(remaining, rx.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                if name_from_instance(info.get_fullname()) != name {
                    continue;
                }
                if let Some(ip) = info.get_addresses_v4().into_iter().next() {
                    break Some(SocketAddr::new(IpAddr::V4(ip), info.get_port()));
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break None,
        }
    };
    let _ = daemon.shutdown();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_name_keeps_its_dots() {
        // DNS-SD escapes a dot inside an instance label. Both `browse` and
        // `resolve` match on the unescaped name, so a machine called "my.desk"
        // must still be found.
        assert_eq!(
            name_from_instance("my\\.desk._pheme._udp.local."),
            "my.desk"
        );
    }

    #[test]
    fn a_plain_name_round_trips() {
        assert_eq!(
            name_from_instance("desk-linux._pheme._udp.local."),
            "desk-linux"
        );
    }

    /// The real round trip. Ignored because GitHub's runners do not reliably
    /// carry multicast, and a test that passes because nothing listened is
    /// worse than no test. Run by hand:
    /// `cargo test -p pheme-net --lib discovery -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn a_registered_service_can_be_resolved() {
        let _a = advertise("pheme-test-instance", 24800, "deadbeef").unwrap();
        let addr = resolve("pheme-test-instance", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            addr.is_some(),
            "the service we just registered was not found"
        );
        let found = browse(Duration::from_secs(5)).await.unwrap();
        let ours = found
            .iter()
            .find(|f| f.name == "pheme-test-instance")
            .expect("our own instance");
        assert_eq!(ours.fingerprint.as_deref(), Some("deadbeef"));
    }
}
