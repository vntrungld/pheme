//! The policy that decides whether a pointer crossing becomes a DDC/CI
//! command.

use std::time::{Duration, Instant};

/// Decides whether a request to select a monitor input becomes a command.
///
/// Pure: no I/O, and no clock of its own -- `Instant` is passed in -- so
/// every rule is a unit test that needs no monitor. The same shape as
/// `pheme_clip::ClipSync`, for the same reason.
///
/// Three rules:
///
/// 1. Never command the input already selected.
/// 2. Never command twice inside the cooldown. Monitors take one to three
///    seconds to switch and re-sync, and a command arriving mid-switch is at
///    best ignored.
/// 3. A request made during the cooldown is *held*, not dropped. The most
///    recent one wins, and it is discarded if by the time it comes due it
///    equals what is selected.
///
/// Rule 3 has to be built this way. A leading-edge throttle -- the obvious
/// implementation -- drops the second request, so brushing the edge and
/// coming straight back would leave the monitor showing the machine the
/// pointer is no longer on, permanently.
#[derive(Debug)]
pub struct DisplaySwitch {
    cooldown: Duration,
    /// The input the monitor is believed to be showing. `None` means
    /// unknown, which is also what a failed command leaves behind.
    selected: Option<u16>,
    /// When the last command was handed out.
    last_at: Option<Instant>,
    /// A value asked for during the cooldown and not yet handed out.
    pending: Option<u16>,
}

impl DisplaySwitch {
    pub fn new(cooldown: Duration) -> Self {
        DisplaySwitch {
            cooldown,
            selected: None,
            last_at: None,
            pending: None,
        }
    }

    /// Seed `selected` from a successful read, so the first crossing to the
    /// input already showing costs no command.
    pub fn observe(&mut self, value: u16) {
        self.selected = Some(value);
    }

    /// A crossing asks for `value`. `Some(v)` means issue it now.
    pub fn request(&mut self, value: u16, now: Instant) -> Option<u16> {
        // Rule 1. Skipped while something is pending, so that asking for the
        // selected input can *cancel* a held request rather than being
        // ignored beside it: `poll` discards it on the same comparison.
        if self.pending.is_none() && self.selected == Some(value) {
            return None;
        }
        // Rules 2 and 3.
        if self.in_cooldown(now) {
            self.pending = Some(value);
            return None;
        }
        Some(self.issue(value, now))
    }

    /// The recovery hotkey (design §8): ignores rules 1 and 2, because its
    /// whole purpose is to correct a monitor whose state the policy has
    /// wrong.
    pub fn force(&mut self, value: u16, now: Instant) -> u16 {
        self.issue(value, now)
    }

    /// Hand out a held request that has come due.
    pub fn poll(&mut self, now: Instant) -> Option<u16> {
        let v = self.pending?;
        if self.in_cooldown(now) {
            return None;
        }
        self.pending = None;
        if self.selected == Some(v) {
            // The state converged while the request was held -- the pointer
            // left and came back -- so there is nothing to command.
            return None;
        }
        Some(self.issue(v, now))
    }

    /// When `poll` could next return something, or `None` when nothing is
    /// held.
    ///
    /// `checked_add` rather than `+`: `Instant + Duration` panics on
    /// overflow, and `cooldown` comes from a config file where `cooldown_ms`
    /// is a `u64` a person can set to anything. An overflowing deadline
    /// reads as "never", which is what that configuration asked for.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending?;
        self.last_at?.checked_add(self.cooldown)
    }

    /// The command the policy handed out reached the monitor.
    pub fn confirm(&mut self, value: u16) {
        self.selected = Some(value);
    }

    /// It did not. `selected` becomes unknown so rule 1 cannot refuse the
    /// retry.
    ///
    /// Sub-project 5 shipped this defect in `ClipSync`, which recorded text
    /// before the write carrying it succeeded and so refused every retry of
    /// the same text forever. Here it would be worse: `selected` would name
    /// an input the monitor is not showing, and rule 1 would refuse to
    /// correct it.
    pub fn forget(&mut self) {
        self.selected = None;
    }

    fn issue(&mut self, value: u16, now: Instant) -> u16 {
        self.last_at = Some(now);
        self.pending = None;
        value
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        match self.last_at {
            // `saturating_duration_since` rather than subtraction: a caller
            // passing a stale `Instant` should get "not in cooldown", never a
            // panic. A zero cooldown is never "in" it, which is what makes
            // `cooldown_ms = 0` mean "switch every time".
            Some(t) => now.saturating_duration_since(t) < self.cooldown,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const COOLDOWN: Duration = Duration::from_millis(1000);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Rule 1. Break it by deleting the `self.selected == Some(value)` arm
    /// of `request`: the second call then returns `Some(0x11)`.
    #[test]
    fn a_repeat_of_the_selected_input_is_refused() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x11, at(t0, 5_000)), None);
    }

    /// Rule 2. Break it by making `in_cooldown` return `false` always: the
    /// second request then issues immediately instead of being held.
    #[test]
    fn a_second_request_inside_the_cooldown_is_held_not_issued() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, at(t0, 100)), None);
        assert_eq!(s.poll(at(t0, 100)), None);
        assert_eq!(s.poll(at(t0, 1_001)), Some(0x0f));
    }

    /// Rule 3, the part a leading-edge throttle gets wrong. Break it by
    /// making `request` drop the value instead of assigning `self.pending`:
    /// the final `poll` then returns `None` and the monitor stays on the
    /// machine the pointer left.
    #[test]
    fn the_held_value_is_the_most_recent_one() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, at(t0, 100)), None);
        assert_eq!(s.request(0x12, at(t0, 200)), None);
        assert_eq!(s.poll(at(t0, 1_001)), Some(0x12));
    }

    /// Rule 3's tail: brushing the edge and ending where you started costs
    /// no command. Break it by deleting the `self.selected == Some(v)`
    /// check in `poll`: it then returns `Some(0x0f)` and re-commands the
    /// input the monitor is already showing, mid-switch.
    ///
    /// Note the two held requests. A version of this test that lets rule 1
    /// answer the second call never sets `pending` at all, so `poll` returns
    /// `None` because there is nothing held -- and the assertion passes with
    /// the discard clause deleted.
    #[test]
    fn a_held_value_that_matches_the_selection_is_discarded() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x0f, t0), Some(0x0f));
        s.confirm(0x0f);
        // Brushed back to the server, then straight out to the client
        // again, both inside the cooldown.
        assert_eq!(s.request(0x11, at(t0, 100)), None);
        assert_eq!(s.request(0x0f, at(t0, 200)), None);
        assert_eq!(s.poll(at(t0, 1_001)), None);
    }

    /// The `ClipSync` defect, in its display form. Break it by making
    /// `forget` a no-op: the retry is then refused forever and the monitor
    /// can never be corrected.
    #[test]
    fn forget_lets_an_identical_retry_through() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x11, at(t0, 2_000)), None);
        s.forget();
        assert_eq!(s.request(0x11, at(t0, 4_000)), Some(0x11));
    }

    /// Break it by routing `force` through `request`: it then returns
    /// `None` on the deduplicated value and the recovery path dies.
    #[test]
    fn force_ignores_both_the_dedupe_and_the_cooldown() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.force(0x11, at(t0, 10)), 0x11);
    }

    /// Review Focus 2. Break it by making `in_cooldown` return `true` when
    /// `cooldown` is zero: the second request is then held with a deadline
    /// already in the past, which the service thread re-arms forever.
    #[test]
    fn a_zero_cooldown_holds_nothing() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(Duration::ZERO);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, t0), Some(0x0f));
        assert_eq!(s.deadline(), None);
    }

    /// Review Focus 3. `cooldown_ms` is a `u64` a person can set to
    /// anything, and `Instant + Duration` panics on overflow. Break it by
    /// writing `t + self.cooldown` in `deadline`.
    ///
    /// Two magnitudes, because they take different branches. The largest
    /// value the config can actually produce -- `from_millis(u64::MAX)`,
    /// about 584 million years -- is still well inside `Instant`'s range on
    /// both Linux and Windows, so the deadline is real and simply never
    /// arrives. `Duration::MAX` is outside it, and that is the branch
    /// `checked_add` exists for.
    #[test]
    fn an_enormous_cooldown_does_not_panic() {
        for (cooldown, deadline_exists) in [
            (Duration::from_millis(u64::MAX), true),
            (Duration::MAX, false),
        ] {
            let t0 = Instant::now();
            let mut s = DisplaySwitch::new(cooldown);
            assert_eq!(s.request(0x11, t0), Some(0x11));
            s.confirm(0x11);
            assert_eq!(s.request(0x0f, at(t0, 1)), None);
            assert_eq!(s.deadline().is_some(), deadline_exists, "{cooldown:?}");
        }
    }
}
