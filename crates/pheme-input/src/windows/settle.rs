//! What to do with mouse moves that arrive just after a release.
//!
//! While grabbed, the cursor is clipped to a 1x1 rect at the park point. Moves the
//! system generated in that state carry an absolute position computed against the
//! park point, and some are still queued behind the hook when the release warps the
//! cursor to the return edge. Passed through, they put the cursor back in the middle
//! of the screen -- measured on a real link: at the warp target at 0 ms, at the park
//! point 50 ms later. A [`Settle`] recognises those stale moves and turns each one into
//! the same displacement from the warp target instead.

use std::time::{Duration, Instant};

/// How long after a release a move can still be stale. Generous: the queued moves are
/// applied within a few milliseconds, and the first fresh move ends the settle anyway.
pub(crate) const SETTLE_WINDOW: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Computed against the park point: swallow it and put the cursor at `to`.
    Stale { to: (i32, i32) },
    /// Not stale, and nothing after it will be: pass it through and drop the settle.
    Done,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Settle {
    park: (i32, i32),
    target: (i32, i32),
    radius: i32,
    deadline: Instant,
}

impl Settle {
    /// `radius` must leave `target` outside the square it draws around `park`, which a
    /// warp to the return edge does whenever it is under half the screen's shorter side.
    pub(crate) fn new(park: (i32, i32), target: (i32, i32), radius: i32, now: Instant) -> Settle {
        Settle {
            park,
            target,
            radius,
            deadline: now + SETTLE_WINDOW,
        }
    }

    pub(crate) fn judge(&mut self, pt: (i32, i32), now: Instant) -> Verdict {
        let (dx, dy) = (pt.0 - self.park.0, pt.1 - self.park.1);
        if now >= self.deadline || dx.abs() > self.radius || dy.abs() > self.radius {
            return Verdict::Done;
        }
        self.target = (self.target.0 + dx, self.target.1 + dy);
        Verdict::Stale { to: self.target }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARK: (i32, i32) = (1280, 720);
    const TARGET: (i32, i32) = (2558, 920);

    fn settle(now: Instant) -> Settle {
        Settle::new(PARK, TARGET, 360, now)
    }

    #[test]
    fn a_move_computed_against_the_park_point_lands_beside_the_target() {
        let now = Instant::now();
        let mut s = settle(now);
        assert_eq!(
            s.judge((1279, 720), now),
            Verdict::Stale { to: (2557, 920) }
        );
    }

    #[test]
    fn stale_moves_accumulate_from_the_target() {
        let now = Instant::now();
        let mut s = settle(now);
        assert_eq!(
            s.judge((1278, 721), now),
            Verdict::Stale { to: (2556, 921) }
        );
        assert_eq!(
            s.judge((1277, 720), now),
            Verdict::Stale { to: (2553, 921) }
        );
    }

    #[test]
    fn a_move_far_from_the_park_point_is_already_relative_to_the_target() {
        let now = Instant::now();
        let mut s = settle(now);
        assert_eq!(s.judge((2556, 920), now), Verdict::Done);
    }

    #[test]
    fn nothing_is_stale_after_the_window() {
        let now = Instant::now();
        let mut s = settle(now);
        assert_eq!(s.judge((1279, 720), now + SETTLE_WINDOW), Verdict::Done);
    }

    #[test]
    fn the_radius_bounds_what_counts_as_near_the_park_point() {
        let now = Instant::now();
        let mut s = settle(now);
        assert_eq!(
            s.judge((1280 - 360, 720 + 360), now),
            Verdict::Stale {
                to: (2558 - 360, 920 + 360)
            }
        );
        let mut s = settle(now);
        assert_eq!(s.judge((1280 - 361, 720), now), Verdict::Done);
    }
}
