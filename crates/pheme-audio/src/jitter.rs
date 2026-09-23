//! Absorbs network jitter, hides loss, and keeps the added latency as small as the link
//! allows.

use std::collections::BTreeMap;

use crate::frame::{bytes_to_samples, Frame};
use crate::{FRAME_BYTES, FRAME_INTERLEAVED};

/// Smallest buffer depth, in frames. 2 frames is 10 ms.
pub const TARGET_MIN: usize = 2;
/// Largest buffer depth, in frames. 8 frames is 40 ms.
pub const TARGET_MAX: usize = 8;
/// A sequence gap this large, in either direction, means the stream restarted rather
/// than lost packets. 200 frames is 1 s.
pub const RESET_GAP: u32 = 200;
/// Frames of concealment before the output goes silent.
pub const CONCEAL_FRAMES: u32 = 4;
/// Consecutive clean pops that lower the target by one frame. 2000 pops is 10 s.
pub const SHRINK_AFTER_POPS: u64 = 2_000;
/// Hard ceiling on buffered depth, in frames. 24 frames is 120 ms.
///
/// The adaptive target and the drift controller between them handle a sender and a
/// playback device whose clocks differ by up to 0.1 %, which is ten times the drift of
/// a real crystal. Nothing else bounds the depth, though: if the consumer is
/// persistently slower than that — a device that reports a rate it does not keep, a
/// broken driver, an emulated sound card — the buffer grows without limit, latency
/// grows with it, and every error counter stays at zero, so nothing in the logs says
/// why the audio is drifting further and further behind. This ceiling turns that
/// silent unbounded growth into a bounded skip and a counter someone can see.
pub const MAX_DEPTH: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pop {
    /// A frame that really arrived.
    Data(Vec<i16>),
    /// A faded copy of the last real frame, standing in for a lost one.
    Conceal(Vec<i16>),
    /// Nothing to play: prefilling, the sender is suppressing silence, or concealment
    /// is exhausted. The caller writes silence.
    Idle,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    pub depth: usize,
    /// Depth measured immediately before the last `pop` removed its frame.
    ///
    /// This is the number the latency budget counts: the audio waiting to be played.
    /// `depth` is sampled after the removal and therefore reads one frame lower, which
    /// is fine for watching the buffer's trend and useless as a lower bound.
    pub depth_prepop: usize,
    pub target: usize,
    pub lost: u64,
    pub late: u64,
    pub dup: u64,
    pub underruns: u64,
    pub resets: u64,
    pub malformed: u64,
    /// Times the buffer hit `MAX_DEPTH` and had to discard its backlog.
    pub overflows: u64,
}

/// Reorders incoming frames, conceals losses, and adapts its depth to the link.
///
/// `push` is called by whatever receives datagrams; `pop` is called once per output
/// frame by the playback worker. The two run at the same average rate — the sender's
/// clock and the playback device's clock — and `DriftController` keeps them there.
pub struct JitterBuffer {
    /// Undelivered payloads by sequence number. Near the `u32` wraparound (about 248
    /// days of continuous audio) the ordering is briefly wrong for a couple of frames;
    /// that is cheaper to accept than to defend against.
    frames: BTreeMap<u32, Vec<u8>>,
    next: Option<u32>,
    target: usize,
    prefilling: bool,
    last_frame: Option<Vec<i16>>,
    last_silent: bool,
    conceal_run: u32,
    clean_pops: u64,
    stats: JitterStats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        JitterBuffer::new()
    }
}

impl JitterBuffer {
    pub fn new() -> JitterBuffer {
        JitterBuffer {
            frames: BTreeMap::new(),
            next: None,
            target: TARGET_MIN,
            prefilling: true,
            last_frame: None,
            last_silent: false,
            conceal_run: 0,
            clean_pops: 0,
            stats: JitterStats {
                target: TARGET_MIN,
                ..JitterStats::default()
            },
        }
    }

    pub fn push(&mut self, f: Frame) {
        if f.bytes.len() != FRAME_BYTES {
            self.stats.malformed += 1;
            return;
        }
        if let Some(next) = self.next {
            let ahead = f.seq.wrapping_sub(next);
            if ahead > u32::MAX / 2 {
                // Behind the read cursor: either a frame whose slot has passed, or a
                // sender that restarted its numbering.
                if next.wrapping_sub(f.seq) > RESET_GAP {
                    self.reset();
                } else {
                    self.stats.late += 1;
                    return;
                }
            } else if ahead > RESET_GAP {
                self.reset();
            }
        }
        if self.frames.contains_key(&f.seq) {
            self.stats.dup += 1;
            return;
        }
        self.frames.insert(f.seq, f.bytes);
        self.stats.depth = self.frames.len();
        self.trim();
    }

    /// Discards the backlog when the buffer has outgrown `MAX_DEPTH`.
    ///
    /// The read cursor moves to the new front, so the frames thrown away are not then
    /// counted as loss on the way past — they were dropped deliberately, and `overflows`
    /// is the counter that says so. `last_frame` is cleared because the new front does
    /// not continue the audio that was playing, so concealing from it would splice two
    /// unrelated moments together.
    fn trim(&mut self) {
        if self.frames.len() <= MAX_DEPTH {
            return;
        }
        while self.frames.len() > self.target {
            let Some(oldest) = self.frames.keys().next().copied() else {
                break;
            };
            self.frames.remove(&oldest);
        }
        self.next = self.frames.keys().next().copied();
        self.last_frame = None;
        self.last_silent = false;
        self.conceal_run = 0;
        self.stats.overflows += 1;
        self.stats.depth = self.frames.len();
    }

    pub fn pop(&mut self) -> Pop {
        self.stats.depth_prepop = self.frames.len();
        if self.prefilling {
            if self.frames.len() < self.target {
                self.stats.depth = self.frames.len();
                return Pop::Idle;
            }
            self.prefilling = false;
            self.next = self.frames.keys().next().copied();
        }
        let Some(next) = self.next else {
            return Pop::Idle;
        };
        self.next = Some(next.wrapping_add(1));

        let Some(bytes) = self.frames.remove(&next) else {
            return self.miss();
        };
        let mut samples = Vec::with_capacity(FRAME_INTERLEAVED);
        if bytes_to_samples(&bytes, &mut samples).is_err() {
            // `push` rejects anything that is not exactly one frame, so this is
            // unreachable; count it rather than panic if that ever stops being true.
            self.stats.malformed += 1;
            return Pop::Idle;
        }
        self.last_silent = samples.iter().all(|s| *s == 0);
        self.last_frame = Some(samples.clone());
        self.conceal_run = 0;
        self.clean_pops += 1;
        if self.clean_pops >= SHRINK_AFTER_POPS {
            self.clean_pops = 0;
            self.target = self.target.saturating_sub(1).max(TARGET_MIN);
        }
        self.stats.depth = self.frames.len();
        self.stats.target = self.target;
        Pop::Data(samples)
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats {
            depth: self.frames.len(),
            target: self.target,
            ..self.stats
        }
    }

    /// The next frame is not here.
    fn miss(&mut self) -> Pop {
        self.stats.depth = self.frames.len();
        let Some(last) = self.last_frame.as_ref() else {
            return Pop::Idle;
        };
        if self.last_silent {
            // The sender is inside its silence-suppression window: it deliberately sent
            // nothing. Counting this as loss would ratchet the target to 40 ms every
            // time the user pauses their music. Real loss during silence is ignored
            // too, which costs nothing — the concealed content would have been silence.
            return Pop::Idle;
        }
        self.stats.lost += 1;
        self.conceal_run += 1;
        if self.conceal_run == 1 {
            self.stats.underruns += 1;
            self.clean_pops = 0;
            self.target = (self.target + 1).min(TARGET_MAX);
            self.stats.target = self.target;
        }
        if self.conceal_run > CONCEAL_FRAMES {
            return Pop::Idle;
        }
        let gain = 1.0 - 0.25 * (self.conceal_run as f32 - 1.0);
        Pop::Conceal(
            last.iter()
                .map(|s| (f32::from(*s) * gain).round().clamp(-32768.0, 32767.0) as i16)
                .collect(),
        )
    }

    /// Drops everything and prefills again. The target is deliberately kept: it
    /// describes the link, which a stream restart does not change.
    fn reset(&mut self) {
        self.frames.clear();
        self.next = None;
        self.prefilling = true;
        self.last_frame = None;
        self.last_silent = false;
        self.conceal_run = 0;
        self.clean_pops = 0;
        self.stats.resets += 1;
        self.stats.depth = 0;
        self.stats.depth_prepop = 0;
    }

    /// Drops everything buffered and prefills again, on the caller's say-so rather than
    /// on a sequence-number gap.
    ///
    /// The client calls this at the moment it asks the server to reopen its microphone.
    /// While the microphone was shut, the sender's numbering stood still and this
    /// buffer's read cursor kept advancing, so the resuming stream arrives *behind* the
    /// cursor. If that distance is under `RESET_GAP` nothing resets on its own and every
    /// arriving frame is counted late and discarded — up to 750 ms of silence at the
    /// start of every recording, with every error counter reading zero. The client is
    /// the one component that knows exactly when the stream is resuming, so it says so
    /// instead of leaving the buffer to infer it.
    ///
    /// Counted in `resets`, because that is what it is: one per recording is honest.
    ///
    /// Unlike the gap-triggered `reset`, this also drops the adaptive target back to
    /// `TARGET_MIN`. `reset`'s target is deliberately kept because it describes a link
    /// that is still the same conversation; `restart` is a new recording, so a target
    /// this buffer grew while concealing the previous recording's tail-off is not a
    /// property of the link the new stream will face, and carrying it over would delay
    /// the very first frames of the new stream behind a stale, inflated prefill target.
    ///
    /// Call this exactly once per resume — when the microphone is asked to reopen — and
    /// never on every tick or poll while it stays closed or idle. Nothing here guards
    /// against repeated calls: each one unconditionally re-pins the target to
    /// `TARGET_MIN`, so calling it more often than that silently disables the adaptive
    /// ratchet in `miss()` for good, with no counter that flags it as a problem —
    /// `stats.resets` climbs the same way one honest restart per recording would.
    pub fn restart(&mut self) {
        self.reset();
        self.target = TARGET_MIN;
        self.stats.target = TARGET_MIN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::samples_to_bytes;
    use crate::{FRAME_INTERLEAVED, FRAME_US};

    /// A frame whose every sample is `v`, so a test can identify it from one sample.
    fn frame(seq: u32, v: i16) -> Frame {
        let mut bytes = Vec::new();
        samples_to_bytes(&vec![v; FRAME_INTERLEAVED], &mut bytes);
        Frame {
            seq,
            ts_us: u64::from(seq) * FRAME_US,
            bytes,
        }
    }

    fn data(p: Pop) -> i16 {
        match p {
            Pop::Data(s) => s[0],
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn nothing_plays_until_the_buffer_is_prefilled() {
        let mut jb = JitterBuffer::new();
        assert_eq!(jb.pop(), Pop::Idle, "empty");
        jb.push(frame(0, 10));
        assert_eq!(jb.pop(), Pop::Idle, "one frame is below the target of two");
        jb.push(frame(1, 11));
        assert_eq!(data(jb.pop()), 10);
        assert_eq!(data(jb.pop()), 11);
    }

    #[test]
    fn a_reordered_frame_is_still_delivered() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 10));
        jb.push(frame(1, 11));
        assert_eq!(data(jb.pop()), 10);
        jb.push(frame(3, 13));
        jb.push(frame(2, 12)); // arrives after its successor
        assert_eq!(data(jb.pop()), 11);
        assert_eq!(data(jb.pop()), 12);
        assert_eq!(data(jb.pop()), 13);
        let st = jb.stats();
        assert_eq!((st.late, st.lost), (0, 0));
    }

    #[test]
    fn a_single_loss_is_concealed_with_a_full_gain_copy() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 1000));
        jb.push(frame(1, 1000));
        assert_eq!(data(jb.pop()), 1000);
        assert_eq!(data(jb.pop()), 1000);
        jb.push(frame(3, 2000)); // 2 never arrives
        match jb.pop() {
            Pop::Conceal(s) => assert_eq!(s[0], 1000),
            other => panic!("expected Conceal, got {other:?}"),
        }
        assert_eq!(data(jb.pop()), 2000);
        let st = jb.stats();
        assert_eq!((st.lost, st.underruns), (1, 1));
    }

    #[test]
    fn a_long_loss_fades_to_silence() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 4000));
        jb.push(frame(1, 4000));
        jb.pop();
        jb.pop();
        let mut heard = Vec::new();
        for _ in 0..6 {
            heard.push(match jb.pop() {
                Pop::Conceal(s) => s[0],
                Pop::Idle => 0,
                other => panic!("expected Conceal or Idle, got {other:?}"),
            });
        }
        assert_eq!(heard, vec![4000, 3000, 2000, 1000, 0, 0]);
        let st = jb.stats();
        assert_eq!(st.lost, 6, "every missing frame is lost");
        assert_eq!(st.underruns, 1, "but one underrun per gap, not per frame");
    }

    #[test]
    fn a_duplicate_is_counted_and_ignored() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 1));
        jb.push(frame(1, 1));
        jb.push(frame(1, 2));
        assert_eq!(jb.stats().dup, 1);
        assert_eq!(data(jb.pop()), 1);
        assert_eq!(data(jb.pop()), 1, "the first copy won");
    }

    #[test]
    fn a_frame_whose_slot_has_passed_is_counted_late() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 1));
        jb.push(frame(1, 1));
        jb.push(frame(2, 1));
        jb.pop();
        jb.pop();
        jb.pop();
        jb.push(frame(2, 9));
        assert_eq!(jb.stats().late, 1);
    }

    #[test]
    fn a_huge_gap_resets_and_reprefills() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 5));
        jb.push(frame(1, 5));
        assert_eq!(data(jb.pop()), 5);
        jb.push(frame(500, 9));
        assert_eq!(jb.stats().resets, 1);
        assert_eq!(jb.pop(), Pop::Idle, "prefilling again");
        jb.push(frame(501, 9));
        assert_eq!(data(jb.pop()), 9);
        assert_eq!(data(jb.pop()), 9);
    }

    #[test]
    fn a_sender_that_restarts_its_numbering_resets_the_buffer() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(1000, 5));
        jb.push(frame(1001, 5));
        jb.pop();
        jb.pop();
        jb.push(frame(0, 9)); // far behind: a restarted sender, not a late frame
        jb.push(frame(1, 9));
        assert_eq!(jb.stats().resets, 1);
        assert_eq!(data(jb.pop()), 9);
    }

    #[test]
    fn a_gap_after_silence_is_not_loss() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 0));
        jb.push(frame(1, 0));
        jb.pop();
        jb.pop();
        for _ in 0..50 {
            assert_eq!(jb.pop(), Pop::Idle, "the sender is suppressing silence");
        }
        let st = jb.stats();
        assert_eq!((st.lost, st.underruns), (0, 0));
        assert_eq!(
            st.target, TARGET_MIN,
            "a pause must never ratchet the latency up"
        );
    }

    #[test]
    fn the_target_grows_once_per_gap_and_is_capped() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 7));
        jb.push(frame(1, 7));
        jb.pop();
        jb.pop();
        assert_eq!(jb.stats().target, TARGET_MIN);
        let mut seq = 2;
        for step in 0..10 {
            seq += 1; // the frame at `seq - 1` never arrives
            jb.push(frame(seq, 7));
            assert!(matches!(jb.pop(), Pop::Conceal(_)), "step {step}: the gap");
            assert!(matches!(jb.pop(), Pop::Data(_)), "step {step}: recovery");
            seq += 1;
        }
        assert_eq!(jb.stats().underruns, 10);
        assert_eq!(jb.stats().target, TARGET_MAX);
    }

    #[test]
    fn the_target_shrinks_after_a_long_clean_run() {
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 7));
        jb.push(frame(1, 7));
        jb.pop();
        jb.pop();
        jb.push(frame(3, 7));
        assert!(matches!(jb.pop(), Pop::Conceal(_)));
        assert!(matches!(jb.pop(), Pop::Data(_)));
        assert_eq!(jb.stats().target, TARGET_MIN + 1);
        for seq in 4..4 + SHRINK_AFTER_POPS as u32 {
            jb.push(frame(seq, 7));
            assert!(matches!(jb.pop(), Pop::Data(_)));
        }
        assert_eq!(
            jb.stats().target,
            TARGET_MIN,
            "latency comes back down on a clean link"
        );
    }

    #[test]
    fn a_backlog_past_the_ceiling_is_discarded_down_to_target() {
        let mut jb = JitterBuffer::new();
        // Fill well past the ceiling without ever popping, as happens when the playback
        // device consumes persistently slower than the sender produces.
        for seq in 0..=MAX_DEPTH as u32 {
            jb.push(frame(seq, 100 + seq as i16));
        }
        let st = jb.stats();
        assert_eq!(st.overflows, 1, "one discard, not one per frame");
        assert_eq!(st.depth, TARGET_MIN, "trimmed back to the target");
        assert_eq!(
            (st.lost, st.late, st.dup, st.resets),
            (0, 0, 0, 0),
            "a deliberate discard is not loss, lateness, duplication or a reset"
        );
    }

    #[test]
    fn the_frames_kept_after_a_discard_are_the_newest() {
        let mut jb = JitterBuffer::new();
        let last = MAX_DEPTH as u32;
        for seq in 0..=last {
            jb.push(frame(seq, 100 + seq as i16));
        }
        // TARGET_MIN frames survive, and they are the tail of the stream rather than
        // its head — playing the head would replay audio the listener has moved past.
        assert_eq!(data(jb.pop()), 100 + (last - 1) as i16);
        assert_eq!(data(jb.pop()), 100 + last as i16);
    }

    #[test]
    fn a_discard_does_not_strand_the_read_cursor() {
        let mut jb = JitterBuffer::new();
        let last = MAX_DEPTH as u32;
        for seq in 0..=last {
            jb.push(frame(seq, 7));
        }
        assert!(matches!(jb.pop(), Pop::Data(_)));
        assert!(matches!(jb.pop(), Pop::Data(_)));
        // The stream continues from where the survivors left off, with no phantom gap
        // between the discarded sequence numbers and the next real frame.
        jb.push(frame(last + 1, 9));
        assert_eq!(data(jb.pop()), 9);
        let st = jb.stats();
        assert_eq!((st.lost, st.underruns), (0, 0));
    }

    #[test]
    fn a_buffer_within_the_ceiling_is_left_alone() {
        let mut jb = JitterBuffer::new();
        for seq in 0..MAX_DEPTH as u32 {
            jb.push(frame(seq, 5));
        }
        let st = jb.stats();
        assert_eq!(st.overflows, 0);
        assert_eq!(st.depth, MAX_DEPTH, "exactly at the ceiling is not over it");
    }

    #[test]
    fn a_malformed_payload_is_dropped() {
        let mut jb = JitterBuffer::new();
        jb.push(Frame {
            seq: 0,
            ts_us: 0,
            bytes: vec![0; 10],
        });
        assert_eq!(jb.stats().malformed, 1);
        assert_eq!(jb.stats().depth, 0);
    }

    #[test]
    fn an_explicit_restart_drops_everything_and_prefills_again() {
        // What the client does when it asks the server to reopen its microphone: the
        // stream is about to resume from an unrelated sequence number, and anything still
        // buffered belongs to the previous recording.
        let mut jb = JitterBuffer::new();
        jb.push(frame(10, 5));
        jb.push(frame(11, 5));
        assert_eq!(data(jb.pop()), 5);

        jb.restart();
        assert_eq!(jb.stats().depth, 0, "the backlog is gone");
        assert_eq!(jb.pop(), Pop::Idle, "prefilling again");

        // A sender starting from zero is picked up cleanly, with no run of late frames.
        jb.push(frame(0, 9));
        jb.push(frame(1, 9));
        assert_eq!(data(jb.pop()), 9);
        assert_eq!(data(jb.pop()), 9);
        assert_eq!(
            jb.stats().late,
            0,
            "a restart must not strand the new stream"
        );
    }

    #[test]
    fn a_restart_without_it_would_discard_the_resumed_stream_as_late() {
        // The failure this exists to prevent: the read cursor advanced while the sender was
        // closed, the sender restarts below it, and the gap is under RESET_GAP so nothing
        // resets on its own. Every arriving frame is late and the listener hears silence.
        let mut jb = JitterBuffer::new();
        jb.push(frame(100, 5));
        jb.push(frame(101, 5));
        jb.pop();
        jb.pop();
        // Cursor walks forward while nothing arrives, as it does while the microphone is
        // shut and the virtual source is still pulling.
        for _ in 0..40 {
            jb.pop();
        }
        jb.push(frame(0, 9));
        jb.push(frame(1, 9));
        assert!(
            jb.stats().late > 0,
            "this is the pathology restart() exists to avoid"
        );

        // With the restart in the right place, the same sequence plays.
        let mut jb = JitterBuffer::new();
        jb.push(frame(100, 5));
        jb.push(frame(101, 5));
        jb.pop();
        jb.pop();
        for _ in 0..40 {
            jb.pop();
        }
        jb.restart();
        jb.push(frame(0, 9));
        jb.push(frame(1, 9));
        assert_eq!(data(jb.pop()), 9);
        assert_eq!(jb.stats().late, 0);
    }

    #[test]
    fn the_reported_depth_is_measured_before_the_pop_removes_its_frame() {
        // `depth_ms` in the stats line is meant to answer "how much audio is waiting", which
        // is what the latency budget counts. Sampling after the pop reads a frame lower and
        // leaves a lower bound of one frame with no headroom at all.
        let mut jb = JitterBuffer::new();
        jb.push(frame(0, 1));
        jb.push(frame(1, 1));
        jb.push(frame(2, 1));
        assert!(matches!(jb.pop(), Pop::Data(_)));
        let st = jb.stats();
        assert_eq!(st.depth, 2, "two frames remain after the pop");
        assert_eq!(
            st.depth_prepop, 3,
            "three frames were waiting when the pop happened"
        );
    }

    #[test]
    fn the_pre_pop_depth_is_zero_while_prefilling() {
        let mut jb = JitterBuffer::new();
        assert_eq!(jb.pop(), Pop::Idle);
        assert_eq!(jb.stats().depth_prepop, 0);
        jb.push(frame(0, 1));
        assert_eq!(jb.pop(), Pop::Idle, "still below target");
        assert_eq!(jb.stats().depth_prepop, 1);
    }
}
