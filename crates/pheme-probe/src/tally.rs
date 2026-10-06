//! Turning a recorded event stream into the numbers a row asserts on.
//!
//! Pure, and deliberately separate from the platform recorders: this is where
//! "nothing was left held" is decided, and that decision is too important to
//! live somewhere only a VM can reach.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::gesture::ProbeEvent;
use pheme_proto::KeyCode;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub downs: u32,
    pub ups: u32,
    pub motion: u32,
    pub wheel: u32,
    /// Keys still down when recording stopped. Tracked as a set of keys, not
    /// inferred from `downs != ups`: a stream with equal counts across two
    /// different keys can still leave one held.
    pub held: Vec<KeyCode>,
    /// Key-ups with no matching down. Non-zero means the recording started
    /// mid-gesture, so the other numbers describe a fragment.
    pub unmatched_ups: u32,
}

pub fn tally(events: &[ProbeEvent]) -> Tally {
    let mut t = Tally::default();
    let mut down: BTreeSet<KeyCode> = BTreeSet::new();
    for e in events {
        match e {
            ProbeEvent::KeyDown(k) => {
                t.downs += 1;
                down.insert(*k);
            }
            ProbeEvent::KeyUp(k) => {
                t.ups += 1;
                if !down.remove(k) {
                    t.unmatched_ups += 1;
                }
            }
            ProbeEvent::MoveRel { .. } => t.motion += 1,
            ProbeEvent::Wheel(_) => t.wheel += 1,
        }
    }
    t.held = down.into_iter().collect();
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::ProbeEvent;
    use pheme_proto::KeyCode;

    fn k(name: &str) -> KeyCode {
        pheme_input::keymap::key_by_name(name).expect("a key the table knows")
    }

    /// Row S4's criterion is "nothing is left held". The production change
    /// that breaks this: compute `held` from a count comparison
    /// (`downs != ups`) instead of tracking which keys are down. That passes
    /// a stream of 2 downs and 2 ups for *different* keys where one is still
    /// held, which is precisely the stuck-key state S4 exists to catch.
    #[test]
    fn a_key_still_down_at_the_end_is_reported_held() {
        let t = tally(&[
            ProbeEvent::KeyDown(k("a")),
            ProbeEvent::KeyUp(k("a")),
            ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT),
        ]);
        assert_eq!(t.downs, 2);
        assert_eq!(t.ups, 1);
        assert_eq!(t.held, vec![KeyCode::LEFT_SHIFT]);
    }

    /// The balanced case, which must report nothing held even though the
    /// events interleave across two keys.
    #[test]
    fn interleaved_but_balanced_keys_leave_nothing_held() {
        let t = tally(&[
            ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT),
            ProbeEvent::KeyDown(k("a")),
            ProbeEvent::KeyUp(KeyCode::LEFT_SHIFT),
            ProbeEvent::KeyUp(k("a")),
        ]);
        assert!(t.held.is_empty(), "held was {:?}", t.held);
    }

    /// A key-up with no matching key-down is not normal: it means the recorder
    /// started mid-gesture and the row's counts cannot be trusted. The
    /// production change that breaks this: ignore the unmatched up, which
    /// would let a row report a clean tally from a partial recording.
    #[test]
    fn an_unmatched_key_up_is_counted_as_an_anomaly() {
        let t = tally(&[ProbeEvent::KeyUp(k("a"))]);
        assert_eq!(t.unmatched_ups, 1);
    }

    #[test]
    fn motion_and_wheel_are_counted_separately() {
        let t = tally(&[
            ProbeEvent::MoveRel { dx: 5, dy: 0 },
            ProbeEvent::MoveRel { dx: 5, dy: 0 },
            ProbeEvent::Wheel(-1),
        ]);
        assert_eq!(t.motion, 2);
        assert_eq!(t.wheel, 1);
    }
}
