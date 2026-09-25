//! How the `connect` string becomes an address.
//!
//! Sub-project 5 design §4.3. Parsing is separate from resolving so that every
//! rule about which string means what is testable without a network.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context};
use pheme_net::DEFAULT_PORT;
use tracing::debug;

/// How long to wait for an mDNS answer before falling back to the resolver.
const MDNS_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An address written out in full.
    Fixed(SocketAddr),
    /// A name for the system resolver, with or without a port.
    Dns(String),
    /// An mDNS instance name.
    Mdns(String),
}

impl Target {
    /// Classifies `s`. The rules are ordered, and the first that matches wins.
    pub fn parse(s: &str) -> anyhow::Result<Target> {
        let s = s.trim();
        if s.is_empty() {
            bail!("no server address: pass HOST or set `connect` in the config");
        }
        if let Ok(a) = s.parse::<SocketAddr>() {
            return Ok(Target::Fixed(a));
        }
        // A colon means a port, a dot means a hostname or an IP. Either way the
        // system resolver has always handled it, and this keeps doing that.
        if s.contains(':') || s.contains('.') {
            return Ok(Target::Dns(s.to_string()));
        }
        Ok(Target::Mdns(s.to_string()))
    }

    /// The address to connect to, looked up afresh.
    ///
    /// Called on every reconnect attempt rather than once at startup, which is
    /// what makes a server that changed address or restarted on another port
    /// reachable again without restarting the client.
    pub async fn resolve(&self) -> anyhow::Result<SocketAddr> {
        match self {
            Target::Fixed(a) => Ok(*a),
            Target::Dns(host) => resolve_dns_off_runtime(host.clone()).await,
            Target::Mdns(name) => {
                match pheme_net::discovery::resolve(name, MDNS_TIMEOUT).await {
                    Ok(Some(a)) => Ok(a),
                    Ok(None) => {
                        // A bare name the local resolver knows still works.
                        debug!(%name, "no mdns answer; trying the system resolver");
                        resolve_dns_off_runtime(name.clone()).await
                    }
                    Err(e) => {
                        debug!(%name, "mdns lookup failed: {e}; trying the system resolver");
                        resolve_dns_off_runtime(name.clone()).await
                    }
                }
            }
        }
    }
}

/// Runs the system resolver without occupying a runtime thread.
///
/// `to_socket_addrs` is a blocking call that can take seconds, and `resolve`
/// runs on every reconnect attempt: left on the runtime it would starve every
/// other task scheduled on that worker for the length of the lookup.
async fn resolve_dns_off_runtime(host: String) -> anyhow::Result<SocketAddr> {
    tokio::task::spawn_blocking(move || resolve_dns(&host))
        .await
        .context("the DNS lookup task failed")?
}

fn resolve_dns(host: &str) -> anyhow::Result<SocketAddr> {
    let with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    };
    let mut addrs = with_port
        .to_socket_addrs()
        .with_context(|| format!("resolving {with_port}"))?;
    match addrs.find(|a| a.is_ipv4()) {
        Some(a) => Ok(a),
        None => bail!("{with_port} did not resolve to an IPv4 address"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_address_is_used_as_written() {
        assert_eq!(
            Target::parse("192.168.1.5:24800").unwrap(),
            Target::Fixed("192.168.1.5:24800".parse().unwrap())
        );
    }

    #[test]
    fn an_ipv6_literal_is_a_socket_address() {
        assert_eq!(
            Target::parse("[::1]:24800").unwrap(),
            Target::Fixed("[::1]:24800".parse().unwrap())
        );
    }

    #[test]
    fn a_bare_ip_goes_to_the_resolver() {
        // It has a dot, so it takes the path that already worked before mDNS
        // existed; `ToSocketAddrs` turns it into an address without a lookup.
        assert_eq!(
            Target::parse("10.0.0.4").unwrap(),
            Target::Dns("10.0.0.4".into())
        );
    }

    #[test]
    fn a_host_and_port_goes_to_the_resolver() {
        assert_eq!(
            Target::parse("laptop:24800").unwrap(),
            Target::Dns("laptop:24800".into())
        );
    }

    #[test]
    fn a_dotted_hostname_goes_to_the_resolver() {
        assert_eq!(
            Target::parse("laptop.lan").unwrap(),
            Target::Dns("laptop.lan".into())
        );
    }

    #[test]
    fn a_bare_label_is_an_mdns_name() {
        assert_eq!(
            Target::parse("laptop-win").unwrap(),
            Target::Mdns("laptop-win".into())
        );
    }

    #[test]
    fn an_empty_target_is_an_error() {
        // Never an mDNS browse for the empty name: that would wait out the
        // timeout on every reconnect and never find anything.
        assert!(Target::parse("").is_err());
    }

    #[test]
    fn a_whitespace_target_is_an_error() {
        assert!(Target::parse("   ").is_err());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            Target::parse("  laptop-win  ").unwrap(),
            Target::Mdns("laptop-win".into())
        );
    }
}
