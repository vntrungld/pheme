//! Translating portal and libei events into `CaptureEvent`.

use std::collections::BTreeSet;

use pheme_core::Rect;
use pheme_proto::{Button, KeyCode};

use crate::keymap;
use crate::linux_uinput::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};

/// Brings an `Activated` cursor position inside `rect`.
///
/// The portal reports where the pointer *would* have gone if the barrier had not
/// stopped it, not where it is. Measured on a 2560-wide screen across three runs:
/// 2577, 2564, 2560 — at or beyond the width every time, and the overshoot grows
/// with pointer speed.
///
/// `on_edge` accepts `0..=w-1`, so an unclamped position matches no edge at all:
/// `ServerCore` returns no actions, the pointer stops at the screen edge, and
/// nothing anywhere reports a problem. Clamping is what makes the backend work,
/// not a guard against a value that "should not" occur.
pub fn clamp_into(rect: &Rect, x: f32, y: f32) -> (i32, i32) {
    // Truncate rather than round: rounding 2559.6 up would leave the value one past
    // the edge again, which is the bug this function exists to prevent.
    let xi = (x as i32).clamp(rect.x, rect.x + rect.w - 1);
    let yi = (y as i32).clamp(rect.y, rect.y + rect.h - 1);
    (xi, yi)
}

/// The modifier keys held at the moment capture started, from libei's `depressed` mask.
///
/// The bit positions are the X11 core modifier layout, which every XKB keymap a
/// compositor generates uses in practice; `0x1` for Shift is the one that was
/// measured. XKB resolves virtual modifiers per keymap, so a keymap that puts Alt
/// somewhere other than Mod1 would produce a wrong modifier here — the spec accepts
/// that rather than linking xkbcommon to look the names up, because the cost is one
/// incorrect modifier in `Enter`, not a crash or a stuck key.
pub fn modifier_keys(depressed: u32) -> Vec<KeyCode> {
    const SHIFT: u32 = 0x01;
    const CONTROL: u32 = 0x04;
    const MOD1_ALT: u32 = 0x08;
    const MOD4_SUPER: u32 = 0x40;
    let mut out = Vec::new();
    for (bit, key) in [
        (SHIFT, KeyCode::LEFT_SHIFT),
        (CONTROL, KeyCode::LEFT_CTRL),
        (MOD1_ALT, KeyCode::LEFT_ALT),
        (MOD4_SUPER, KeyCode::LEFT_GUI),
    ] {
        if depressed & bit != 0 {
            out.push(key);
        }
    }
    out
}

/// libei key codes are **raw evdev**, unlike X11's, which offset evdev by 8.
///
/// Measured twice: 42 while Shift was held (`KEY_LEFTSHIFT`) and 30 for the letter A
/// (`KEY_A`). Under the X11 convention those would decode as `KEY_G` and `KEY_Y`. The
/// libei keyboard device does carry an XKB keymap, so borrowing the X11 path's
/// `- 8` looks reasonable and shifts every key by eight positions.
pub fn key_from_evdev(code: u32) -> Option<KeyCode> {
    keymap::evdev_to_hid(u16::try_from(code).ok()?)
}

/// The inverse of `linux_uinput::button_code`, which owns this mapping.
pub fn button_from_evdev(code: u32) -> Option<Button> {
    Some(match u16::try_from(code).ok()? {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE => Button::Back,
        BTN_EXTRA => Button::Forward,
        _ => return None,
    })
}

/// libei's discrete scroll is already in the 120-per-notch unit `CaptureEvent::Wheel`
/// uses, and that the X11 backend emits.
pub fn wheel_from_discrete(dx: i32, dy: i32) -> (i32, i32) {
    (dx, dy)
}

/// Accumulates libei's `f32` relative motion into whole pixels.
///
/// Truncating each event independently loses slow movement entirely: a steady
/// 0.4 px per event would never move the pointer at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct Motion {
    rem_x: f32,
    rem_y: f32,
}

impl Motion {
    /// Returns the whole pixels to emit, carrying the remainder into the next call.
    pub fn push(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        let x = self.rem_x + dx;
        let y = self.rem_y + dy;
        let (ix, iy) = (x.trunc(), y.trunc());
        self.rem_x = x - ix;
        self.rem_y = y - iy;
        (ix as i32, iy as i32)
    }
}

/// Tracks which keys this backend has seen pressed and not released.
///
/// A Wayland server sees the keyboard only while capturing. Hold Shift, cross the
/// edge, come back, then release Shift: the key-up goes to the compositor and never
/// reaches this backend, so `ServerCore`'s `held` set keeps Shift forever and every
/// later `Enter` reports a modifier nobody is pressing. `flush` is what prevents it.
#[derive(Debug, Default)]
pub struct HeldKeys(BTreeSet<u16>);

impl HeldKeys {
    pub fn saw(&mut self, code: KeyCode, down: bool) {
        if down {
            self.0.insert(code.0);
        } else {
            self.0.remove(&code.0);
        }
    }

    /// The keys still held, clearing the set. Emit a key-up for each.
    pub fn flush(&mut self) -> Vec<KeyCode> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(KeyCode)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::geometry::on_edge;
    use pheme_core::{Rect, Side};

    fn screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 2560,
            h: 1440,
        }
    }

    #[test]
    fn a_position_past_the_edge_lands_on_the_edge() {
        // The three values the portal actually reported on this machine.
        for reported in [2577.0_f32, 2564.0, 2560.0] {
            let (x, y) = clamp_into(&screen(), reported, 737.7);
            assert_eq!(x, 2559, "reported {reported}");
            assert_eq!(
                on_edge(&screen(), Side::Right, x, y),
                Some(737),
                "an unclamped position matches no edge, and the switch never happens"
            );
        }
    }

    #[test]
    fn a_position_before_the_origin_lands_on_the_origin() {
        let (x, y) = clamp_into(&screen(), -13.0, -4.0);
        assert_eq!((x, y), (0, 0));
        assert_eq!(on_edge(&screen(), Side::Left, x, y), Some(0));
    }

    #[test]
    fn an_offset_rect_clamps_to_its_own_bounds() {
        let r = Rect {
            x: 100,
            y: 50,
            w: 800,
            h: 600,
        };
        assert_eq!(clamp_into(&r, 5000.0, 5000.0), (899, 649));
        assert_eq!(clamp_into(&r, 0.0, 0.0), (100, 50));
    }

    #[test]
    fn a_fractional_position_truncates_toward_the_screen() {
        let (_, y) = clamp_into(&screen(), 2577.0, 737.9);
        assert_eq!(
            y, 737,
            "rounding up could push the value off the opposite edge"
        );
    }

    #[test]
    fn the_shift_bit_becomes_the_shift_key() {
        // Measured: holding Shift across the barrier reported depressed = 0x1.
        assert_eq!(modifier_keys(0x1), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn every_modifier_bit_maps_to_its_key() {
        assert_eq!(modifier_keys(0x04), vec![KeyCode::LEFT_CTRL]);
        assert_eq!(modifier_keys(0x08), vec![KeyCode::LEFT_ALT]);
        assert_eq!(modifier_keys(0x40), vec![KeyCode::LEFT_GUI]);
    }

    #[test]
    fn several_modifiers_all_come_through() {
        let keys = modifier_keys(0x01 | 0x04 | 0x40);
        assert_eq!(
            keys,
            vec![KeyCode::LEFT_SHIFT, KeyCode::LEFT_CTRL, KeyCode::LEFT_GUI]
        );
    }

    #[test]
    fn bits_that_are_not_modifiers_are_ignored() {
        // 0x02 is Lock (caps), 0x10 is Mod2 (num lock): neither is a Pheme modifier,
        // and neither must produce a stray key press.
        assert!(modifier_keys(0x02 | 0x10).is_empty());
        // Asserting only the line above would still pass against a `modifier_keys`
        // that returned nothing for every input, so the name would outrun the test.
        // Mixing a real modifier in pins selective exclusion, which is the property.
        assert_eq!(modifier_keys(0x02 | 0x10 | 0x01), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn the_keys_produce_the_modifiers_the_protocol_expects() {
        use pheme_proto::Modifiers;
        let m = Modifiers::from_held(modifier_keys(0x01 | 0x08));
        assert_eq!(m, Modifiers(Modifiers::SHIFT | Modifiers::ALT));
    }

    #[test]
    fn key_codes_are_evdev_and_are_not_shifted_by_eight() {
        // Measured: 42 while Shift was held, 30 for the letter A. Under the X11
        // convention these would be KEY_G and KEY_Y.
        assert_eq!(key_from_evdev(42), Some(KeyCode::LEFT_SHIFT));
        assert_eq!(key_from_evdev(30), crate::keymap::evdev_to_hid(30));
        assert_ne!(
            key_from_evdev(30),
            crate::keymap::evdev_to_hid(30 - 8),
            "subtracting 8 is the X11 convention and is wrong here"
        );
    }

    #[test]
    fn an_unknown_key_code_is_dropped_rather_than_guessed() {
        assert_eq!(key_from_evdev(0xFFFF), None);
    }

    #[test]
    fn button_codes_round_trip_against_the_uinput_mapping() {
        use pheme_proto::Button;
        for b in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            let code = crate::linux_uinput::button_code(b);
            assert_eq!(
                button_from_evdev(code as u32),
                Some(b),
                "{b:?} does not survive the round trip"
            );
        }
        // The measured value, named explicitly so the round trip cannot pass by
        // agreeing with itself on a wrong constant.
        assert_eq!(button_from_evdev(272), Some(Button::Left));
    }

    #[test]
    fn an_unknown_button_is_dropped() {
        assert_eq!(button_from_evdev(999), None);
    }

    #[test]
    fn one_scroll_notch_is_one_hundred_and_twenty() {
        assert_eq!(wheel_from_discrete(0, 120), (0, 120));
        assert_eq!(wheel_from_discrete(0, -120), (0, -120));
        assert_eq!(wheel_from_discrete(-120, 0), (-120, 0));
    }

    #[test]
    fn slow_motion_is_accumulated_rather_than_truncated_away() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..8 {
            let (dx, _) = m.push(0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, 3, "0.4 x 8 is 3.2 px; truncating each event gives 0");
    }

    #[test]
    fn the_remainder_does_not_drift_over_a_long_run() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..1000 {
            let (dx, _) = m.push(1.5, 0.0);
            total += dx;
        }
        assert_eq!(total, 1500);
    }

    #[test]
    fn negative_motion_accumulates_symmetrically() {
        let mut m = Motion::default();
        let mut total = 0;
        for _ in 0..8 {
            let (dx, _) = m.push(-0.4, 0.0);
            total += dx;
        }
        assert_eq!(total, -3);
    }

    #[test]
    fn held_keys_are_released_when_capture_ends() {
        let mut h = HeldKeys::default();
        h.saw(KeyCode::LEFT_SHIFT, true);
        h.saw(KeyCode(0x04), true); // the letter A
        h.saw(KeyCode(0x04), false);
        assert_eq!(h.flush(), vec![KeyCode::LEFT_SHIFT]);
    }

    #[test]
    fn flushing_twice_releases_nothing_the_second_time() {
        let mut h = HeldKeys::default();
        h.saw(KeyCode::LEFT_SHIFT, true);
        assert_eq!(h.flush().len(), 1);
        assert!(
            h.flush().is_empty(),
            "a second flush would send a key-up for a key nobody is holding"
        );
    }

    #[test]
    fn a_key_held_across_a_release_does_not_leak_into_the_next_capture() {
        // Hold Shift, cross, come back, release Shift while local. The key-up goes to
        // the compositor and this backend never sees it, so without the flush `held`
        // in ServerCore would keep Shift forever and every later Enter would be wrong.
        let mut h = HeldKeys::default();
        for k in modifier_keys(0x1) {
            h.saw(k, true);
        }
        assert_eq!(h.flush(), vec![KeyCode::LEFT_SHIFT]);
        assert!(h.flush().is_empty());
    }
}
