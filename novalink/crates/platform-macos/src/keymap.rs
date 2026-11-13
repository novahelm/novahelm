//! macOS virtual keycode (`kVK_*`) <-> USB HID usage (keyboard page) mapping.
//!
//! Media keys (play/pause, next, prev, stop) arrive on macOS as NX system-defined
//! events rather than key events, so they are intentionally unmapped here for now.
//! `kVK_Function` (0x3F) has no HID equivalent and is unmapped.

use novalink_proto::keys as k;

pub const TABLE: &[(u16, u16)] = &[
    // Letters (ANSI positions)
    (0x00, k::A),
    (0x0B, k::B),
    (0x08, k::C),
    (0x02, k::D),
    (0x0E, k::E),
    (0x03, k::F),
    (0x05, k::G),
    (0x04, k::H),
    (0x22, k::I),
    (0x26, k::J),
    (0x28, k::K),
    (0x25, k::L),
    (0x2E, k::M),
    (0x2D, k::N),
    (0x1F, k::O),
    (0x23, k::P),
    (0x0C, k::Q),
    (0x0F, k::R),
    (0x01, k::S),
    (0x11, k::T),
    (0x20, k::U),
    (0x09, k::V),
    (0x0D, k::W),
    (0x07, k::X),
    (0x10, k::Y),
    (0x06, k::Z),
    // Digits
    (0x12, k::N1),
    (0x13, k::N2),
    (0x14, k::N3),
    (0x15, k::N4),
    (0x17, k::N5),
    (0x16, k::N6),
    (0x1A, k::N7),
    (0x1C, k::N8),
    (0x19, k::N9),
    (0x1D, k::N0),
    // Punctuation
    (0x18, k::EQUAL),
    (0x1B, k::MINUS),
    (0x1E, k::RIGHT_BRACKET),
    (0x21, k::LEFT_BRACKET),
    (0x27, k::QUOTE),
    (0x29, k::SEMICOLON),
    (0x2A, k::BACKSLASH),
    (0x2B, k::COMMA),
    (0x2C, k::SLASH),
    (0x2F, k::PERIOD),
    (0x32, k::GRAVE),
    (0x0A, k::NON_US_BACKSLASH),
    // Whitespace / editing
    (0x24, k::ENTER),
    (0x30, k::TAB),
    (0x31, k::SPACE),
    (0x33, k::BACKSPACE),
    (0x35, k::ESCAPE),
    (0x39, k::CAPS_LOCK),
    // Modifiers
    (0x37, k::LEFT_GUI),
    (0x38, k::LEFT_SHIFT),
    (0x3A, k::LEFT_ALT),
    (0x3B, k::LEFT_CTRL),
    (0x36, k::RIGHT_GUI),
    (0x3C, k::RIGHT_SHIFT),
    (0x3D, k::RIGHT_ALT),
    (0x3E, k::RIGHT_CTRL),
    // Function keys
    (0x7A, k::F1),
    (0x78, k::F2),
    (0x63, k::F3),
    (0x76, k::F4),
    (0x60, k::F5),
    (0x61, k::F6),
    (0x62, k::F7),
    (0x64, k::F8),
    (0x65, k::F9),
    (0x6D, k::F10),
    (0x67, k::F11),
    (0x6F, k::F12),
    (0x69, k::F13),
    (0x6B, k::F14),
    (0x71, k::F15),
    (0x6A, k::F16),
    (0x40, k::F17),
    (0x4F, k::F18),
    (0x50, k::F19),
    (0x5A, k::F20),
    // Navigation (kVK_Help is the PC Insert position)
    (0x72, k::INSERT),
    (0x73, k::HOME),
    (0x74, k::PAGE_UP),
    (0x75, k::DELETE),
    (0x77, k::END),
    (0x79, k::PAGE_DOWN),
    (0x7B, k::LEFT_ARROW),
    (0x7C, k::RIGHT_ARROW),
    (0x7D, k::DOWN_ARROW),
    (0x7E, k::UP_ARROW),
    // Keypad (kVK_ANSI_KeypadClear is the Num Lock position)
    (0x41, k::KP_DECIMAL),
    (0x43, k::KP_MULTIPLY),
    (0x45, k::KP_PLUS),
    (0x47, k::NUM_LOCK),
    (0x4B, k::KP_DIVIDE),
    (0x4C, k::KP_ENTER),
    (0x4E, k::KP_MINUS),
    (0x51, k::KP_EQUAL),
    (0x52, k::KP_0),
    (0x53, k::KP_1),
    (0x54, k::KP_2),
    (0x55, k::KP_3),
    (0x56, k::KP_4),
    (0x57, k::KP_5),
    (0x58, k::KP_6),
    (0x59, k::KP_7),
    (0x5B, k::KP_8),
    (0x5C, k::KP_9),
    (0x5F, k::KP_COMMA),
    // Misc
    (0x6E, k::APPLICATION),
    (0x4A, k::MUTE),
    (0x48, k::VOLUME_UP),
    (0x49, k::VOLUME_DOWN),
    // JIS
    (0x5D, k::INTL3),
    (0x5E, k::INTL1),
    (0x66, k::LANG2),
    (0x68, k::LANG1),
];

pub fn to_usage(vk: u16) -> Option<u16> {
    TABLE.iter().find(|&&(v, _)| v == vk).map(|&(_, u)| u)
}

pub fn to_vk(usage: u16) -> Option<u16> {
    TABLE.iter().find(|&&(_, u)| u == usage).map(|&(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keymap_roundtrips_every_mapped_key() {
        for &(vk, usage) in TABLE {
            assert_eq!(to_usage(vk), Some(usage));
            assert_eq!(to_vk(usage), Some(vk));
        }
        assert_eq!(to_usage(0x00), Some(novalink_proto::keys::A)); // kVK_ANSI_A
        assert!(TABLE.len() >= 100);
    }

    #[test]
    fn table_is_one_to_one() {
        let vks: HashSet<_> = TABLE.iter().map(|&(v, _)| v).collect();
        let us: HashSet<_> = TABLE.iter().map(|&(_, u)| u).collect();
        assert_eq!(vks.len(), TABLE.len());
        assert_eq!(us.len(), TABLE.len());
    }

    #[test]
    fn unmapped_returns_none() {
        assert_eq!(to_usage(0x3F), None); // Fn
        assert_eq!(to_vk(novalink_proto::keys::MEDIA_PLAY_PAUSE), None);
    }
}
