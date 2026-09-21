//! Reconnect backoff: 0.5 s doubling to 5 s, reset after a long-lived connection.

use std::time::Duration;

const INITIAL: Duration = Duration::from_millis(500);
const MAX: Duration = Duration::from_millis(5000);
const STABLE_AFTER: Duration = Duration::from_secs(10);

pub struct Backoff {
    current: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Backoff {
        Backoff { current: INITIAL }
    }

    /// Returns the delay to wait now and advances to the next step.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(MAX);
        d
    }

    pub fn reset(&mut self) {
        self.current = INITIAL;
    }

    pub fn note_connected_for(&mut self, d: Duration) {
        if d >= STABLE_AFTER {
            self.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_and_caps_then_resets_after_long_connection() {
        let mut b = Backoff::new();
        assert_eq!(b.next(), Duration::from_millis(500));
        assert_eq!(b.next(), Duration::from_millis(1000));
        assert_eq!(b.next(), Duration::from_millis(2000));
        assert_eq!(b.next(), Duration::from_millis(4000));
        assert_eq!(b.next(), Duration::from_millis(5000));
        assert_eq!(b.next(), Duration::from_millis(5000));
        b.note_connected_for(Duration::from_secs(3));
        assert_eq!(
            b.next(),
            Duration::from_millis(5000),
            "short connection does not reset"
        );
        b.note_connected_for(Duration::from_secs(10));
        assert_eq!(b.next(), Duration::from_millis(500));
    }
}
