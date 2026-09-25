//! What crosses the network and what does not.
//!
//! This is the clipboard's whole policy, and it touches neither the operating
//! system nor the network so that all of it can be tested. See §3.4 of the
//! sub-project 5 design.

use pheme_proto::MAX_CLIP_BYTES;
use tracing::warn;

/// Remembers the last text this side exchanged, in either direction.
///
/// One field answers three questions. Content equal to `last` is not sent,
/// which stops repeats. Content *received* also becomes `last`, which is what
/// stops an echo: text that arrived from the peer is never sent back to it.
#[derive(Debug, Default)]
pub struct ClipSync {
    last: Option<String>,
}

impl ClipSync {
    pub fn new() -> ClipSync {
        ClipSync::default()
    }

    /// The text to send to the peer, or `None` to send nothing.
    pub fn outgoing(&mut self, text: String) -> Option<String> {
        if text.is_empty() {
            // An empty clipboard carries no intent, and sending it would clear
            // the peer's.
            return None;
        }
        if text.len() > MAX_CLIP_BYTES {
            // Deliberately not recorded in `last`: refusing this must not also
            // refuse whatever the user copies next.
            warn!(
                bytes = text.len(),
                limit = MAX_CLIP_BYTES,
                "clipboard content is too large to share; it stays on this machine"
            );
            return None;
        }
        if self.last.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last = Some(text.clone());
        Some(text)
    }

    /// Whether the caller should write `text` to the local clipboard.
    pub fn incoming(&mut self, text: &str) -> bool {
        if text.is_empty() || text.len() > MAX_CLIP_BYTES {
            return false;
        }
        if self.last.as_deref() == Some(text) {
            return false;
        }
        self.last = Some(text.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_clipboard_is_not_sent() {
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing(String::new()), None);
    }

    #[test]
    fn the_first_text_is_sent() {
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing("hello".into()), Some("hello".to_string()));
    }

    #[test]
    fn the_same_text_is_not_sent_twice() {
        let mut s = ClipSync::new();
        assert!(s.outgoing("hello".into()).is_some());
        assert_eq!(s.outgoing("hello".into()), None);
    }

    #[test]
    fn new_text_is_sent_after_a_repeat() {
        let mut s = ClipSync::new();
        assert!(s.outgoing("one".into()).is_some());
        assert_eq!(s.outgoing("one".into()), None);
        assert_eq!(s.outgoing("two".into()), Some("two".to_string()));
    }

    #[test]
    fn text_received_from_the_peer_does_not_echo_back() {
        // The whole reason `last` records both directions: without this, crossing
        // back would return the peer's own text to it on every crossing.
        let mut s = ClipSync::new();
        assert!(s.incoming("from the peer"));
        assert_eq!(s.outgoing("from the peer".into()), None);
    }

    #[test]
    fn the_same_text_is_not_applied_twice() {
        let mut s = ClipSync::new();
        assert!(s.incoming("hello"));
        assert!(!s.incoming("hello"));
    }

    #[test]
    fn an_empty_message_does_not_clear_the_local_clipboard() {
        let mut s = ClipSync::new();
        assert!(!s.incoming(""));
    }

    #[test]
    fn oversized_text_is_not_sent() {
        let mut s = ClipSync::new();
        let big = "x".repeat(MAX_CLIP_BYTES + 1);
        assert_eq!(s.outgoing(big), None);
    }

    #[test]
    fn a_refused_oversized_text_does_not_block_the_next_one() {
        // `outgoing` must not record what it refused: otherwise copying something
        // huge and then something small would send neither.
        let mut s = ClipSync::new();
        assert_eq!(s.outgoing("x".repeat(MAX_CLIP_BYTES + 1)), None);
        assert_eq!(s.outgoing("small".into()), Some("small".to_string()));
    }

    #[test]
    fn text_exactly_at_the_limit_is_sent() {
        let mut s = ClipSync::new();
        let at = "x".repeat(MAX_CLIP_BYTES);
        assert_eq!(s.outgoing(at.clone()), Some(at));
    }

    #[test]
    fn oversized_text_from_a_peer_is_refused() {
        let mut s = ClipSync::new();
        assert!(!s.incoming(&"x".repeat(MAX_CLIP_BYTES + 1)));
    }
}
