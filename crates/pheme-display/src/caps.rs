//! The input values a monitor claims to accept.
//!
//! A DDC/CI capability string looks like
//! `(prot(monitor)type(LCD)vcp(02 10 60(0F 11 12) AC)mccs_ver(2.1))`. The
//! `vcp(...)` section lists feature codes, and a code followed by
//! parentheses lists the values it accepts. Feature 60 is Input Select.
//!
//! Advisory only: plenty of monitors return nothing, or a list that omits
//! inputs they do accept. `pheme displays` prints it as a hint beside the
//! input actually selected, never as the truth.

use crate::INPUT_SELECT;

/// The values listed for VCP feature 0x60 inside the `vcp(...)` section, or
/// an empty vector when the string has no such list.
pub fn input_values_from_caps(caps: &str) -> Vec<u16> {
    let Some(vcp) = section(caps, "vcp(") else {
        return Vec::new();
    };
    let needle = format!("{INPUT_SELECT:02X}(");
    let start = match find_ignore_case(vcp, &needle) {
        Some(i) => i + needle.len(),
        None => return Vec::new(),
    };
    // A list with no closing parenthesis is a truncated string, not a list
    // that runs to the end: reading on would take values out of whatever
    // text follows.
    let Some(end) = vcp[start..].find(')') else {
        return Vec::new();
    };
    vcp[start..start + end]
        .split_whitespace()
        .filter_map(|t| u16::from_str_radix(t, 16).ok())
        .collect()
}

/// The text between `open` and its matching close parenthesis.
fn section<'a>(caps: &'a str, open: &str) -> Option<&'a str> {
    let start = find_ignore_case(caps, open)? + open.len();
    let mut depth = 1usize;
    for (i, c) in caps[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&caps[start..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_uppercase()
        .find(&needle.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Break it by dropping the `section` call and scanning the whole
    /// string for "60(": `model(A60(1))` then yields `[0x01]`.
    ///
    /// That token is the whole point of this input and it has to carry a
    /// real "60(". A model number like `model(X60)` would not: it contains
    /// "60" but not "60(", so a naive scan would skip past it, land on the
    /// genuine list and return the right answer for the wrong reason.
    #[test]
    fn the_input_list_comes_from_the_vcp_section() {
        let caps = "(prot(monitor)type(LCD)model(A60(1))cmds(01 02 03)\
                    vcp(02 10 12 14(05 08) 60(0F 11 12) AC)mccs_ver(2.1))";
        assert_eq!(input_values_from_caps(caps), vec![0x0F, 0x11, 0x12]);
    }

    /// Break it by unwrapping the result of the `60(` search: a monitor
    /// whose capability string lists no input feature then panics
    /// `pheme displays`.
    #[test]
    fn a_string_without_the_input_feature_yields_nothing() {
        let caps = "(prot(monitor)type(LCD)vcp(02 10 12)mccs_ver(2.1))";
        assert!(input_values_from_caps(caps).is_empty());
    }

    /// Defence in depth, and said plainly rather than claimed as one line:
    /// a truncated string is stopped by `section`'s balance scan, and if
    /// that scan were made to return the remainder instead of `None`, the
    /// `find(')')` guard would stop it again. Either guard alone holds the
    /// line; the values only leak out of the truncation if both go.
    #[test]
    fn an_unbalanced_capability_string_yields_nothing() {
        let caps = "(prot(monitor)vcp(02 60(0F 11";
        assert!(input_values_from_caps(caps).is_empty());
    }

    /// Break it by replacing the `filter_map(..ok())` with
    /// `map(..unwrap())`: one junk token in a monitor's own capability
    /// string then panics `pheme displays` instead of being skipped.
    #[test]
    fn a_token_that_is_not_hexadecimal_is_skipped() {
        assert_eq!(
            input_values_from_caps("vcp(60(0F ZZ 11))"),
            vec![0x0F, 0x11]
        );
    }

    /// Break it by parsing decimal: 0x11 would come back as 11.
    #[test]
    fn values_are_hexadecimal() {
        assert_eq!(input_values_from_caps("vcp(60(10 11))"), vec![0x10, 0x11]);
    }
}
