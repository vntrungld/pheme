//! Recording what the operating system delivered, independently of pheme.
//!
//! The decoders here are pure and tested; the platform halves that open an
//! evdev device or install a low-level hook live in `record_linux` and
//! `record_windows`, and do nothing but feed them.

use crate::gesture::ProbeEvent;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_WHEEL: u16 = 8;

/// The name `pheme-input`'s uinput backend gives its device -- the one pheme's
/// client injects through, and the one this probe creates when it emits.
pub const PHEME_DEVICE: &str = "Pheme Virtual Input";

/// What a recording watches for, and until when.
pub struct Watch<'a> {
    pub deadline: std::time::Instant,
    /// Narrows the evdev device to one whose name contains this. Without it,
    /// the recorder wants exactly [`PHEME_DEVICE`].
    pub device_hint: Option<&'a str>,
    /// Called once the recorder is listening, so nothing is emitted into a
    /// recorder that has not started yet.
    pub ready: &'a mut dyn FnMut(),
    /// Ends the recording early once the events seen so far are enough.
    pub done: &'a dyn Fn(&[ProbeEvent]) -> bool,
}

pub fn name_matches(name: Option<&str>, hint: Option<&str>) -> bool {
    match (name, hint) {
        (Some(n), Some(h)) => n.contains(h),
        (Some(n), None) => n == PHEME_DEVICE,
        (None, _) => false,
    }
}

pub fn describe(hint: Option<&str>) -> String {
    match hint {
        Some(h) => format!("a name containing {h:?}"),
        None => format!("{PHEME_DEVICE:?}"),
    }
}

/// Records until `watch.done` says enough or the deadline passes.
pub fn record(watch: Watch<'_>) -> anyhow::Result<Vec<ProbeEvent>> {
    #[cfg(target_os = "linux")]
    return crate::record_linux::record(watch);
    #[cfg(target_os = "windows")]
    return crate::record_windows::record(watch);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = watch;
        anyhow::bail!("the probe runs on Linux and Windows only")
    }
}

/// Reassembles evdev's event frames into the probe's events.
///
/// The kernel reports one pointer movement as up to two relative events --
/// it drops zero-valued ones -- closed by `SYN_REPORT`, and a wheel notch as
/// the classic axis plus a high-resolution one. Counting raw events would
/// make a diagonal move count twice and every notch count twice; counting
/// frames makes what arrives comparable with what was sent.
#[derive(Debug, Default)]
pub struct EvdevFrames {
    dx: i32,
    dy: i32,
    wheel: i32,
}

impl EvdevFrames {
    /// Feeds one raw event; returns whatever it completes.
    pub fn push(&mut self, ev_type: u16, code: u16, value: i32) -> Vec<ProbeEvent> {
        match (ev_type, code) {
            (EV_KEY, _) => {
                // 2 is autorepeat of a key already down, not a new press.
                let Some(key) = pheme_input::keymap::evdev_to_hid(code) else {
                    return vec![];
                };
                match value {
                    1 => vec![ProbeEvent::KeyDown(key)],
                    0 => vec![ProbeEvent::KeyUp(key)],
                    _ => vec![],
                }
            }
            (EV_REL, REL_X) => {
                self.dx += value;
                vec![]
            }
            (EV_REL, REL_Y) => {
                self.dy += value;
                vec![]
            }
            // The high-resolution axes repeat what REL_WHEEL says.
            (EV_REL, REL_WHEEL) => {
                self.wheel += value;
                vec![]
            }
            (EV_SYN, SYN_REPORT) => {
                let mut out = vec![];
                if self.dx != 0 || self.dy != 0 {
                    out.push(ProbeEvent::MoveRel {
                        dx: self.dx,
                        dy: self.dy,
                    });
                }
                if self.wheel != 0 {
                    out.push(ProbeEvent::Wheel(self.wheel));
                }
                *self = EvdevFrames::default();
                out
            }
            _ => vec![],
        }
    }
}

/// The wheel event a Windows low-level mouse hook reports: the delta is the
/// signed high word of `mouseData`, in 1/120ths of a notch.
pub fn windows_wheel(mouse_data: u32) -> ProbeEvent {
    let delta = (mouse_data >> 16) as u16 as i16;
    ProbeEvent::Wheel(i32::from(delta) / 120)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::ProbeEvent;
    use pheme_proto::KeyCode;

    const EV_SYN: u16 = 0;
    const EV_KEY: u16 = 1;
    const EV_REL: u16 = 2;
    const SYN_REPORT: u16 = 0;
    const REL_X: u16 = 0;
    const REL_Y: u16 = 1;
    const REL_WHEEL: u16 = 8;
    const REL_WHEEL_HI_RES: u16 = 11;
    const KEY_LEFTSHIFT: u16 = 42;

    fn feed(f: &mut EvdevFrames, evs: &[(u16, u16, i32)]) -> Vec<ProbeEvent> {
        evs.iter().flat_map(|&(t, c, v)| f.push(t, c, v)).collect()
    }

    /// The kernel reports one diagonal move as REL_X and REL_Y in the same
    /// frame. The production change that breaks this: emit a MoveRel per REL
    /// event, which counts a diagonal move twice and a straight one once, so
    /// no row could compare what arrived with what was sent.
    #[test]
    fn one_frame_of_motion_is_one_move() {
        let mut f = EvdevFrames::default();
        let got = feed(
            &mut f,
            &[
                (EV_REL, REL_X, 10),
                (EV_REL, REL_Y, 5),
                (EV_SYN, SYN_REPORT, 0),
            ],
        );
        assert_eq!(got, vec![ProbeEvent::MoveRel { dx: 10, dy: 5 }]);
    }

    /// uinput sends the classic wheel axis and the high-resolution one
    /// together. The production change that breaks this: count both, which
    /// doubles every notch and makes row S6 fail on a correct scroll.
    #[test]
    fn a_hi_res_wheel_event_does_not_count_twice() {
        let mut f = EvdevFrames::default();
        let got = feed(
            &mut f,
            &[
                (EV_REL, REL_WHEEL, -1),
                (EV_REL, REL_WHEEL_HI_RES, -120),
                (EV_SYN, SYN_REPORT, 0),
            ],
        );
        assert_eq!(got, vec![ProbeEvent::Wheel(-1)]);
    }

    /// Value 2 is the kernel's autorepeat for a key still held. The
    /// production change that breaks this: treat any non-zero value as a
    /// press, which turns one held Shift into dozens of downs with one up.
    #[test]
    fn autorepeat_is_not_a_key_press() {
        let mut f = EvdevFrames::default();
        let got = feed(
            &mut f,
            &[
                (EV_KEY, KEY_LEFTSHIFT, 1),
                (EV_KEY, KEY_LEFTSHIFT, 2),
                (EV_KEY, KEY_LEFTSHIFT, 2),
                (EV_KEY, KEY_LEFTSHIFT, 0),
                (EV_SYN, SYN_REPORT, 0),
            ],
        );
        assert_eq!(
            got,
            vec![
                ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT),
                ProbeEvent::KeyUp(KeyCode::LEFT_SHIFT),
            ]
        );
    }

    /// Recording the wrong device judges a row on input that never went
    /// through pheme. The production change that breaks this: match any name
    /// containing "Pheme" by default, which also takes a keyboard someone
    /// named "Pheme Test Keyboard" -- or, with `--device-hint` ignored, makes
    /// the self-check unable to fail for want of a device.
    #[test]
    fn without_a_hint_only_pheme_s_own_device_matches() {
        assert!(name_matches(Some(PHEME_DEVICE), None));
        assert!(!name_matches(Some("Pheme Test Keyboard"), None));
        assert!(!name_matches(None, None));
    }

    #[test]
    fn a_hint_narrows_by_substring() {
        assert!(name_matches(Some(PHEME_DEVICE), Some("Virtual Input")));
        assert!(!name_matches(
            Some(PHEME_DEVICE),
            Some("nonexistent-device")
        ));
    }

    /// The low-level hook packs the wheel delta into the high word of
    /// `mouseData` as a signed number. The production change that breaks
    /// this: read it unsigned, which turns every scroll down into a huge
    /// scroll up, and row S6's sign check would blame pheme.
    #[test]
    fn a_windows_wheel_delta_keeps_its_sign() {
        let down_one_notch = ((-120i16) as u16 as u32) << 16;
        assert_eq!(windows_wheel(down_one_notch), ProbeEvent::Wheel(-1));
        assert_eq!(windows_wheel(120 << 16), ProbeEvent::Wheel(1));
    }
}
