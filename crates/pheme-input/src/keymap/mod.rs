//! HID ↔ platform key code tables.

use std::collections::HashMap;
use std::sync::OnceLock;

use pheme_proto::KeyCode;

pub mod table;

pub const KEY_PAUSE: KeyCode = KeyCode(0x48);
pub const KEY_PRINT_SCREEN: KeyCode = KeyCode(0x46);
pub const KEY_NUM_LOCK: KeyCode = KeyCode(0x53);

struct Maps {
    hid_to_evdev: HashMap<u16, u16>,
    evdev_to_hid: HashMap<u16, u16>,
    hid_to_sc: HashMap<u16, (u16, bool)>,
    sc_to_hid: HashMap<(u16, bool), u16>,
    by_name: HashMap<String, u16>,
}

fn maps() -> &'static Maps {
    static MAPS: OnceLock<Maps> = OnceLock::new();
    MAPS.get_or_init(|| {
        let mut m = Maps {
            hid_to_evdev: HashMap::new(),
            evdev_to_hid: HashMap::new(),
            hid_to_sc: HashMap::new(),
            sc_to_hid: HashMap::new(),
            by_name: HashMap::new(),
        };
        for e in table::TABLE {
            m.hid_to_evdev.insert(e.hid, e.evdev);
            m.evdev_to_hid.insert(e.evdev, e.hid);
            if let Some(w) = e.win {
                m.hid_to_sc.insert(e.hid, w);
                m.sc_to_hid.insert(w, e.hid);
            }
            m.by_name.insert(e.name.to_ascii_lowercase(), e.hid);
        }
        m
    })
}

pub fn hid_to_evdev(code: KeyCode) -> Option<u16> {
    maps().hid_to_evdev.get(&code.0).copied()
}

pub fn evdev_to_hid(code: u16) -> Option<KeyCode> {
    maps().evdev_to_hid.get(&code).map(|&h| KeyCode(h))
}

/// Windows scancode set 1 and whether the E0 prefix (extended) applies.
pub fn hid_to_scancode(code: KeyCode) -> Option<(u16, bool)> {
    maps().hid_to_sc.get(&code.0).copied()
}

pub fn scancode_to_hid(sc: u16, extended: bool) -> Option<KeyCode> {
    maps().sc_to_hid.get(&(sc, extended)).map(|&h| KeyCode(h))
}

pub fn key_by_name(name: &str) -> Option<KeyCode> {
    maps()
        .by_name
        .get(&name.to_ascii_lowercase())
        .map(|&h| KeyCode(h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_entry_roundtrips_through_evdev_and_scancode() {
        for e in table::TABLE {
            let hid = KeyCode(e.hid);
            assert_eq!(hid_to_evdev(hid), Some(e.evdev), "{}", e.name);
            assert_eq!(evdev_to_hid(e.evdev), Some(hid), "{}", e.name);
            if let Some((sc, ext)) = e.win {
                assert_eq!(hid_to_scancode(hid), Some((sc, ext)), "{}", e.name);
                assert_eq!(scancode_to_hid(sc, ext), Some(hid), "{}", e.name);
            }
        }
    }

    #[test]
    fn no_duplicate_codes() {
        let mut hids = HashSet::new();
        let mut evs = HashSet::new();
        let mut scs = HashSet::new();
        for e in table::TABLE {
            assert!(
                hids.insert(e.hid),
                "duplicate hid {:#x} ({})",
                e.hid,
                e.name
            );
            assert!(
                evs.insert(e.evdev),
                "duplicate evdev {} ({})",
                e.evdev,
                e.name
            );
            if let Some(w) = e.win {
                assert!(scs.insert(w), "duplicate scancode {:?} ({})", w, e.name);
            }
        }
    }

    #[test]
    fn tricky_keys() {
        assert_eq!(key_by_name("Pause"), Some(KEY_PAUSE));
        assert_eq!(hid_to_evdev(KEY_PAUSE), Some(119));
        assert_eq!(
            hid_to_scancode(KEY_PAUSE),
            None,
            "Pause has an E1 prefix; handled specially"
        );
        assert_eq!(hid_to_evdev(KEY_PRINT_SCREEN), Some(99));
        assert_eq!(hid_to_scancode(KEY_PRINT_SCREEN), Some((0x37, true)));
        assert_eq!(hid_to_evdev(KEY_NUM_LOCK), Some(69));
        assert_eq!(hid_to_scancode(KEY_NUM_LOCK), Some((0x45, false)));
        assert_eq!(
            hid_to_scancode(KeyCode(0x4F)),
            Some((0x4D, true)),
            "Right arrow is extended"
        );
        assert_eq!(
            hid_to_scancode(KeyCode(0x5E)),
            Some((0x4D, false)),
            "KP6 is not"
        );
        assert_eq!(hid_to_scancode(KeyCode::RIGHT_CTRL), Some((0x1D, true)));
        assert_eq!(hid_to_scancode(KeyCode::LEFT_CTRL), Some((0x1D, false)));
        assert_eq!(hid_to_scancode(KeyCode::RIGHT_ALT), Some((0x38, true)));
        assert_eq!(hid_to_scancode(KeyCode::LEFT_GUI), Some((0x5B, true)));
        assert_eq!(hid_to_evdev(KeyCode::LEFT_GUI), Some(125));
        assert_eq!(hid_to_scancode(KeyCode(0x7F)), Some((0x20, true)), "Mute");
        assert_eq!(hid_to_evdev(KeyCode(0x80)), Some(115), "VolumeUp");
    }

    #[test]
    fn names_are_case_insensitive_and_unknown_is_none() {
        assert_eq!(key_by_name("scrolllock"), Some(KeyCode(0x47)));
        assert_eq!(key_by_name("SCROLLLOCK"), Some(KeyCode(0x47)));
        assert_eq!(key_by_name("F13"), Some(KeyCode(0x68)));
        assert_eq!(key_by_name("nope"), None);
    }

    #[test]
    fn unknown_codes_are_none() {
        assert_eq!(hid_to_evdev(KeyCode(0xFFFF)), None);
        assert_eq!(evdev_to_hid(0xFFFF), None);
        assert_eq!(scancode_to_hid(0x1FF, false), None);
    }
}
