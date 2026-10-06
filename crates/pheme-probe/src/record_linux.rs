//! Reads the evdev device pheme created: the OS input layer, independent of
//! pheme's own accounting. A recorder that read pheme's `--stats` instead
//! would be asking the accused for a verdict.

use std::io::ErrorKind;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::gesture::ProbeEvent;
use crate::record::{describe, name_matches, EvdevFrames, Watch};

pub fn record(watch: Watch<'_>) -> Result<Vec<ProbeEvent>> {
    let Watch {
        deadline,
        device_hint,
        ready,
        done,
    } = watch;
    // pheme's client creates its device when it connects, which may be after
    // the recorder starts, so its absence is waited out rather than fatal.
    let mut dev = loop {
        let mut found: Vec<_> = evdev::enumerate()
            .filter(|(_, d)| name_matches(d.name(), device_hint))
            .collect();
        match found.len() {
            1 => break found.remove(0).1,
            0 => {}
            // Never a guess: recording the wrong device would judge a row on
            // input that never went through pheme.
            n => bail!(
                "{n} input devices match {}; pass a narrower --device-hint",
                describe(device_hint)
            ),
        }
        if Instant::now() >= deadline {
            bail!(
                "no input device matching {} appeared before the deadline",
                describe(device_hint)
            );
        }
        std::thread::sleep(Duration::from_millis(50)); // poll interval
    };
    dev.set_nonblocking(true)
        .context("making the input device non-blocking")?;
    ready();

    let mut frames = EvdevFrames::default();
    let mut events = Vec::new();
    while Instant::now() < deadline && !done(&events) {
        match dev.fetch_events() {
            Ok(batch) => {
                for ev in batch {
                    events.extend(frames.push(ev.event_type().0, ev.code(), ev.value()));
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5)); // poll interval
            }
            Err(e) => return Err(e).context("reading the input device"),
        }
    }
    Ok(events)
}
