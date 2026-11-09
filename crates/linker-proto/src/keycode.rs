//! Hardware-independent keycodes.
//!
//! Keys travel on the wire as USB HID usage IDs (Keyboard/Keypad page 0x07),
//! which are layout- and OS-independent physical key positions. Each backend
//! converts its native code with the tables below:
//!
//! * Linux: evdev `KEY_*` codes (`linux/input-event-codes.h`)
//! * macOS: virtual keycodes (`kVK_*` from `HIToolbox/Events.h`)

/// `(hid, evdev, mac_vk)`; `None` where a platform has no such key.
const TABLE: &[(u16, Option<u16>, Option<u16>)] = &[
    // Letters
    (0x04, Some(30), Some(0x00)), // A
    (0x05, Some(48), Some(0x0B)), // B
    (0x06, Some(46), Some(0x08)), // C
    (0x07, Some(32), Some(0x02)), // D
    (0x08, Some(18), Some(0x0E)), // E
    (0x09, Some(33), Some(0x03)), // F
    (0x0A, Some(34), Some(0x05)), // G
    (0x0B, Some(35), Some(0x04)), // H
    (0x0C, Some(23), Some(0x22)), // I
    (0x0D, Some(36), Some(0x26)), // J
    (0x0E, Some(37), Some(0x28)), // K
    (0x0F, Some(38), Some(0x25)), // L
    (0x10, Some(50), Some(0x2E)), // M
    (0x11, Some(49), Some(0x2D)), // N
    (0x12, Some(24), Some(0x1F)), // O
    (0x13, Some(25), Some(0x23)), // P
    (0x14, Some(16), Some(0x0C)), // Q
    (0x15, Some(19), Some(0x0F)), // R
    (0x16, Some(31), Some(0x01)), // S
    (0x17, Some(20), Some(0x11)), // T
    (0x18, Some(22), Some(0x20)), // U
    (0x19, Some(47), Some(0x09)), // V
    (0x1A, Some(17), Some(0x0D)), // W
    (0x1B, Some(45), Some(0x07)), // X
    (0x1C, Some(21), Some(0x10)), // Y
    (0x1D, Some(44), Some(0x06)), // Z
    // Digits row
    (0x1E, Some(2), Some(0x12)),  // 1
    (0x1F, Some(3), Some(0x13)),  // 2
    (0x20, Some(4), Some(0x14)),  // 3
    (0x21, Some(5), Some(0x15)),  // 4
    (0x22, Some(6), Some(0x17)),  // 5
    (0x23, Some(7), Some(0x16)),  // 6
    (0x24, Some(8), Some(0x1A)),  // 7
    (0x25, Some(9), Some(0x1C)),  // 8
    (0x26, Some(10), Some(0x19)), // 9
    (0x27, Some(11), Some(0x1D)), // 0
    // Control / punctuation
    (0x28, Some(28), Some(0x24)), // Enter
    (0x29, Some(1), Some(0x35)),  // Escape
    (0x2A, Some(14), Some(0x33)), // Backspace (mac "Delete")
    (0x2B, Some(15), Some(0x30)), // Tab
    (0x2C, Some(57), Some(0x31)), // Space
    (0x2D, Some(12), Some(0x1B)), // Minus
    (0x2E, Some(13), Some(0x18)), // Equal
    (0x2F, Some(26), Some(0x21)), // [
    (0x30, Some(27), Some(0x1E)), // ]
    (0x31, Some(43), Some(0x2A)), // Backslash
    (0x33, Some(39), Some(0x29)), // ;
    (0x34, Some(40), Some(0x27)), // '
    (0x35, Some(41), Some(0x32)), // `
    (0x36, Some(51), Some(0x2B)), // ,
    (0x37, Some(52), Some(0x2F)), // .
    (0x38, Some(53), Some(0x2C)), // /
    (0x39, Some(58), Some(0x39)), // Caps Lock
    // F-keys
    (0x3A, Some(59), Some(0x7A)),  // F1
    (0x3B, Some(60), Some(0x78)),  // F2
    (0x3C, Some(61), Some(0x63)),  // F3
    (0x3D, Some(62), Some(0x76)),  // F4
    (0x3E, Some(63), Some(0x60)),  // F5
    (0x3F, Some(64), Some(0x61)),  // F6
    (0x40, Some(65), Some(0x62)),  // F7
    (0x41, Some(66), Some(0x64)),  // F8
    (0x42, Some(67), Some(0x65)),  // F9
    (0x43, Some(68), Some(0x6D)),  // F10
    (0x44, Some(87), Some(0x67)),  // F11
    (0x45, Some(88), Some(0x6F)),  // F12
    (0x46, Some(99), None),        // Print Screen
    (0x47, Some(70), None),        // Scroll Lock
    (0x48, Some(119), None),       // Pause
    (0x49, Some(110), Some(0x72)), // Insert ↔ mac Help
    (0x4A, Some(102), Some(0x73)), // Home
    (0x4B, Some(104), Some(0x74)), // Page Up
    (0x4C, Some(111), Some(0x75)), // Delete (forward)
    (0x4D, Some(107), Some(0x77)), // End
    (0x4E, Some(109), Some(0x79)), // Page Down
    (0x4F, Some(106), Some(0x7C)), // Right
    (0x50, Some(105), Some(0x7B)), // Left
    (0x51, Some(108), Some(0x7D)), // Down
    (0x52, Some(103), Some(0x7E)), // Up
    // Keypad
    (0x53, Some(69), Some(0x47)),  // Num Lock ↔ mac Keypad Clear
    (0x54, Some(98), Some(0x4B)),  // KP /
    (0x55, Some(55), Some(0x43)),  // KP *
    (0x56, Some(74), Some(0x4E)),  // KP -
    (0x57, Some(78), Some(0x45)),  // KP +
    (0x58, Some(96), Some(0x4C)),  // KP Enter
    (0x59, Some(79), Some(0x53)),  // KP 1
    (0x5A, Some(80), Some(0x54)),  // KP 2
    (0x5B, Some(81), Some(0x55)),  // KP 3
    (0x5C, Some(75), Some(0x56)),  // KP 4
    (0x5D, Some(76), Some(0x57)),  // KP 5
    (0x5E, Some(77), Some(0x58)),  // KP 6
    (0x5F, Some(71), Some(0x59)),  // KP 7
    (0x60, Some(72), Some(0x5B)),  // KP 8
    (0x61, Some(73), Some(0x5C)),  // KP 9
    (0x62, Some(82), Some(0x52)),  // KP 0
    (0x63, Some(83), Some(0x41)),  // KP .
    (0x64, Some(86), Some(0x0A)),  // ISO extra key (102nd) ↔ mac ISO §
    (0x65, Some(127), None),       // Application / Compose
    (0x67, Some(117), Some(0x51)), // KP =
    (0x68, Some(183), Some(0x69)), // F13
    (0x69, Some(184), Some(0x6B)), // F14
    (0x6A, Some(185), Some(0x71)), // F15
    (0x6B, Some(186), Some(0x6A)), // F16
    (0x6C, Some(187), Some(0x40)), // F17
    (0x6D, Some(188), Some(0x4F)), // F18
    (0x6E, Some(189), Some(0x50)), // F19
    (0x6F, Some(190), Some(0x5A)), // F20
    (0x7F, Some(113), Some(0x4A)), // Mute
    (0x80, Some(115), Some(0x48)), // Volume Up
    (0x81, Some(114), Some(0x49)), // Volume Down
    // Modifiers
    (0xE0, Some(29), Some(0x3B)),  // Left Ctrl
    (0xE1, Some(42), Some(0x38)),  // Left Shift
    (0xE2, Some(56), Some(0x3A)),  // Left Alt / Option
    (0xE3, Some(125), Some(0x37)), // Left Meta / Command
    (0xE4, Some(97), Some(0x3E)),  // Right Ctrl
    (0xE5, Some(54), Some(0x3C)),  // Right Shift
    (0xE6, Some(100), Some(0x3D)), // Right Alt / Option
    (0xE7, Some(126), Some(0x36)), // Right Meta / Command
];

pub const HID_LCTRL: u16 = 0xE0;
pub const HID_LSHIFT: u16 = 0xE1;
pub const HID_LALT: u16 = 0xE2;
pub const HID_LMETA: u16 = 0xE3;
pub const HID_RCTRL: u16 = 0xE4;
pub const HID_RSHIFT: u16 = 0xE5;
pub const HID_RALT: u16 = 0xE6;
pub const HID_RMETA: u16 = 0xE7;
pub const HID_ESCAPE: u16 = 0x29;
pub const HID_SCROLL_LOCK: u16 = 0x47;

pub fn hid_from_evdev(code: u16) -> Option<u16> {
    TABLE.iter().find(|e| e.1 == Some(code)).map(|e| e.0)
}

pub fn evdev_from_hid(hid: u16) -> Option<u16> {
    TABLE.iter().find(|e| e.0 == hid).and_then(|e| e.1)
}

pub fn hid_from_mac(vk: u16) -> Option<u16> {
    TABLE.iter().find(|e| e.2 == Some(vk)).map(|e| e.0)
}

pub fn mac_from_hid(hid: u16) -> Option<u16> {
    TABLE.iter().find(|e| e.0 == hid).and_then(|e| e.2)
}

/// All evdev key codes we may need to inject (for uinput device setup).
pub fn all_evdev_codes() -> impl Iterator<Item = u16> {
    TABLE.iter().filter_map(|e| e.1)
}

pub fn is_modifier(hid: u16) -> bool {
    (0xE0..=0xE7).contains(&hid)
}

/// Modifier bit (see [`crate::modifiers`]) for a modifier HID code.
pub fn modifier_bit(hid: u16) -> u8 {
    use crate::modifiers::*;
    match hid {
        HID_LSHIFT | HID_RSHIFT => SHIFT,
        HID_LCTRL | HID_RCTRL => CTRL,
        HID_LALT | HID_RALT => ALT,
        HID_LMETA | HID_RMETA => META,
        _ => 0,
    }
}

/// Swap Control ↔ Meta (Command). Used so that Ctrl+C on the Linux keyboard
/// acts as ⌘C on the Mac (configurable).
pub fn swap_ctrl_meta(hid: u16) -> u16 {
    match hid {
        HID_LCTRL => HID_LMETA,
        HID_LMETA => HID_LCTRL,
        HID_RCTRL => HID_RMETA,
        HID_RMETA => HID_RCTRL,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_has_no_duplicates() {
        let mut hid = HashSet::new();
        let mut ev = HashSet::new();
        let mut mac = HashSet::new();
        for (h, e, m) in TABLE {
            assert!(hid.insert(*h), "duplicate hid {h:#x}");
            if let Some(e) = e {
                assert!(ev.insert(*e), "duplicate evdev {e}");
            }
            if let Some(m) = m {
                assert!(mac.insert(*m), "duplicate mac vk {m:#x}");
            }
        }
    }

    #[test]
    fn evdev_roundtrip() {
        for (h, e, _) in TABLE {
            if let Some(e) = e {
                assert_eq!(hid_from_evdev(*e), Some(*h));
                assert_eq!(evdev_from_hid(*h), Some(*e));
            }
        }
    }

    #[test]
    fn mac_roundtrip() {
        for (h, _, m) in TABLE {
            if let Some(m) = m {
                assert_eq!(hid_from_mac(*m), Some(*h));
                assert_eq!(mac_from_hid(*h), Some(*m));
            }
        }
    }

    #[test]
    fn cross_platform_spot_checks() {
        // KEY_A (30) on Linux is kVK_ANSI_A (0) on mac.
        assert_eq!(mac_from_hid(hid_from_evdev(30).unwrap()), Some(0x00));
        // KEY_ENTER (28) -> kVK_Return (0x24)
        assert_eq!(mac_from_hid(hid_from_evdev(28).unwrap()), Some(0x24));
        // KEY_LEFTMETA (125) -> kVK_Command (0x37)
        assert_eq!(mac_from_hid(hid_from_evdev(125).unwrap()), Some(0x37));
        // kVK_ANSI_Z (6) -> KEY_Z (44)
        assert_eq!(evdev_from_hid(hid_from_mac(0x06).unwrap()), Some(44));
        // Print Screen has no mac equivalent.
        assert_eq!(mac_from_hid(0x46), None);
    }

    #[test]
    fn modifiers() {
        assert!(is_modifier(HID_LSHIFT));
        assert!(!is_modifier(0x04));
        assert_eq!(modifier_bit(HID_RCTRL), crate::modifiers::CTRL);
        assert_eq!(swap_ctrl_meta(HID_LCTRL), HID_LMETA);
        assert_eq!(swap_ctrl_meta(swap_ctrl_meta(HID_RMETA)), HID_RMETA);
        assert_eq!(swap_ctrl_meta(0x04), 0x04);
    }
}
