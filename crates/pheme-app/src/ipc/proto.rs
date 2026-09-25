//! What the front-end and the core say to each other.
//!
//! Deliberately not `pheme_proto::Msg`. That enum is the protocol between two
//! machines; these types exist only for a local GUI and must never make the
//! wire protocol carry a field for its benefit. Sub-project 6 design §4.

use serde::{Deserialize, Serialize};

use crate::config::Role;

/// The largest IPC frame either side will send or accept.
///
/// About a thousand times the largest `Status`. It exists to bound a reader
/// against a length prefix the other end controls, not to carry anything.
pub const MAX_IPC_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkState {
    Starting,
    Listening,
    Connecting,
    Connected,
    /// The core is running but the link failed, and this is why.
    Failed(String),
}

/// Pushed by the core once a second, and once immediately on connect.
///
/// Not every field is meaningful to every role, and the spec fixes which:
/// `lost` is counted only by the client, which numbers the gaps in the
/// server's sequence, so a server sends `0`. `audio_*` describes the stream
/// the server plays and `mic_*` the stream the client plays, so each side
/// fills the pair it owns and sends `0` for the other. A zero therefore means
/// "not measured here", and the window labels fields by role rather than
/// showing a misleading nought for something the other end would have counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub role: Role,
    pub state: LinkState,
    pub peer: Option<String>,
    pub rtt_us: u64,
    pub locked: bool,
    pub events: u64,
    pub lost: u64,
    pub audio_depth_ms: u32,
    pub audio_lost: u64,
    pub mic_depth_ms: u32,
    pub mic_lost: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Lock,
    Unlock,
    /// Stop cleanly. The front-end sends this before restarting the child with
    /// a changed configuration.
    Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("ipc frame of {0} bytes is over the limit")]
    TooLong(usize),
    #[error("ipc encoding: {0}")]
    Codec(#[from] postcard::Error),
    #[error("ipc io: {0}")]
    Io(#[from] std::io::Error),
}

/// Appends one length-prefixed frame to `buf`.
pub fn encode_frame<T: Serialize>(value: &T, buf: &mut Vec<u8>) -> Result<(), IpcError> {
    let body = postcard::to_stdvec(value)?;
    if body.len() > MAX_IPC_FRAME {
        return Err(IpcError::TooLong(body.len()));
    }
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(())
}

/// Reads one frame from the front of `buf`.
///
/// `Ok(None)` means the buffer holds less than a whole frame, which is the
/// normal state of a socket read and not an error. `Ok(Some((value, used)))`
/// returns the value and how many bytes it consumed, so the caller can drain
/// exactly that much and look again.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, IpcError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // Checked before the body is looked at, let alone allocated: the prefix
    // comes from the other end of the socket.
    if len > MAX_IPC_FRAME {
        return Err(IpcError::TooLong(len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let value = postcard::from_bytes(&buf[4..4 + len])?;
    Ok(Some((value, 4 + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> Status {
        Status {
            role: Role::Server,
            state: LinkState::Connected,
            peer: Some("laptop-win".into()),
            rtt_us: 412,
            locked: false,
            events: 91,
            lost: 0,
            audio_depth_ms: 22,
            audio_lost: 3,
            mic_depth_ms: 0,
            mic_lost: 0,
        }
    }

    #[test]
    fn a_status_survives_a_round_trip() {
        let mut buf = Vec::new();
        encode_frame(&sample_status(), &mut buf).unwrap();
        let (m, used) = decode_frame::<Status>(&buf).unwrap().unwrap();
        assert_eq!(m, sample_status());
        assert_eq!(used, buf.len(), "the whole frame was consumed");
    }

    #[test]
    fn every_command_survives_a_round_trip() {
        for c in [Command::Lock, Command::Unlock, Command::Stop] {
            let mut buf = Vec::new();
            encode_frame(&c, &mut buf).unwrap();
            let (got, _) = decode_frame::<Command>(&buf).unwrap().unwrap();
            assert_eq!(got, c);
        }
    }

    #[test]
    fn a_failure_message_survives_a_round_trip() {
        // The reason a link failed is the one thing a person needs and today
        // it exists only in the log, so it must cross intact.
        let s = Status {
            state: LinkState::Failed("address already in use".into()),
            ..sample_status()
        };
        let mut buf = Vec::new();
        encode_frame(&s, &mut buf).unwrap();
        let (got, _) = decode_frame::<Status>(&buf).unwrap().unwrap();
        assert_eq!(
            got.state,
            LinkState::Failed("address already in use".into())
        );
    }

    #[test]
    fn an_incomplete_frame_asks_for_more_rather_than_failing() {
        // A socket read can stop anywhere. Half a frame is not an error.
        let mut buf = Vec::new();
        encode_frame(&sample_status(), &mut buf).unwrap();
        for cut in [0, 1, 3, buf.len() - 1] {
            assert!(
                decode_frame::<Status>(&buf[..cut]).unwrap().is_none(),
                "{cut} bytes should have been treated as incomplete"
            );
        }
    }

    #[test]
    fn two_frames_in_one_buffer_are_read_one_at_a_time() {
        let mut buf = Vec::new();
        encode_frame(&Command::Lock, &mut buf).unwrap();
        let first_len = buf.len();
        encode_frame(&Command::Stop, &mut buf).unwrap();
        let (a, used) = decode_frame::<Command>(&buf).unwrap().unwrap();
        assert_eq!(a, Command::Lock);
        assert_eq!(used, first_len);
        let (b, _) = decode_frame::<Command>(&buf[used..]).unwrap().unwrap();
        assert_eq!(b, Command::Stop);
    }

    #[test]
    fn an_oversized_frame_is_refused_without_being_decoded() {
        // The length prefix is attacker-controlled once the socket is open.
        // Refusing on the prefix alone is what stops a reader allocating on it.
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_IPC_FRAME + 1) as u32).to_le_bytes());
        assert!(decode_frame::<Status>(&buf).is_err());
    }

    #[test]
    fn a_frame_of_rubbish_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert!(decode_frame::<Status>(&buf).is_err());
    }
}
