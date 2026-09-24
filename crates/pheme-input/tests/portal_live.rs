//! Exercises the real InputCapture portal. Ignored by default: it needs a Wayland
//! session with a compositor that implements the portal, and it raises a permission
//! dialog.
//!
//! Run with:
//!   cargo test -p pheme-input --test portal_live -- --ignored --nocapture
//!
//! It does NOT capture input — it stops before `Enable` has anything to trigger it,
//! so it cannot take the keyboard away from whoever is running it.

#![cfg(target_os = "linux")]

#[test]
#[ignore = "needs a Wayland session and raises a permission dialog"]
fn a_session_can_be_created_and_barriers_accepted() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        panic!("not a Wayland session");
    }
    let mut cap = pheme_input::portal::PortalCapture::new().expect("portal backend");
    let screens = pheme_input::InputCapture::screens(&cap);
    assert!(!screens.is_empty(), "wl_output reported no screens");

    let (tx, _rx) = crossbeam_channel::unbounded();
    pheme_input::InputCapture::start(&mut cap, tx).expect("session start");

    // The edge the design probe used. If the barrier convention is wrong the session
    // thread reports it here rather than sitting silent forever.
    let edges = [pheme_input::CaptureEdge {
        side: pheme_core::Side::Right,
        span: (0.0, 1.0),
    }];
    pheme_input::InputCapture::set_edges(&mut cap, &edges).expect("barriers accepted");

    pheme_input::InputCapture::stop(&mut cap);
}
