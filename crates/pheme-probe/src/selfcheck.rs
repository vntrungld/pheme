//! The loopback self-check: emit and record on one machine, with pheme not
//! running at all, and require the recorder to hear exactly what was sent.
//!
//! It proves the probe pair works before any cross-machine row is judged.
//! Without it, a broken recorder fails every row and the report blames pheme
//! -- a whole run of false FAIL verdicts, which is worse than no run. The
//! runner treats a self-check failure as a rig error, never as FAIL.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::gesture::{expand, parse, ProbeEvent};
use crate::record::{record, Watch};
use crate::tally::{tally, Tally};

/// Keys, a shifted key, motion in both axes, and the wheel both ways: every
/// kind of event a row counts. Moves return to where they started.
pub const GESTURE: &str = "type Ab; move 12 0; move 0 -7; move -12 7; wheel -1; wheel 2";

pub fn expected() -> Tally {
    tally(&expand(&parse(GESTURE).expect("GESTURE parses")))
}

/// Passes only when the recording's tally is exactly the gesture's.
pub fn judge(recorded: &[ProbeEvent]) -> Result<Tally> {
    let want = expected();
    let got = tally(recorded);
    if got != want {
        bail!("the recorder heard {got:?}, but {want:?} was sent");
    }
    Ok(got)
}

/// Emits [`GESTURE`] through the platform injector and records it.
pub fn run(device_hint: Option<String>, timeout: Duration) -> Result<Tally> {
    let events = expand(&parse(GESTURE)?);
    let mut inj = crate::emit::open()?;
    let deadline = Instant::now() + timeout;
    let want = expected();

    let (ready_tx, ready_rx) = mpsc::channel();
    let recorder = std::thread::spawn(move || {
        let mut ready = move || {
            let _ = ready_tx.send(());
        };
        let done = |seen: &[ProbeEvent]| tally(seen) == want;
        record(Watch {
            deadline,
            device_hint: device_hint.as_deref(),
            ready: &mut ready,
            done: &done,
        })
    });

    // Emitting before the recorder listens would lose the first events and
    // fail the check for a reason that is not the recorder's.
    let listening = ready_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_ok();
    if listening {
        crate::emit::send(inj.as_mut(), &events)?;
    }
    let recorded = recorder
        .join()
        .map_err(|_| anyhow!("the recorder thread panicked"))??;
    if !listening {
        bail!("the recorder never started listening");
    }
    judge(&recorded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::{expand, parse};

    /// The check exists to fail when the recorder hears nothing. The
    /// production change that breaks this: shrink the gesture to nothing --
    /// an empty tally then equals an empty tally, and a recorder that hears
    /// nothing passes.
    #[test]
    fn an_empty_recording_fails_the_selfcheck() {
        assert!(judge(&[]).is_err());
    }

    /// The gesture must exercise everything a row will count, or a recorder
    /// that loses, say, wheel events passes the check and fails every
    /// scrolling row instead -- blaming pheme.
    #[test]
    fn the_gesture_exercises_keys_shift_motion_and_wheel() {
        let t = expected();
        assert!(t.downs > 0 && t.downs == t.ups, "{t:?}");
        assert!(t.motion > 0 && t.wheel > 0, "{t:?}");
        assert!(t.held.is_empty() && t.unmatched_ups == 0, "{t:?}");
        let events = expand(&parse(GESTURE).expect("valid"));
        assert!(
            events.contains(&ProbeEvent::KeyDown(pheme_proto::KeyCode::LEFT_SHIFT)),
            "no shifted character in {GESTURE:?}"
        );
    }

    #[test]
    fn hearing_exactly_what_was_sent_passes() {
        let events = expand(&parse(GESTURE).expect("valid"));
        assert_eq!(judge(&events).expect("matches"), expected());
    }

    /// The production change that breaks this: accept a recording that
    /// merely contains the expected events, so a recorder doubling every
    /// event would pass.
    #[test]
    fn hearing_more_than_was_sent_fails() {
        let mut events = expand(&parse(GESTURE).expect("valid"));
        events.push(ProbeEvent::Wheel(1));
        assert!(judge(&events).is_err());
    }
}
