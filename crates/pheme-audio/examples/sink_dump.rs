//! Creates the "Pheme Speaker" sink and prints how loud what it receives is.
//!
//! Run it, then in the system sound settings select "Pheme Speaker" as the output and
//! play something. Peaks should track the audio, and the node should disappear from
//! `pactl list sinks short` as soon as the example exits.

use std::time::{Duration, Instant};

use pheme_audio::FRAME_INTERLEAVED;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let mut cap = pheme_audio::detect_capture(None)?;
    let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 200);
    cap.start(producer)?;
    println!(
        "\"{}\" is live. Select it as your output device.",
        cap.device_name()
    );

    let started = Instant::now();
    let mut frame = Vec::with_capacity(FRAME_INTERLEAVED);
    let mut peak = 0i32;
    let mut last_report = Instant::now();
    while started.elapsed() < Duration::from_secs(60) {
        while consumer.slots() >= FRAME_INTERLEAVED {
            frame.clear();
            for _ in 0..FRAME_INTERLEAVED {
                match consumer.pop() {
                    Ok(s) => frame.push(s),
                    Err(_) => break,
                }
            }
            peak = peak.max(frame.iter().map(|s| i32::from(s.abs())).max().unwrap_or(0));
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "peak {peak:>6}  ({:.0}% of full scale)",
                peak as f32 / 327.68
            );
            peak = 0;
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cap.stop();
    Ok(())
}
