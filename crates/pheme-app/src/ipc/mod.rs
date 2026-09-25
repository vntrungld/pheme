//! The local socket between the front-end and the core it supervises.

mod proto;

pub use proto::{decode_frame, encode_frame, Command, IpcError, LinkState, Status, MAX_IPC_FRAME};
