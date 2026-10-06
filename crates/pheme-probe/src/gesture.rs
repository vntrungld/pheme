//! Gestures a row asks for, and their expansion into individual events.
//!
//! Pure on purpose: this is the half of the probe that decides *what* to send,
//! and it is unit-tested, while the half that talks to uinput or SendInput is
//! not testable off a real machine. Keeping them apart means a row's intent
//! can be checked without a VM.

use anyhow::{anyhow, bail, Context, Result};
use pheme_input::keymap::key_by_name;
use pheme_proto::KeyCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gesture {
    MoveRel {
        dx: i32,
        dy: i32,
    },
    /// Text whose every character [`parse`] has checked has a key.
    Type(String),
    Hold(KeyCode),
    Release(KeyCode),
    /// Whole notches; negative scrolls down.
    Wheel(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeEvent {
    MoveRel { dx: i32, dy: i32 },
    KeyDown(KeyCode),
    KeyUp(KeyCode),
    Wheel(i32),
}

/// Expands gestures into the events to send, in order.
///
/// Panics on a `Type` holding a character with no key; [`parse`] rejects
/// those, and it is the only way the binary builds a `Gesture`.
pub fn expand(gestures: &[Gesture]) -> Vec<ProbeEvent> {
    let mut out = Vec::new();
    for g in gestures {
        match g {
            Gesture::MoveRel { dx, dy } => out.push(ProbeEvent::MoveRel { dx: *dx, dy: *dy }),
            Gesture::Wheel(n) => out.push(ProbeEvent::Wheel(*n)),
            // Hold and Release are unbalanced by design: row S5 holds a
            // modifier on one machine and releases it on the other.
            Gesture::Hold(k) => out.push(ProbeEvent::KeyDown(*k)),
            Gesture::Release(k) => out.push(ProbeEvent::KeyUp(*k)),
            Gesture::Type(text) => {
                for ch in text.chars() {
                    let (key, shift) = key_for_char(ch).expect("parse validated the text");
                    if shift {
                        out.push(ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT));
                    }
                    out.push(ProbeEvent::KeyDown(key));
                    out.push(ProbeEvent::KeyUp(key));
                    if shift {
                        out.push(ProbeEvent::KeyUp(KeyCode::LEFT_SHIFT));
                    }
                }
            }
        }
    }
    out
}

/// Parses the gesture string the runner sends over ssh, e.g.
/// `"move 2000 0; type Hi; hold LeftShift"`.
pub fn parse(spec: &str) -> Result<Vec<Gesture>> {
    let mut out = Vec::new();
    for part in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut words = part.split_whitespace();
        let verb = words.next().unwrap_or("");
        let mut next = || {
            words
                .next()
                .ok_or_else(|| anyhow!("{part:?} is missing an argument"))
        };
        match verb {
            "move" => {
                let dx = next()?.parse().with_context(|| format!("in {part:?}"))?;
                let dy = next()?.parse().with_context(|| format!("in {part:?}"))?;
                out.push(Gesture::MoveRel { dx, dy });
            }
            "type" => {
                let text = words.collect::<Vec<_>>().join(" ");
                // Checked here so `expand` never meets a character it cannot
                // type: a row claiming to type 100 characters must not
                // quietly type 98, nor stop after 50.
                for ch in text.chars() {
                    key_for_char(ch)?;
                }
                out.push(Gesture::Type(text));
            }
            "hold" => out.push(Gesture::Hold(named_key(next()?)?)),
            "release" => out.push(Gesture::Release(named_key(next()?)?)),
            "wheel" => out.push(Gesture::Wheel(
                next()?.parse().with_context(|| format!("in {part:?}"))?,
            )),
            // Never silently ignored: a typo in a row's gesture would become
            // a no-op that still reported PASS.
            other => bail!("unknown gesture verb {other:?} in {part:?}"),
        }
    }
    Ok(out)
}

fn named_key(name: &str) -> Result<KeyCode> {
    key_by_name(name).ok_or_else(|| anyhow!("no key named {name:?}"))
}

/// The key for a character, and whether Shift must be held for it. Uppercase
/// letters are the shifted form of the same key, which is why this returns a
/// flag rather than a second KeyCode. Names come from the product's own key
/// table, so the probe cannot drift from the keyboard pheme knows.
fn key_for_char(ch: char) -> Result<(KeyCode, bool)> {
    let name = match ch {
        ' ' => "Space".to_string(),
        c => c.to_ascii_lowercase().to_string(),
    };
    let key = key_by_name(&name).ok_or_else(|| anyhow!("no key for character {ch:?}"))?;
    Ok((key, ch.is_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `KeyCode` is a newtype over a HID usage code, not an enum -- see
    /// `crates/pheme-proto/src/lib.rs:65` -- and `pheme_input::keymap` already
    /// maps names to it. Tests go through that map rather than hardcoding
    /// numbers, so they also pin the name the runner will send over ssh.
    fn k(name: &str) -> KeyCode {
        pheme_input::keymap::key_by_name(name).expect("a key the table knows")
    }

    /// The production change that breaks this: make `expand` emit a single
    /// event per character and drop the key-up. A recorder counting downs and
    /// ups would then see 100 downs and 0 ups, and row S4's "no key left held"
    /// criterion would be unprovable.
    #[test]
    fn typing_a_lowercase_letter_presses_and_releases_it() {
        let events = expand(&[Gesture::Type("a".into())]);
        assert_eq!(
            events,
            vec![ProbeEvent::KeyDown(k("a")), ProbeEvent::KeyUp(k("a"))]
        );
    }

    /// The production change that breaks this: emit the letter without
    /// bracketing it in Shift. The row would then type "b" where it claimed
    /// to type "B", and S4's Shift+letter coverage would be a fiction.
    #[test]
    fn typing_an_uppercase_letter_brackets_it_in_shift() {
        let events = expand(&[Gesture::Type("B".into())]);
        assert_eq!(
            events,
            vec![
                ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT),
                ProbeEvent::KeyDown(k("b")),
                ProbeEvent::KeyUp(k("b")),
                ProbeEvent::KeyUp(KeyCode::LEFT_SHIFT),
            ]
        );
    }

    /// Row S5 holds a modifier across the edge, so `Hold` must deliberately
    /// leave the key down. The production change that breaks this: have
    /// `Hold` emit a matching `KeyUp`, which would make S5 assert nothing.
    #[test]
    fn hold_leaves_the_key_down_on_purpose() {
        let events = expand(&[Gesture::Hold(KeyCode::LEFT_SHIFT)]);
        assert_eq!(events, vec![ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT)]);
    }

    /// A gesture string is what the runner passes over ssh, so its parser is
    /// part of the contract. The production change that breaks this: accept
    /// an unknown verb silently, which would turn a typo in a row into a
    /// no-op that still reports PASS.
    #[test]
    fn an_unknown_gesture_verb_is_an_error_not_a_no_op() {
        assert!(parse("wiggle 3").is_err());
    }

    /// `expand` trusts its input, so `parse` is where text is checked. The
    /// production change that breaks this: accept any text in `type` and let
    /// `expand` meet the character -- the probe would then panic halfway
    /// through a row, having emitted half its keystrokes.
    #[test]
    fn a_character_with_no_key_is_a_parse_error() {
        assert!(parse("type naïve").is_err());
    }

    /// The runner types sentences, and the key table names the space bar
    /// "Space", not " ". The production change that breaks this: look every
    /// character up by its own text, which makes any phrase with a space
    /// untypeable.
    #[test]
    fn typing_a_space_presses_the_space_bar() {
        let g = parse("type a b").expect("valid");
        let events = expand(&g);
        assert_eq!(events[2], ProbeEvent::KeyDown(k("Space")));
        assert_eq!(events.len(), 6);
    }

    #[test]
    fn gestures_parse_in_order() {
        let g = parse("move 2000 0; type Hi; hold LeftShift").expect("valid");
        assert_eq!(
            g,
            vec![
                Gesture::MoveRel { dx: 2000, dy: 0 },
                Gesture::Type("Hi".into()),
                Gesture::Hold(KeyCode::LEFT_SHIFT),
            ]
        );
    }
}
