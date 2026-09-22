//! Runs the complete audio pipeline on one machine: the "Pheme Speaker" sink feeds the
//! packer, the frames go straight into a jitter buffer, and the playback backend plays
//! them on the default output.
//!
//! Select "Pheme Speaker" as the system output and play music: you should hear it on
//! your normal speakers, delayed by about 20 ms, with no crackle. This is the same code
//! path the server runs, minus the network.

use std::time::{Duration, Instant};

use pheme_audio::drift::DriftController;
use pheme_audio::jitter::{JitterBuffer, Pop};
use pheme_audio::pack::Packer;
use pheme_audio::{FRAME_INTERLEAVED, FRAME_US, RATE};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let mut cap = pheme_audio::detect_capture(None)?;
    let (cap_tx, mut cap_rx) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 200);
    cap.start(cap_tx)?;

    let mut play = pheme_audio::detect_playback(None)?;
    let (mut play_tx, play_rx) = rtrb::RingBuffer::<i16>::new(FRAME_INTERLEAVED * 4);
    play.start(play_rx)?;

    println!(
        "capture: {}   playback: {}   (60 s)",
        cap.device_name(),
        play.device_name()
    );

    let mut packer = Packer::new();
    let mut jitter = JitterBuffer::new();
    let mut drift = DriftController::new(f64::from(play.rate()) / f64::from(RATE));
    let mut taken = 0u64;
    let mut frame = Vec::with_capacity(FRAME_INTERLEAVED);
    let started = Instant::now();
    let mut last_report = Instant::now();

    while started.elapsed() < Duration::from_secs(60) {
        // Capture side: whole frames only.
        while cap_rx.slots() >= FRAME_INTERLEAVED {
            frame.clear();
            for _ in 0..FRAME_INTERLEAVED {
                match cap_rx.pop() {
                    Ok(s) => frame.push(s),
                    Err(_) => break,
                }
            }
            if frame.len() == FRAME_INTERLEAVED {
                if let Some(f) = packer.push(&frame, taken * FRAME_US) {
                    jitter.push(f);
                }
                taken += 1;
            }
        }
        // Playback side: keep the ring about one frame deep.
        while play_tx.slots() >= FRAME_INTERLEAVED * 3 {
            let st = jitter.stats();
            let _ratio = drift.tick(st.depth, st.target);
            let samples = match jitter.pop() {
                Pop::Data(s) | Pop::Conceal(s) => s,
                Pop::Idle => vec![0; FRAME_INTERLEAVED],
            };
            for s in samples {
                let _ = play_tx.push(s);
            }
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            let st = jitter.stats();
            println!(
                "depth {} target {} lost {} underruns {} ratio {:.6} healthy {}/{}",
                st.depth,
                st.target,
                st.lost,
                st.underruns,
                drift.ratio(),
                cap.healthy(),
                play.healthy()
            );
            last_report = Instant::now();
        }
        // The supervisor in `pheme-app` polls exactly this and rebuilds both backends.
        // Stopping here instead is what makes a daemon restart visible in this example.
        if !cap.healthy() || !play.healthy() {
            println!(
                "a backend reported ill health (capture {}, playback {}); stopping",
                cap.healthy(),
                play.healthy()
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cap.stop();
    play.stop();
    Ok(())
}
