//! Prints capture events for 5 s in Observe mode, then grabs for 3 s, then exits.
use std::time::{Duration, Instant};

use pheme_input::CaptureMode;

fn main() {
    let mut cap = pheme_input::detect_capture().expect("capture backend");
    println!("screens: {:?}", cap.screens());
    let (tx, rx) = crossbeam_channel::bounded(1024);
    cap.start(tx).unwrap();
    let phases = [
        (CaptureMode::Observe, 5),
        (CaptureMode::Grab, 3),
        (CaptureMode::Observe, 1),
    ];
    for (mode, secs) in phases {
        println!("== {mode:?} for {secs}s ==");
        cap.set_mode(mode).unwrap();
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                println!("{ev:?}");
            }
        }
    }
    cap.warp_cursor(100, 100).unwrap();
    cap.stop();
}
