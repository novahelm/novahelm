//! Windows set-1 scancode (+ E0 "extended" flag) <-> USB HID usage (keyboard page).
//!
//! Scancodes are position-based, so the mapping is layout-independent.
//!
//! Quirks:
//! - Pause (E1 1D 45) and Num Lock (45) collide in scancode space, and the hook
//!   reports them inconsistently, so both are identified by virtual key instead
//!   (`usage_from_special_vk` / `special_vk`). The `TABLE` row for Num Lock exists
//!   for completeness; capture and injection check the special-VK path first.
//! - Num-Lock-on navigation keys arrive wrapped in fake `E0 2A` / `E0 36` shifts, and
//!   AltGr arrives preceded by a fake LCtrl with scancode `0x21D`. None of those are in
//!   `TABLE`, so capture drops them; the remote re-creates them natively when it
//!   injects the real key (RAlt, arrows, ...).
//! - Media transport keys (play/pause/next/prev) are not mapped yet.

use novalink_proto::keys as k;

pub const VK_PAUSE: u16 = 0x13;
pub const VK_NUMLOCK: u16 = 0x90;

/// `(scancode, extended, HID usage)`.
pub const TABLE: &[(u16, bool, u16)] = &[
    (0x1E, false, k::A),
    (0x30, false, k::B),
    (0x2E, false, k::C),
    (0x20, false, k::D),
    (0x12, false, k::E),
    (0x21, false, k::F),
    (0x22, false, k::G),
    (0x23, false, k::H),
    (0x17, false, k::I),
    (0x24, false, k::J),
    (0x25, false, k::K),
    (0x26, false, k::L),
    (0x32, false, k::M),
    (0x31, false, k::N),
    (0x18, false, k::O),
    (0x19, false, k::P),
    (0x10, false, k::Q),
    (0x13, false, k::R),
    (0x1F, false, k::S),
    (0x14, false, k::T),
    (0x16, false, k::U),
    (0x2F, false, k::V),
    (0x11, false, k::W),
    (0x2D, false, k::X),
    (0x15, false, k::Y),
    (0x2C, false, k::Z),
    (0x02, false, k::N1),
    (0x03, false, k::N2),
    (0x04, false, k::N3),
    (0x05, false, k::N4),
    (0x06, false, k::N5),
    (0x07, false, k::N6),
    (0x08, false, k::N7),
    (0x09, false, k::N8),
    (0x0A, false, k::N9),
    (0x0B, false, k::N0),
    (0x1C, false, k::ENTER),
    (0x01, false, k::ESCAPE),
    (0x0E, false, k::BACKSPACE),
    (0x0F, false, k::TAB),
    (0x39, false, k::SPACE),
    (0x0C, false, k::MINUS),
    (0x0D, false, k::EQUAL),
    (0x1A, false, k::LEFT_BRACKET),
    (0x1B, false, k::RIGHT_BRACKET),
    (0x2B, false, k::BACKSLASH),
    (0x27, false, k::SEMICOLON),
    (0x28, false, k::QUOTE),
    (0x29, false, k::GRAVE),
    (0x33, false, k::COMMA),
    (0x34, false, k::PERIOD),
    (0x35, false, k::SLASH),
    (0x3A, false, k::CAPS_LOCK),
    (0x56, false, k::NON_US_BACKSLASH),
    (0x3B, false, k::F1),
    (0x3C, false, k::F2),
    (0x3D, false, k::F3),
    (0x3E, false, k::F4),
    (0x3F, false, k::F5),
    (0x40, false, k::F6),
    (0x41, false, k::F7),
    (0x42, false, k::F8),
    (0x43, false, k::F9),
    (0x44, false, k::F10),
    (0x57, false, k::F11),
    (0x58, false, k::F12),
    (0x64, false, k::F13),
    (0x65, false, k::F14),
    (0x66, false, k::F15),
    (0x67, false, k::F16),
    (0x68, false, k::F17),
    (0x69, false, k::F18),
    (0x6A, false, k::F19),
    (0x6B, false, k::F20),
    (0x6C, false, k::F21),
    (0x6D, false, k::F22),
    (0x6E, false, k::F23),
    (0x76, false, k::F24),
    (0x37, true, k::PRINT_SCREEN),
    (0x46, false, k::SCROLL_LOCK),
    (0x45, false, k::NUM_LOCK),
    (0x52, true, k::INSERT),
    (0x47, true, k::HOME),
    (0x49, true, k::PAGE_UP),
    (0x53, true, k::DELETE),
    (0x4F, true, k::END),
    (0x51, true, k::PAGE_DOWN),
    (0x4D, true, k::RIGHT_ARROW),
    (0x4B, true, k::LEFT_ARROW),
    (0x50, true, k::DOWN_ARROW),
    (0x48, true, k::UP_ARROW),
    (0x35, true, k::KP_DIVIDE),
    (0x37, false, k::KP_MULTIPLY),
    (0x4A, false, k::KP_MINUS),
    (0x4E, false, k::KP_PLUS),
    (0x1C, true, k::KP_ENTER),
    (0x4F, false, k::KP_1),
    (0x50, false, k::KP_2),
    (0x51, false, k::KP_3),
    (0x4B, false, k::KP_4),
    (0x4C, false, k::KP_5),
    (0x4D, false, k::KP_6),
    (0x47, false, k::KP_7),
    (0x48, false, k::KP_8),
    (0x49, false, k::KP_9),
    (0x52, false, k::KP_0),
    (0x53, false, k::KP_DECIMAL),
    (0x59, false, k::KP_EQUAL),
    (0x7E, false, k::KP_COMMA),
    (0x5D, true, k::APPLICATION),
    (0x20, true, k::MUTE),
    (0x2E, true, k::VOLUME_DOWN),
    (0x30, true, k::VOLUME_UP),
    (0x73, false, k::INTL1),
    (0x70, false, k::INTL2),
    (0x7D, false, k::INTL3),
    (0x79, false, k::INTL4),
    (0x7B, false, k::INTL5),
    (0x72, false, k::LANG1),
    (0x71, false, k::LANG2),
    (0x1D, false, k::LEFT_CTRL),
    (0x1D, true, k::RIGHT_CTRL),
    (0x2A, false, k::LEFT_SHIFT),
    (0x36, false, k::RIGHT_SHIFT),
    (0x38, false, k::LEFT_ALT),
    (0x38, true, k::RIGHT_ALT),
    (0x5B, true, k::LEFT_GUI),
    (0x5C, true, k::RIGHT_GUI),
];

pub fn to_usage(scancode: u16, extended: bool) -> Option<u16> {
    TABLE
        .iter()
        .find(|&&(s, e, _)| s == scancode && e == extended)
        .map(|&(_, _, u)| u)
}

pub fn to_scancode(usage: u16) -> Option<(u16, bool)> {
    TABLE
        .iter()
        .find(|&&(_, _, u)| u == usage)
        .map(|&(s, e, _)| (s, e))
}

/// Usage for the keys that must be identified by virtual key code.
pub fn usage_from_special_vk(vk: u16) -> Option<u16> {
    match vk {
        VK_PAUSE => Some(k::PAUSE),
        VK_NUMLOCK => Some(k::NUM_LOCK),
        _ => None,
    }
}

/// `(virtual key, needs KEYEVENTF_EXTENDEDKEY)` for keys injected by virtual key.
pub fn special_vk(usage: u16) -> Option<(u16, bool)> {
    match usage {
        k::PAUSE => Some((VK_PAUSE, false)),
        k::NUM_LOCK => Some((VK_NUMLOCK, true)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keymap_roundtrips_every_mapped_key() {
        for &(sc, ext, usage) in TABLE {
            assert_eq!(to_usage(sc, ext), Some(usage));
            assert_eq!(to_scancode(usage), Some((sc, ext)));
        }
        assert_eq!(to_usage(0x1E, false), Some(novalink_proto::keys::A));
        assert_eq!(to_usage(0x48, true), Some(novalink_proto::keys::UP_ARROW)); // E0 48
        assert_eq!(to_usage(0x48, false), Some(novalink_proto::keys::KP_8));
    }

    #[test]
    fn table_has_no_duplicates() {
        let mut keys = HashSet::new();
        let mut usages = HashSet::new();
        for &(sc, ext, usage) in TABLE {
            assert!(keys.insert((sc, ext)), "dup scancode {sc:#x} ext={ext}");
            assert!(usages.insert(usage), "dup usage {usage:#x}");
        }
    }

    #[test]
    fn modifiers_are_distinct_per_side() {
        use novalink_proto::keys as k;
        assert_eq!(to_scancode(k::LEFT_CTRL), Some((0x1D, false)));
        assert_eq!(to_scancode(k::RIGHT_CTRL), Some((0x1D, true)));
        assert_eq!(to_scancode(k::LEFT_ALT), Some((0x38, false)));
        assert_eq!(to_scancode(k::RIGHT_ALT), Some((0x38, true)));
        assert_eq!(to_scancode(k::LEFT_SHIFT), Some((0x2A, false)));
        assert_eq!(to_scancode(k::RIGHT_SHIFT), Some((0x36, false)));
        assert_eq!(to_scancode(k::LEFT_GUI), Some((0x5B, true)));
        assert_eq!(to_scancode(k::RIGHT_GUI), Some((0x5C, true)));
    }

    #[test]
    fn extended_keys_need_the_e0_prefix() {
        use novalink_proto::keys as k;
        for u in [
            k::INSERT,
            k::DELETE,
            k::HOME,
            k::END,
            k::PAGE_UP,
            k::PAGE_DOWN,
            k::LEFT_ARROW,
            k::RIGHT_ARROW,
            k::UP_ARROW,
            k::DOWN_ARROW,
            k::KP_ENTER,
            k::KP_DIVIDE,
            k::LEFT_GUI,
            k::RIGHT_GUI,
            k::RIGHT_CTRL,
            k::RIGHT_ALT,
        ] {
            assert!(to_scancode(u).unwrap().1, "usage {u:#x} must be extended");
        }
        // The keypad twins of the navigation block are not extended.
        assert!(!to_scancode(novalink_proto::keys::KP_7).unwrap().1);
    }

    #[test]
    fn fake_shift_and_altgr_ctrl_are_unmapped() {
        // Num-Lock-on navigation wraps E0 keys in fake E0 2A / E0 36 shifts;
        // AltGr is preceded by a fake LCtrl with scancode 0x21D.
        assert_eq!(to_usage(0x2A, true), None);
        assert_eq!(to_usage(0x36, true), None);
        assert_eq!(to_usage(0x21D, false), None);
    }

    #[test]
    fn pause_and_numlock_use_virtual_keys() {
        use novalink_proto::keys as k;
        assert_eq!(usage_from_special_vk(0x13), Some(k::PAUSE));
        assert_eq!(usage_from_special_vk(0x90), Some(k::NUM_LOCK));
        assert_eq!(usage_from_special_vk(0x41), None);
        assert_eq!(special_vk(k::PAUSE), Some((0x13, false)));
        assert_eq!(special_vk(k::NUM_LOCK), Some((0x90, true)));
        assert_eq!(special_vk(k::A), None);
    }
}
