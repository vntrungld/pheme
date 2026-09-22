//! Keeps the jitter buffer at its target depth by nudging the playback resample ratio.

/// Ticks between ratio updates. At one tick per 5 ms frame, 20 ticks is 100 ms.
pub const TICKS_PER_UPDATE: u32 = 20;
/// Largest relative correction. 0.1 % is inaudible and drains one excess frame in about
/// five seconds.
pub const MAX_ADJ: f64 = 0.001;
/// Proportional gain on the relative depth error. The correction saturates once the
/// depth is half a target away, which is intended: outside that band, go flat out.
pub const GAIN: f64 = 0.002;
/// Weight of each new depth sample in the moving average. At one sample per 5 ms the
/// time constant is about half a second.
pub const EMA_ALPHA: f64 = 0.01;

/// Turns jitter-buffer depth into a playback resample ratio.
///
/// Feed it the depth at every popped frame; it smooths the depth, and every 100 ms it
/// recomputes the ratio to hand to the resampler. `tick` reads no clock, so the whole
/// control loop is deterministic and testable.
pub struct DriftController {
    base: f64,
    ema: Option<f64>,
    ratio: f64,
    ticks: u32,
}

impl DriftController {
    /// `base_ratio` is `device_rate / 48000`: the fixed conversion the resampler would
    /// do with no drift at all.
    pub fn new(base_ratio: f64) -> DriftController {
        DriftController {
            base: base_ratio,
            ema: None,
            ratio: base_ratio,
            ticks: 0,
        }
    }

    /// Records one popped frame and returns the ratio to use now.
    pub fn tick(&mut self, depth: usize, target: usize) -> f64 {
        let d = depth as f64;
        self.ema = Some(match self.ema {
            None => d,
            Some(e) => e * (1.0 - EMA_ALPHA) + d * EMA_ALPHA,
        });
        self.ticks += 1;
        if self.ticks >= TICKS_PER_UPDATE {
            self.ticks = 0;
            let t = target.max(1) as f64;
            let error = self.ema.unwrap_or(t) - t;
            // Negative on purpose: a deeper buffer needs a smaller ratio. See the module
            // note in the plan and the spec's drift section.
            let adj = (-GAIN * error / t).clamp(-MAX_ADJ, MAX_ADJ);
            self.ratio = self.base * (1.0 + adj);
        }
        self.ratio
    }

    /// The ratio last computed.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The smoothed buffer depth, for diagnostics.
    pub fn depth_ema(&self) -> f64 {
        self.ema.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulated minutes of playback against a sender whose clock runs `ppm` parts per
    /// million off ours. Returns the mean buffer depth over the last 10 000 frames, the
    /// final ratio, and the largest correction the controller ever asked for.
    ///
    /// One iteration is one popped frame. A frame is 240 samples *per channel*, so
    /// playing it takes `240 * ratio / 48000` seconds, during which the sender produces
    /// `200 * (1 + ppm)` frames. Drift is a slow effect — 50 ppm is only 0.6 frames per
    /// minute — so the run is 30 minutes long. No wall clock is involved, so it is
    /// deterministic and takes milliseconds.
    ///
    /// `depth` is reported to the controller rounded to whole frames, because that is
    /// what a real buffer holds. The quantisation means the loop settles into a slow
    /// cycle within one frame of the target rather than exactly on it.
    const POPS: usize = 360_000;
    const TAIL: usize = 10_000;

    fn simulate(ppm: f64, start_depth: f64, correct: bool) -> (f64, f64, f64) {
        let mut d = DriftController::new(1.0);
        let mut depth = start_depth;
        let target = 2usize;
        let mut worst = 0.0f64;
        let mut tail_sum = 0.0;
        for i in 0..POPS {
            let ratio = if correct {
                d.tick(depth.max(0.0).round() as usize, target)
            } else {
                1.0
            };
            worst = worst.max((ratio - 1.0).abs());
            let seconds = 240.0 * ratio / 48_000.0;
            depth += seconds * 200.0 * (1.0 + ppm) - 1.0;
            if i >= POPS - TAIL {
                tail_sum += depth;
            }
        }
        (tail_sum / TAIL as f64, d.ratio(), worst)
    }

    #[test]
    fn an_uncorrected_clock_difference_runs_the_buffer_away() {
        let (mean, _, _) = simulate(50e-6, 2.0, false);
        assert!(
            mean > 12.0,
            "without correction 30 minutes at 50 ppm piles up ~20 frames, got {mean}"
        );
    }

    #[test]
    fn a_fast_sender_is_absorbed_without_the_buffer_growing() {
        let (mean, ratio, worst) = simulate(50e-6, 2.0, true);
        assert!(
            (mean - 2.0).abs() < 1.0,
            "depth settled at {mean}, expected within one frame of the target of 2"
        );
        assert!(
            ratio < 1.0,
            "a fast sender must be consumed slightly faster"
        );
        assert!(
            worst <= MAX_ADJ + 1e-12,
            "correction {worst} exceeded the clamp"
        );
    }

    #[test]
    fn a_slow_sender_is_absorbed_without_the_buffer_emptying() {
        let (mean, ratio, worst) = simulate(-50e-6, 2.0, true);
        assert!(
            (mean - 2.0).abs() < 1.0,
            "depth settled at {mean}, expected within one frame of the target of 2"
        );
        assert!(
            ratio > 1.0,
            "a slow sender must be consumed slightly slower"
        );
        assert!(
            worst <= MAX_ADJ + 1e-12,
            "correction {worst} exceeded the clamp"
        );
    }

    #[test]
    fn an_overfull_buffer_is_drained_back_to_target() {
        let (mean, _, worst) = simulate(0.0, 6.0, true);
        assert!(
            (mean - 2.0).abs() < 1.0,
            "depth settled at {mean}, expected the extra frames to be drained"
        );
        assert!(worst <= MAX_ADJ + 1e-12);
    }

    #[test]
    fn the_correction_is_clamped_to_a_tenth_of_a_percent() {
        let mut d = DriftController::new(1.0);
        for _ in 0..1_000 {
            d.tick(100, 2); // absurdly deep
        }
        assert!((d.ratio() - (1.0 - MAX_ADJ)).abs() < 1e-12);
        let mut d = DriftController::new(1.0);
        for _ in 0..1_000 {
            d.tick(0, 8); // absurdly empty
        }
        assert!((d.ratio() - (1.0 + MAX_ADJ)).abs() < 1e-12);
    }

    #[test]
    fn a_non_unity_base_ratio_is_the_centre_of_the_correction() {
        let base = 44_100.0 / 48_000.0; // a device that runs at 44.1 kHz
        let mut d = DriftController::new(base);
        assert_eq!(d.ratio(), base, "before any tick");
        for _ in 0..TICKS_PER_UPDATE {
            d.tick(2, 2); // exactly on target
        }
        assert!((d.ratio() - base).abs() < 1e-12, "on target, no correction");
        for _ in 0..1_000 {
            d.tick(100, 2);
        }
        assert!((d.ratio() - base * (1.0 - MAX_ADJ)).abs() < 1e-12);
    }

    #[test]
    fn the_ratio_only_changes_on_an_update_boundary() {
        let mut d = DriftController::new(1.0);
        for _ in 0..(TICKS_PER_UPDATE - 1) {
            assert_eq!(d.tick(100, 2), 1.0, "no update yet");
        }
        assert!(
            d.tick(100, 2) < 1.0,
            "the update lands on the boundary tick"
        );
    }
}
