//! The system text clipboard, behind one trait with a mock.
//!
//! Sub-project 5 design: `docs/superpowers/specs/2026-09-25-clipboard-discovery-design.md`.

mod backend;
pub mod mock;
mod sync;

pub use sync::ClipSync;

#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    /// No clipboard exists to talk to. This is a supported state, not a bug:
    /// GNOME's compositor implements no data-control protocol, and a headless
    /// session has no clipboard at all. The caller runs without clipboard
    /// sharing and everything else keeps working.
    #[error("no clipboard is available: {0}")]
    Unavailable(String),
    /// A clipboard exists but this call did not work.
    #[error("clipboard: {0}")]
    Backend(String),
}

/// Read and write the system clipboard's text.
///
/// Deliberately not `Send`: on X11 a clipboard handle owns the `CLIPBOARD`
/// selection on the thread that created it, so the handle is created on the
/// thread that will use it and never moves. `ClipboardService` in `pheme-app`
/// takes a factory rather than a handle for exactly this reason.
pub trait Clipboard {
    /// `Ok(None)` when the clipboard holds no text. An image on the clipboard
    /// is `Ok(None)`, not an error: there is simply nothing to send.
    fn get_text(&mut self) -> Result<Option<String>, ClipError>;
    fn set_text(&mut self, text: &str) -> Result<(), ClipError>;
}

/// Opens the platform clipboard.
///
/// `Err(ClipError::Unavailable)` is a supported outcome, not a failure to
/// handle. GNOME's Wayland compositor implements neither `wlr-data-control` nor
/// `ext-data-control` and has declined to, so there is no route for a
/// window-less process; a headless session has no clipboard at all. The caller
/// logs once and runs without clipboard sharing. §3.3.
pub fn open() -> Result<Box<dyn Clipboard>, ClipError> {
    backend::SystemClipboard::open().map(|c| Box::new(c) as Box<dyn Clipboard>)
}
