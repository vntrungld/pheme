//! QUIC transport with pinned-certificate mTLS, pairing and reconnect helpers.

mod fsutil;
pub mod identity;
pub mod pairing;
pub mod transport;
pub mod trust;
pub mod verifier;

pub use identity::Identity;
// pub use transport::{CloseReason, Endpoint, Incoming, Peer, PeerSender};
pub use trust::{SharedTrust, TrustStore, TrustedPeer};

pub const ALPN_MAIN: &[u8] = b"pheme/1";
pub const ALPN_PAIR: &[u8] = b"pheme-pair/1";
pub const DEFAULT_PORT: u16 = 24800;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("connection: {0}")]
    Connection(String),
    #[error(transparent)]
    Proto(#[from] pheme_proto::ProtoError),
    #[error("untrusted peer {0}")]
    Untrusted(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
}

pub type Result<T> = std::result::Result<T, NetError>;
