//! One 5 ms audio frame and the conversion between i16 samples and wire bytes.

use crate::{Error, Result};

/// One 5 ms frame of interleaved little-endian i16 stereo PCM.
///
/// `seq` counts every 5 ms of the sender's audio time, including frames that silence
/// suppression drops, so a gap always means that much audio time is missing. `ts_us` is
/// the sender's monotonic capture timestamp and is diagnostic only: the two machines
/// share no clock and nothing in the pipeline synchronises on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub ts_us: u64,
    pub bytes: Vec<u8>,
}

impl Frame {
    /// True when every sample is exactly zero.
    ///
    /// The test is exact rather than a threshold so quiet content is never mistaken for
    /// silence; digital silence from an idle OS mixer is exactly zero. Testing the bytes
    /// is equivalent to testing the samples and avoids decoding.
    pub fn is_silent(&self) -> bool {
        self.bytes.iter().all(|b| *b == 0)
    }
}

/// Writes interleaved i16 samples as little-endian bytes, replacing `dst`.
pub fn samples_to_bytes(src: &[i16], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len() * 2);
    for s in src {
        dst.extend_from_slice(&s.to_le_bytes());
    }
}

/// Reads little-endian bytes as interleaved i16 samples, replacing `dst`.
///
/// Fails when `src` is not a whole number of stereo sample pairs (4 bytes).
pub fn bytes_to_samples(src: &[u8], dst: &mut Vec<i16>) -> Result<()> {
    if !src.len().is_multiple_of(2 * crate::CHANNELS) {
        return Err(Error::Backend(format!(
            "audio payload of {} bytes is not a whole number of stereo samples",
            src.len()
        )));
    }
    dst.clear();
    dst.reserve(src.len() / 2);
    for c in src.chunks_exact(2) {
        dst.push(i16::from_le_bytes([c[0], c[1]]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_BYTES, FRAME_INTERLEAVED};

    #[test]
    fn samples_round_trip_through_bytes() {
        let src: Vec<i16> = vec![0, 1, -1, i16::MIN, i16::MAX, 1234, -4321, 32, 0, -2];
        let mut bytes = Vec::new();
        samples_to_bytes(&src, &mut bytes);
        assert_eq!(bytes.len(), src.len() * 2);
        let mut back = Vec::new();
        bytes_to_samples(&bytes, &mut back).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn little_endian_layout_is_pinned() {
        let mut bytes = Vec::new();
        samples_to_bytes(&[0x0102u16 as i16, -2], &mut bytes);
        assert_eq!(bytes, vec![0x02, 0x01, 0xFE, 0xFF]);
    }

    #[test]
    fn a_partial_sample_pair_is_rejected() {
        let mut back = Vec::new();
        assert!(bytes_to_samples(&[0, 0, 0], &mut back).is_err());
        assert!(
            bytes_to_samples(&[0, 0], &mut back).is_err(),
            "half a stereo pair"
        );
    }

    #[test]
    fn a_full_frame_is_960_bytes() {
        let mut bytes = Vec::new();
        samples_to_bytes(&vec![0i16; FRAME_INTERLEAVED], &mut bytes);
        assert_eq!(bytes.len(), FRAME_BYTES);
    }

    #[test]
    fn silence_is_exact_not_approximate() {
        let mut bytes = Vec::new();
        samples_to_bytes(&vec![0i16; FRAME_INTERLEAVED], &mut bytes);
        let silent = Frame {
            seq: 0,
            ts_us: 0,
            bytes,
        };
        assert!(silent.is_silent());

        let mut loud = vec![0i16; FRAME_INTERLEAVED];
        loud[FRAME_INTERLEAVED - 1] = 1;
        let mut bytes = Vec::new();
        samples_to_bytes(&loud, &mut bytes);
        let frame = Frame {
            seq: 0,
            ts_us: 0,
            bytes,
        };
        assert!(
            !frame.is_silent(),
            "a single non-zero sample is not silence"
        );
    }
}
