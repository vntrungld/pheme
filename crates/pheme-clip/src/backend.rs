//! The platform clipboard, through `arboard`.
//!
//! This is the one backend in this repository that is not written here, and
//! §3.3 of the sub-project 5 design says why: a crate exists that does exactly
//! this job on all three platforms, and the X11 half of the job is not a thin
//! wrapper — X11 has no clipboard, only selections, and a process that sets one
//! must own `CLIPBOARD` and answer every `SelectionRequest` from every other
//! client for as long as it holds it.
//!
//! On Linux Wayland, `arboard` tries the compositor's data-control protocol
//! and falls back to its X11 backend when that protocol is not offered. On
//! GNOME, whose Mutter compositor implements neither `wlr-data-control` nor
//! `ext-data-control`, that fallback is what runs: it succeeds through
//! Xwayland, which GNOME starts by default, so `open()` returns `Ok` there,
//! not `Err(Unavailable)`. `Err(Unavailable)` is reached only where the
//! fallback has nothing to fall back to — no Xwayland and no data-control
//! protocol, as on a bare compositor or a headless session.

use arboard::Clipboard as Arboard;

use crate::{ClipError, Clipboard};

pub(crate) struct SystemClipboard(Arboard);

impl SystemClipboard {
    pub(crate) fn open() -> Result<SystemClipboard, ClipError> {
        Arboard::new()
            .map(SystemClipboard)
            .map_err(|e| ClipError::Unavailable(e.to_string()))
    }
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<Option<String>, ClipError> {
        match self.0.get_text() {
            Ok(t) => Ok(Some(t)),
            // Not an error: this is what an empty clipboard and an image on the
            // clipboard both look like, and in both cases there is nothing to send.
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(ClipError::Backend(e.to_string())),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipError> {
        self.0
            .set_text(text)
            .map_err(|e| ClipError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opening_the_clipboard_never_panics() {
        // Both outcomes are correct. On CI there is no display and this is
        // `Err(Unavailable)`; on a desktop it is `Ok`. What must never happen is
        // a panic, because `open()` runs during startup on every platform and a
        // panic there would take input and audio down with it.
        let _ = crate::open();
    }

    /// The real round trip, which needs a session no CI runner has.
    /// Run by hand with `cargo test -p pheme-clip -- --ignored`.
    #[test]
    #[ignore]
    fn text_survives_a_round_trip_through_the_system_clipboard() {
        let mut c = crate::open().expect("a desktop session");
        c.set_text("pheme round trip ✅ xin chào").unwrap();
        assert_eq!(
            c.get_text().unwrap().as_deref(),
            Some("pheme round trip ✅ xin chào")
        );
    }
}
