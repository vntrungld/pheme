//! Translating portal and libei events into `CaptureEvent`.

use pheme_core::Rect;
use pheme_proto::KeyCode;

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
    }

    #[test]
    fn the_keys_produce_the_modifiers_the_protocol_expects() {
        use pheme_proto::Modifiers;
        let m = Modifiers::from_held(modifier_keys(0x01 | 0x08));
        assert_eq!(m, Modifiers(Modifiers::SHIFT | Modifiers::ALT));
    }
}
