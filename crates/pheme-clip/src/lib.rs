//! The system text clipboard, behind one trait with a mock.
//!
//! Sub-project 5 design: `docs/superpowers/specs/2026-09-25-clipboard-discovery-design.md`.

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
