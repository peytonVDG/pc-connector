//! Windows input injection with SendInput (keyboard + mouse).
//!
//! Virtual gamepads on Windows need a kernel driver (ViGEmBus), so gamepad input is not
//! injected when a Windows PC is the host.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use super::input::{Injector, RumbleCallback};
use crate::keymap::hid_to_win_scancode;
use crate::protocol::GamepadState;

pub struct SendInputInjector {
    warned_gamepad: bool,
}

impl SendInputInjector {
    pub fn new() -> Self {
        Self { warned_gamepad: false }
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: `inputs` is a valid slice of INPUT structures.
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

impl Injector for SendInputInjector {
    fn key(&mut self, hid: u16, down: bool) {
        let up_flag = if down { 0 } else { KEYEVENTF_KEYUP };
        let ki = if hid == 0x48 {
            // Pause has no usable single scancode; use its virtual key.
            KEYBDINPUT { wVk: VK_PAUSE, wScan: 0, dwFlags: up_flag, time: 0, dwExtraInfo: 0 }
        } else {
            let Some(sc) = hid_to_win_scancode(hid) else { return };
            let ext = if sc & 0xE000 != 0 { KEYEVENTF_EXTENDEDKEY } else { 0 };
            KEYBDINPUT {
                wVk: 0,
                wScan: sc & 0xFF,
                dwFlags: KEYEVENTF_SCANCODE | ext | up_flag,
                time: 0,
                dwExtraInfo: 0,
            }
        };
        send(&[INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }]);
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) {
        send(&[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)]);
    }

    fn mouse_abs(&mut self, x: u16, y: u16) {
        // Normalised 0..65535 over the primary monitor (the one that is captured by default).
        send(&[mouse_input(x as i32, y as i32, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)]);
    }

    fn mouse_button(&mut self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (1, false) => (MOUSEEVENTF_LEFTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (4, true) => (MOUSEEVENTF_XDOWN, 1u32),
            (4, false) => (MOUSEEVENTF_XUP, 1),
            (5, true) => (MOUSEEVENTF_XDOWN, 2),
            (5, false) => (MOUSEEVENTF_XUP, 2),
            _ => return,
        };
        send(&[mouse_input(0, 0, data, flags)]);
    }

    fn scroll(&mut self, vertical: i16, horizontal: i16) {
        if vertical != 0 {
            send(&[mouse_input(0, 0, vertical as i32 as u32, MOUSEEVENTF_WHEEL)]);
        }
        if horizontal != 0 {
            send(&[mouse_input(0, 0, horizontal as i32 as u32, MOUSEEVENTF_HWHEEL)]);
        }
    }

    fn gamepad(&mut self, _index: u8, _state: GamepadState) {
        if !self.warned_gamepad {
            log::warn!("Gamepad input is not supported when a Windows PC is the host");
            self.warned_gamepad = true;
        }
    }

    fn gamepad_removed(&mut self, _index: u8) {}
    fn remove_all_gamepads(&mut self) {}
    fn set_rumble_callback(&mut self, _cb: Option<RumbleCallback>) {}
}
