//! Just enough EDID to name a monitor.
//!
//! The `edid` crate would do this, and would pull `nom 3.2.1` into the
//! build -- code cargo already reports as "will be rejected by a future
//! version of Rust". The two fields needed sit at fixed offsets, so this
//! reads them directly. Layout is EDID 1.3/1.4, which has not moved since
//! 2006.

/// Offsets of the four 18-byte descriptor blocks.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];
/// Descriptor tag for the monitor name.
const TAG_NAME: u8 = 0xFC;
/// Descriptor tag for the serial number.
const TAG_SERIAL: u8 = 0xFF;

/// A display name built from a raw EDID base block: the manufacturer id
/// (bytes 8-9), the descriptor tagged 0xFC (monitor name) and the one
/// tagged 0xFF (serial number).
///
/// `None` when the block is shorter than 128 bytes, the 8-byte header is
/// not `00 FF FF FF FF FF FF 00`, or the bytes do not sum to 0 mod 256.
pub fn identity_from_edid(edid: &[u8]) -> Option<String> {
    let block = edid.get(..128)?;
    if block[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    if block.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return None;
    }
    let mut out = manufacturer(block[8], block[9]);
    if let Some(name) = descriptor_text(block, TAG_NAME) {
        out.push(' ');
        out.push_str(&name);
    }
    if let Some(serial) = descriptor_text(block, TAG_SERIAL) {
        out.push_str(" (");
        out.push_str(&serial);
        out.push(')');
    }
    Some(out)
}

/// Bytes 8-9 are three 5-bit letters, big-endian, with `A` = 1.
fn manufacturer(hi: u8, lo: u8) -> String {
    let v = u16::from_be_bytes([hi, lo]);
    (0..3)
        .map(|i| {
            let five = ((v >> (10 - 5 * i)) & 0x1F) as u8;
            // 0 is not a letter; anything out of range becomes '?' rather
            // than a control character, because this string is printed.
            if (1..=26).contains(&five) {
                (b'A' + five - 1) as char
            } else {
                '?'
            }
        })
        .collect()
}

/// The text of the display descriptor carrying `tag`, or `None`.
///
/// A display descriptor has three leading zero bytes; byte 3 is the tag and
/// bytes 5..18 the text, ended by 0x0A and padded with 0x20. Bytes outside
/// printable ASCII are dropped rather than replaced: the result is printed
/// by `pheme displays` and substring-matched against `display.monitor`, and
/// neither wants a control character in it.
fn descriptor_text(block: &[u8], tag: u8) -> Option<String> {
    for off in DESCRIPTORS {
        let d = &block[off..off + 18];
        if d[0] != 0 || d[1] != 0 || d[2] != 0 || d[3] != tag {
            continue;
        }
        let text: String = d[5..18]
            .iter()
            .take_while(|b| **b != 0x0A)
            .filter(|b| (0x20..0x7F).contains(*b))
            .map(|b| *b as char)
            .collect();
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a valid 128-byte EDID with the given display descriptors.
    /// `descriptors` is a list of `(tag, text)` placed at offsets 54, 72, 90
    /// and 108 in order; unused slots become dummy descriptors.
    fn edid_with(manufacturer: [u8; 2], descriptors: &[(u8, &[u8])]) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        e[8] = manufacturer[0];
        e[9] = manufacturer[1];
        for (i, off) in [54usize, 72, 90, 108].iter().enumerate() {
            let (tag, text) = descriptors.get(i).copied().unwrap_or((0x10, &[][..]));
            e[*off] = 0;
            e[off + 1] = 0;
            e[off + 2] = 0;
            e[off + 3] = tag;
            e[off + 4] = 0;
            for j in 0..13 {
                e[off + 5 + j] = *text.get(j).unwrap_or(&0x20);
            }
        }
        // EDID checksum: the 128 bytes must sum to 0 mod 256.
        let sum: u8 = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        e[127] = 0u8.wrapping_sub(sum);
        e
    }

    /// Break it by dropping the serial branch: the identity loses
    /// " (106NTMXE1579)".
    #[test]
    fn a_name_and_a_serial_become_one_identity() {
        let e = edid_with(
            // "GSM" -> 0b00111_10011_01101 -> 0x1E6D
            [0x1E, 0x6D],
            &[(0xFC, b"LG ULTRAGEAR\n"), (0xFF, b"106NTMXE1579\n")],
        );
        assert_eq!(
            identity_from_edid(&e).as_deref(),
            Some("GSM LG ULTRAGEAR (106NTMXE1579)")
        );
    }

    /// Break it by returning the manufacturer unconditionally instead of
    /// appending only the descriptors that exist: a trailing " ()" appears.
    #[test]
    fn a_manufacturer_alone_is_still_an_identity() {
        let e = edid_with([0x1E, 0x6D], &[]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM"));
    }

    /// Break it by deleting the header check: a block of zeroes then parses
    /// as a display named "@@@".
    #[test]
    fn a_bad_header_is_refused() {
        let mut e = edid_with([0x1E, 0x6D], &[(0xFC, b"X\n")]);
        e[1] = 0x00;
        assert_eq!(identity_from_edid(&e), None);
    }

    /// Break it by deleting the checksum check: the corrupted block parses.
    #[test]
    fn a_bad_checksum_is_refused() {
        let mut e = edid_with([0x1E, 0x6D], &[(0xFC, b"X\n")]);
        e[127] = e[127].wrapping_add(1);
        assert_eq!(identity_from_edid(&e), None);
    }

    /// Break it by indexing `edid[54..]` without a length check: this
    /// panics instead of returning None.
    #[test]
    fn a_short_block_is_refused() {
        assert_eq!(identity_from_edid(&[0x00, 0xFF]), None);
    }

    /// Review Focus 5: text that fills all 13 bytes with no 0x0A, and text
    /// carrying a byte outside printable ASCII. Break it by trimming on
    /// 0x0A alone, or by using `String::from_utf8_lossy` with no filter:
    /// the identity then carries a replacement character or a control byte
    /// into a string that gets printed and substring-matched.
    #[test]
    fn descriptor_text_is_trimmed_and_kept_printable() {
        let e = edid_with([0x1E, 0x6D], &[(0xFC, b"ABCDEFGHIJKLM")]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM ABCDEFGHIJKLM"));

        let e = edid_with([0x1E, 0x6D], &[(0xFC, b"AB\x01\x7fCD\n")]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM ABCD"));
    }
}
