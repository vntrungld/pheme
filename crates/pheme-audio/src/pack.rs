//! Turns a stream of samples into numbered frames, dropping long silences.

use crate::frame::{samples_to_bytes, Frame};
use crate::{FRAME_BYTES, FRAME_INTERLEAVED};

/// Frames of exact silence tolerated before the sender goes quiet. 40 frames is 200 ms.
pub const SILENCE_SUPPRESS_AFTER: u32 = 40;

/// Numbers outgoing frames and suppresses long silences.
///
/// `seq` advances on every call, including calls whose frame is suppressed, so the
/// receiver can always read a gap as "this much audio time is missing" whatever the
/// cause. Suppression saves 1.5 Mbit/s while nothing is playing; the receiver's jitter
/// buffer treats the resulting gap as silence, not loss, because the last frame it did
/// receive was silent.
pub struct Packer {
    seq: u32,
    silent_run: u32,
}

impl Default for Packer {
    fn default() -> Self {
        Packer::new()
    }
}

impl Packer {
    pub fn new() -> Packer {
        Packer {
            seq: 0,
            silent_run: 0,
        }
    }

    /// Consumes one frame's worth of interleaved samples.
    ///
    /// `samples` must be exactly `FRAME_INTERLEAVED` long. Returns `None` while the
    /// suppression window is open.
    pub fn push(&mut self, samples: &[i16], ts_us: u64) -> Option<Frame> {
        debug_assert_eq!(samples.len(), FRAME_INTERLEAVED);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);

        if samples.iter().all(|s| *s == 0) {
            self.silent_run = self.silent_run.saturating_add(1);
        } else {
            self.silent_run = 0;
        }
        if self.suppressed() {
            return None;
        }

        let mut bytes = Vec::with_capacity(FRAME_BYTES);
        samples_to_bytes(samples, &mut bytes);
        Some(Frame { seq, ts_us, bytes })
    }

    /// True while the suppression window is open.
    pub fn suppressed(&self) -> bool {
        self.silent_run > SILENCE_SUPPRESS_AFTER
    }

    /// The `seq` the next call to `push` will use.
    pub fn next_seq(&self) -> u32 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_INTERLEAVED;

    fn silence() -> Vec<i16> {
        vec![0; FRAME_INTERLEAVED]
    }

    fn tone() -> Vec<i16> {
        (0..FRAME_INTERLEAVED).map(|i| (i as i16) - 240).collect()
    }

    #[test]
    fn sequence_numbers_start_at_zero_and_increment() {
        let mut p = Packer::new();
        for expected in 0..5 {
            let f = p
                .push(&tone(), expected as u64 * 5_000)
                .expect("audible frame is sent");
            assert_eq!(f.seq, expected);
            assert_eq!(f.ts_us, expected as u64 * 5_000);
            assert_eq!(f.bytes.len(), crate::FRAME_BYTES);
        }
    }

    #[test]
    fn short_silences_are_still_sent() {
        let mut p = Packer::new();
        p.push(&tone(), 0).unwrap();
        for i in 0..SILENCE_SUPPRESS_AFTER {
            assert!(
                p.push(&silence(), 0).is_some(),
                "silent frame {i} is within the window and must still be sent"
            );
        }
        assert!(!p.suppressed());
    }

    #[test]
    fn a_long_silence_is_suppressed() {
        let mut p = Packer::new();
        p.push(&tone(), 0).unwrap();
        for _ in 0..SILENCE_SUPPRESS_AFTER {
            p.push(&silence(), 0).unwrap();
        }
        assert!(p.push(&silence(), 0).is_none(), "one past the window");
        assert!(p.suppressed());
        for _ in 0..1000 {
            assert!(p.push(&silence(), 0).is_none());
        }
    }

    #[test]
    fn resuming_audio_reports_every_skipped_frame_in_seq() {
        let mut p = Packer::new();
        let first = p.push(&tone(), 0).unwrap();
        assert_eq!(first.seq, 0);
        // 100 silent frames: the first SILENCE_SUPPRESS_AFTER are sent, the rest dropped.
        for _ in 0..100 {
            p.push(&silence(), 0);
        }
        let resumed = p
            .push(&tone(), 0)
            .expect("audible frame ends the suppression");
        assert_eq!(resumed.seq, 101, "seq counts suppressed frames too");
        assert!(!p.suppressed());
    }

    #[test]
    fn one_non_zero_sample_breaks_the_silence_run() {
        let mut p = Packer::new();
        for _ in 0..1000 {
            p.push(&silence(), 0);
        }
        assert!(p.suppressed());
        let mut almost = silence();
        almost[7] = -1;
        assert!(p.push(&almost, 0).is_some(), "not silence, however quiet");
        assert!(!p.suppressed());
        assert!(
            p.push(&silence(), 0).is_some(),
            "the run restarts from zero"
        );
    }

    #[test]
    fn the_payload_matches_the_samples() {
        let mut p = Packer::new();
        let samples = tone();
        let f = p.push(&samples, 0).unwrap();
        let mut back = Vec::new();
        crate::frame::bytes_to_samples(&f.bytes, &mut back).unwrap();
        assert_eq!(back, samples);
    }
}
