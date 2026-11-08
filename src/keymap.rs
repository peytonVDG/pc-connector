//! Keyboard code translation.
//!
//! Keys travel over the network as USB HID keyboard usage ids (the same numbering SDL
//! uses for scancodes), which are layout independent: the host's keyboard layout decides
//! which character a key produces, exactly like a USB keyboard plugged into the host.
//!
//! Media keys use the 0xE8..0xEF range like Linux's boot-protocol table.

/// HID usage -> Linux evdev KEY_* code (from drivers/hid/hid-input.c `hid_keyboard`).
#[rustfmt::skip]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const HID_TO_EVDEV: [u8; 256] = [
      0,  0,  0,  0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
     50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,  2,  3,
      4,  5,  6,  7,  8,  9, 10, 11, 28,  1, 14, 15, 57, 12, 13, 26,
     27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
     65, 66, 67, 68, 87, 88, 99, 70,119,110,102,104,111,107,109,106,
    105,108,103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
     72, 73, 82, 83, 86,127,116,117,183,184,185,186,187,188,189,190,
    191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
    115,114,  0,  0,  0,121,  0, 89, 93,124, 92, 94, 95,  0,  0,  0,
    122,123, 90, 91, 85,  0,  0,  0,  0,  0,  0,  0,111,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,179,180,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,111,  0,  0,  0,  0,  0,  0,  0,
     29, 42, 56,125, 97, 54,100,126,164,166,165,163,161,115,114,113,
    150,158,159,128,136,177,178,176,142,152,173,140,  0,  0,  0,  0,
];

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn hid_to_evdev(hid: u16) -> Option<u16> {
    let code = *HID_TO_EVDEV.get(hid as usize)?;
    (code != 0).then_some(code as u16)
}

/// All evdev key codes a virtual keyboard should advertise.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn all_evdev_keys() -> Vec<u16> {
    let mut v: Vec<u16> = HID_TO_EVDEV.iter().filter(|&&c| c != 0).map(|&c| c as u16).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Windows "set 1" scancodes. `0xE0xx` marks an extended key.
#[rustfmt::skip]
const HID_WIN_SCANCODES: &[(u16, u16)] = &[
    (0x04, 0x1E), (0x05, 0x30), (0x06, 0x2E), (0x07, 0x20), (0x08, 0x12), (0x09, 0x21),
    (0x0A, 0x22), (0x0B, 0x23), (0x0C, 0x17), (0x0D, 0x24), (0x0E, 0x25), (0x0F, 0x26),
    (0x10, 0x32), (0x11, 0x31), (0x12, 0x18), (0x13, 0x19), (0x14, 0x10), (0x15, 0x13),
    (0x16, 0x1F), (0x17, 0x14), (0x18, 0x16), (0x19, 0x2F), (0x1A, 0x11), (0x1B, 0x2D),
    (0x1C, 0x15), (0x1D, 0x2C),
    (0x1E, 0x02), (0x1F, 0x03), (0x20, 0x04), (0x21, 0x05), (0x22, 0x06), (0x23, 0x07),
    (0x24, 0x08), (0x25, 0x09), (0x26, 0x0A), (0x27, 0x0B),
    (0x28, 0x1C), (0x29, 0x01), (0x2A, 0x0E), (0x2B, 0x0F), (0x2C, 0x39), (0x2D, 0x0C),
    (0x2E, 0x0D), (0x2F, 0x1A), (0x30, 0x1B), (0x31, 0x2B), (0x32, 0x2B), (0x33, 0x27),
    (0x34, 0x28), (0x35, 0x29), (0x36, 0x33), (0x37, 0x34), (0x38, 0x35), (0x39, 0x3A),
    (0x3A, 0x3B), (0x3B, 0x3C), (0x3C, 0x3D), (0x3D, 0x3E), (0x3E, 0x3F), (0x3F, 0x40),
    (0x40, 0x41), (0x41, 0x42), (0x42, 0x43), (0x43, 0x44), (0x44, 0x57), (0x45, 0x58),
    (0x46, 0xE037), (0x47, 0x46), (0x48, 0x45), (0x49, 0xE052), (0x4A, 0xE047), (0x4B, 0xE049),
    (0x4C, 0xE053), (0x4D, 0xE04F), (0x4E, 0xE051), (0x4F, 0xE04D), (0x50, 0xE04B), (0x51, 0xE050),
    (0x52, 0xE048), (0x53, 0xE045), (0x54, 0xE035), (0x55, 0x37), (0x56, 0x4A), (0x57, 0x4E),
    (0x58, 0xE01C), (0x59, 0x4F), (0x5A, 0x50), (0x5B, 0x51), (0x5C, 0x4B), (0x5D, 0x4C),
    (0x5E, 0x4D), (0x5F, 0x47), (0x60, 0x48), (0x61, 0x49), (0x62, 0x52), (0x63, 0x53),
    (0x64, 0x56), (0x65, 0xE05D), (0x67, 0x59),
    (0x68, 0x64), (0x69, 0x65), (0x6A, 0x66), (0x6B, 0x67), (0x6C, 0x68), (0x6D, 0x69),
    (0x6E, 0x6A), (0x6F, 0x6B), (0x70, 0x6C), (0x71, 0x6D), (0x72, 0x6E), (0x73, 0x76),
    (0x7F, 0xE020), (0x80, 0xE030), (0x81, 0xE02E),
    (0x85, 0x7E), (0x87, 0x73), (0x88, 0x70), (0x89, 0x7D), (0x8A, 0x79), (0x8B, 0x7B),
    (0xE0, 0x1D), (0xE1, 0x2A), (0xE2, 0x38), (0xE3, 0xE05B), (0xE4, 0xE01D), (0xE5, 0x36),
    (0xE6, 0xE038), (0xE7, 0xE05C),
    (0xE8, 0xE022), (0xE9, 0xE024), (0xEA, 0xE010), (0xEB, 0xE019),
];

/// HID usage -> Windows scancode (with 0xE000 set for extended keys).
#[allow(dead_code)]
pub fn hid_to_win_scancode(hid: u16) -> Option<u16> {
    HID_WIN_SCANCODES.iter().find(|(h, _)| *h == hid).map(|(_, s)| *s)
}

/// Windows scancode (+ extended flag) -> HID usage.
#[allow(dead_code)]
pub fn win_scancode_to_hid(scancode: u16, extended: bool) -> Option<u16> {
    let key = if extended { 0xE000 | (scancode & 0xFF) } else { scancode & 0xFF };
    // HID 0x31 and 0x32 share a scancode; prefer the common backslash key.
    HID_WIN_SCANCODES.iter().find(|(_, s)| *s == key).map(|(h, _)| *h)
}

/// winit physical key -> HID usage.
pub fn winit_to_hid(code: winit::keyboard::KeyCode) -> Option<u16> {
    use winit::keyboard::KeyCode as K;
    let hid = match code {
        K::KeyA => 0x04,
        K::KeyB => 0x05,
        K::KeyC => 0x06,
        K::KeyD => 0x07,
        K::KeyE => 0x08,
        K::KeyF => 0x09,
        K::KeyG => 0x0A,
        K::KeyH => 0x0B,
        K::KeyI => 0x0C,
        K::KeyJ => 0x0D,
        K::KeyK => 0x0E,
        K::KeyL => 0x0F,
        K::KeyM => 0x10,
        K::KeyN => 0x11,
        K::KeyO => 0x12,
        K::KeyP => 0x13,
        K::KeyQ => 0x14,
        K::KeyR => 0x15,
        K::KeyS => 0x16,
        K::KeyT => 0x17,
        K::KeyU => 0x18,
        K::KeyV => 0x19,
        K::KeyW => 0x1A,
        K::KeyX => 0x1B,
        K::KeyY => 0x1C,
        K::KeyZ => 0x1D,
        K::Digit1 => 0x1E,
        K::Digit2 => 0x1F,
        K::Digit3 => 0x20,
        K::Digit4 => 0x21,
        K::Digit5 => 0x22,
        K::Digit6 => 0x23,
        K::Digit7 => 0x24,
        K::Digit8 => 0x25,
        K::Digit9 => 0x26,
        K::Digit0 => 0x27,
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2A,
        K::Tab => 0x2B,
        K::Space => 0x2C,
        K::Minus => 0x2D,
        K::Equal => 0x2E,
        K::BracketLeft => 0x2F,
        K::BracketRight => 0x30,
        K::Backslash => 0x31,
        K::Semicolon => 0x33,
        K::Quote => 0x34,
        K::Backquote => 0x35,
        K::Comma => 0x36,
        K::Period => 0x37,
        K::Slash => 0x38,
        K::CapsLock => 0x39,
        K::F1 => 0x3A,
        K::F2 => 0x3B,
        K::F3 => 0x3C,
        K::F4 => 0x3D,
        K::F5 => 0x3E,
        K::F6 => 0x3F,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::PrintScreen => 0x46,
        K::ScrollLock => 0x47,
        K::Pause => 0x48,
        K::Insert => 0x49,
        K::Home => 0x4A,
        K::PageUp => 0x4B,
        K::Delete => 0x4C,
        K::End => 0x4D,
        K::PageDown => 0x4E,
        K::ArrowRight => 0x4F,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,
        K::NumLock => 0x53,
        K::NumpadDivide => 0x54,
        K::NumpadMultiply => 0x55,
        K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57,
        K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59,
        K::Numpad2 => 0x5A,
        K::Numpad3 => 0x5B,
        K::Numpad4 => 0x5C,
        K::Numpad5 => 0x5D,
        K::Numpad6 => 0x5E,
        K::Numpad7 => 0x5F,
        K::Numpad8 => 0x60,
        K::Numpad9 => 0x61,
        K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63,
        K::IntlBackslash => 0x64,
        K::ContextMenu => 0x65,
        K::Power => 0x66,
        K::NumpadEqual => 0x67,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6A,
        K::F16 => 0x6B,
        K::F17 => 0x6C,
        K::F18 => 0x6D,
        K::F19 => 0x6E,
        K::F20 => 0x6F,
        K::F21 => 0x70,
        K::F22 => 0x71,
        K::F23 => 0x72,
        K::F24 => 0x73,
        K::Help => 0x75,
        K::Select => 0x77,
        K::Again => 0x79,
        K::Undo => 0x7A,
        K::Cut => 0x7B,
        K::Copy => 0x7C,
        K::Paste => 0x7D,
        K::Find => 0x7E,
        K::AudioVolumeMute => 0x7F,
        K::AudioVolumeUp => 0x80,
        K::AudioVolumeDown => 0x81,
        K::NumpadComma => 0x85,
        K::IntlRo => 0x87,
        K::KanaMode => 0x88,
        K::IntlYen => 0x89,
        K::Convert => 0x8A,
        K::NonConvert => 0x8B,
        K::Lang1 => 0x90,
        K::Lang2 => 0x91,
        K::Katakana => 0x92,
        K::Hiragana => 0x93,
        K::NumpadParenLeft => 0xB6,
        K::NumpadParenRight => 0xB7,
        K::ControlLeft => 0xE0,
        K::ShiftLeft => 0xE1,
        K::AltLeft => 0xE2,
        K::SuperLeft => 0xE3,
        K::ControlRight => 0xE4,
        K::ShiftRight => 0xE5,
        K::AltRight => 0xE6,
        K::SuperRight => 0xE7,
        K::MediaPlayPause => 0xE8,
        K::MediaStop => 0xE9,
        K::MediaTrackPrevious => 0xEA,
        K::MediaTrackNext => 0xEB,
        _ => return None,
    };
    Some(hid)
}

pub const HID_LCTRL: u16 = 0xE0;
pub const HID_LSHIFT: u16 = 0xE1;
pub const HID_LALT: u16 = 0xE2;
pub const HID_RCTRL: u16 = 0xE4;
pub const HID_RSHIFT: u16 = 0xE5;
pub const HID_RALT: u16 = 0xE6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_basics() {
        assert_eq!(hid_to_evdev(0x04), Some(30)); // A
        assert_eq!(hid_to_evdev(0x29), Some(1)); // Esc
        assert_eq!(hid_to_evdev(0x2C), Some(57)); // Space
        assert_eq!(hid_to_evdev(0x52), Some(103)); // Up
        assert_eq!(hid_to_evdev(0xE3), Some(125)); // Left meta
        assert_eq!(hid_to_evdev(0x00), None);
        assert_eq!(hid_to_evdev(999), None);
    }

    #[test]
    fn windows_roundtrip() {
        for &(hid, sc) in HID_WIN_SCANCODES {
            if hid == 0x32 {
                continue; // shares a scancode with 0x31
            }
            assert_eq!(hid_to_win_scancode(hid), Some(sc));
            assert_eq!(win_scancode_to_hid(sc & 0xFF, sc & 0xE000 != 0), Some(hid), "hid {hid:#x}");
        }
    }

    #[test]
    fn every_win_key_maps_to_evdev() {
        for &(hid, _) in HID_WIN_SCANCODES {
            assert!(hid_to_evdev(hid).is_some(), "hid {hid:#x} has no evdev code");
        }
    }
}
