//! The local socket between the front-end and the core it supervises.

mod proto;
mod transport;

pub use proto::{decode_frame, encode_frame, Command, IpcError, LinkState, Status, MAX_IPC_FRAME};
pub use transport::{CoreLink, IpcConnection, IpcListener};
