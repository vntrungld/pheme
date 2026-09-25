//! The clipboard worker: one thread that owns the clipboard handle and the
//! policy, and the two moments the clipboard crosses the network.
//!
//! Sub-project 5 design §3.1, §3.4 and §3.6.

use std::thread;

use crossbeam_channel::{unbounded, Sender};
use pheme_clip::{ClipError, ClipSync, Clipboard};
use pheme_net::PeerSender;
use pheme_proto::{Msg, CLIP_MIME};
use tokio::runtime::Handle;
use tracing::{debug, warn};

enum Cmd {
    /// Read the local clipboard and, if the policy allows, send it to this peer.
    SendTo(PeerSender),
    /// Text that arrived from the peer.
    Apply(String),
}

/// A handle to the clipboard worker. Cheap to clone; every clone reaches the
/// same thread, and therefore the same `ClipSync`.
#[derive(Clone)]
pub struct ClipboardService {
    tx: Sender<Cmd>,
}

impl ClipboardService {
    /// Starts the worker, or returns `None` when no clipboard is reachable.
    ///
    /// `open` runs *on the worker thread*, and the handle it produces never
    /// leaves that thread. On X11 a process owns the `CLIPBOARD` selection from
    /// the thread that created the handle, and that thread has to outlive every
    /// use of it. Taking a factory rather than a handle is what makes that true
    /// by construction — and it is also how a test injects a mock.
    ///
    /// `None` is a supported state, not a failure: GNOME's Wayland compositor
    /// implements no data-control protocol. Callers keep running without
    /// clipboard sharing.
    pub fn spawn(
        open: impl FnOnce() -> Result<Box<dyn Clipboard>, ClipError> + Send + 'static,
    ) -> Option<ClipboardService> {
        let handle = Handle::current();
        let (tx, rx) = unbounded();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("pheme-clipboard".into())
            .spawn(move || {
                let mut clip = match open() {
                    Ok(c) => {
                        let _ = ready_tx.send(true);
                        c
                    }
                    Err(e) => {
                        warn!("clipboard sharing is off: {e}");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let mut sync = ClipSync::new();
                // A clipboard that has stopped answering would otherwise log on
                // every single crossing. The first failure is worth a warning;
                // the rest are not.
                let mut reported = false;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::SendTo(peer) => {
                            let text = match clip.get_text() {
                                Ok(Some(t)) => {
                                    reported = false;
                                    t
                                }
                                Ok(None) => continue,
                                Err(e) => {
                                    if reported {
                                        debug!("reading the clipboard failed: {e}");
                                    } else {
                                        warn!("reading the clipboard failed: {e}");
                                        reported = true;
                                    }
                                    continue;
                                }
                            };
                            let Some(text) = sync.outgoing(text) else {
                                continue;
                            };
                            let m = Msg::Clipboard {
                                mime: CLIP_MIME.to_string(),
                                data: text.into_bytes(),
                            };
                            // The send belongs to the runtime, not to this
                            // thread: the pointer handover that triggered it
                            // must not wait for a network round trip.
                            handle.spawn(async move {
                                if let Err(e) = peer.send_clipboard(&m).await {
                                    debug!("clipboard send failed: {e}");
                                }
                            });
                        }
                        Cmd::Apply(text) => {
                            if !sync.incoming(&text) {
                                continue;
                            }
                            if let Err(e) = clip.set_text(&text) {
                                warn!("writing the clipboard failed: {e}");
                            }
                        }
                    }
                }
            })
            .expect("spawning the clipboard thread");
        match ready_rx.recv() {
            Ok(true) => Some(ClipboardService { tx }),
            // `Err` means the thread ended before reporting, which is the same
            // outcome for the caller as an unavailable clipboard.
            _ => None,
        }
    }

    /// Reads the local clipboard and sends it to `peer`, on the worker thread.
    /// Returns immediately; nothing on the input path waits for it.
    pub fn send_to(&self, peer: PeerSender) {
        let _ = self.tx.send(Cmd::SendTo(peer));
    }

    /// Applies a `Msg::Clipboard` that arrived from the peer. Anything else is
    /// ignored.
    pub fn apply(&self, m: &Msg) {
        let Msg::Clipboard { mime, data } = m else {
            return;
        };
        if mime != CLIP_MIME {
            debug!(%mime, "clipboard message in a format pheme does not speak; ignored");
            return;
        }
        match std::str::from_utf8(data) {
            Ok(text) => {
                let _ = self.tx.send(Cmd::Apply(text.to_string()));
            }
            // A peer sending bytes that are not text is not a reason to stop.
            Err(e) => debug!("clipboard message was not valid UTF-8: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_clip::mock::MockClipboard;
    use pheme_clip::ClipError;
    use std::time::Duration;

    /// The service does its work on another thread, so tests wait for an effect
    /// rather than assuming it has already happened.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[tokio::test]
    async fn an_unavailable_clipboard_yields_no_service() {
        // GNOME Wayland and a headless session both land here, and both must
        // leave the rest of the program running.
        let s = ClipboardService::spawn(|| Err(ClipError::Unavailable("no display".into())));
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn an_incoming_message_reaches_the_clipboard() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"xin ch\xc3\xa0o".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("xin chào")));
    }

    #[tokio::test]
    async fn a_message_that_is_not_utf8_is_ignored() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: vec![0xff, 0xfe, 0xfd],
        });
        // Then something valid, to prove the service is still alive rather than
        // merely slow.
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"after".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("after")));
        assert_eq!(handle.sets(), 1, "the invalid message must not be written");
    }

    #[tokio::test]
    async fn a_message_in_an_unknown_format_is_ignored() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        s.apply(&Msg::Clipboard {
            mime: "image/png".to_string(),
            data: b"not text".to_vec(),
        });
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"text".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("text")));
        assert_eq!(handle.sets(), 1, "the png must not be written");
    }

    #[tokio::test]
    async fn a_failing_clipboard_does_not_stop_the_service() {
        // A compositor restart, or an X11 selection owner that went away: the
        // session must continue, and a later working call must still work.
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        handle.fail_with("the compositor went away");
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"lost".to_vec(),
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            handle.sets(),
            0,
            "a failing backend must not record a write"
        );
        handle.stop_failing();
        s.apply(&Msg::Clipboard {
            mime: CLIP_MIME.to_string(),
            data: b"found".to_vec(),
        });
        assert!(eventually(|| handle.text().as_deref() == Some("found")));
    }

    #[tokio::test]
    async fn the_same_text_is_written_once() {
        let (clip, handle) = MockClipboard::new();
        let s = ClipboardService::spawn(move || Ok(Box::new(clip) as Box<dyn Clipboard>)).unwrap();
        for _ in 0..3 {
            s.apply(&Msg::Clipboard {
                mime: CLIP_MIME.to_string(),
                data: b"once".to_vec(),
            });
        }
        assert!(eventually(|| handle.text().as_deref() == Some("once")));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(handle.sets(), 1);
    }
}
