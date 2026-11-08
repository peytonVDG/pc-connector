//! Client side of input: sends keyboard / mouse / gamepad events to the host.

use std::collections::BTreeSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use crate::keymap::*;
use crate::protocol::{InputEvent, InputPacket};

/// Hotkeys handled locally (Ctrl+Alt+Shift + key); never forwarded to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    Disconnect,       // Q
    ToggleCapture,    // Z
    ToggleMouseMode,  // M
    ToggleFullscreen, // F
    ToggleStats,      // S
}

struct Held {
    keys: BTreeSet<u16>,
    buttons: u8,
    last_snapshot: Instant,
}

pub struct InputSender {
    sock: UdpSocket,
    token: u64,
    seq: AtomicU32,
    held: Mutex<Held>,
}

impl InputSender {
    pub fn new(host: SocketAddr, token: u64) -> std::io::Result<InputSender> {
        let bind: SocketAddr = if host.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
        let sock = UdpSocket::bind(bind)?;
        sock.connect(host)?;
        crate::net::set_dscp(&sock, 46); // EF: Wi-Fi voice queue, lowest latency
        Ok(InputSender {
            sock,
            token,
            seq: AtomicU32::new(1),
            held: Mutex::new(Held { keys: BTreeSet::new(), buttons: 0, last_snapshot: Instant::now() }),
        })
    }

    pub fn send(&self, event: InputEvent) {
        {
            let mut held = self.held.lock().unwrap();
            match &event {
                InputEvent::Key { hid, down } => {
                    if *down {
                        if !held.keys.insert(*hid) {
                            return; // OS auto-repeat: the host generates its own repeats
                        }
                    } else if !held.keys.remove(hid) {
                        return; // never sent the key-down (e.g. the hotkey's own key)
                    }
                }
                InputEvent::MouseButton { button, down } if (1..=8).contains(button) => {
                    let bit = 1u8 << (button - 1);
                    if *down {
                        held.buttons |= bit;
                    } else {
                        held.buttons &= !bit;
                    }
                }
                InputEvent::ReleaseAll => {
                    held.keys.clear();
                    held.buttons = 0;
                }
                _ => {}
            }
        }
        self.send_raw(event);
    }

    fn send_raw(&self, event: InputEvent) {
        let pkt = InputPacket { token: self.token, seq: self.seq.fetch_add(1, Ordering::Relaxed), event };
        let mut buf = Vec::with_capacity(64);
        pkt.encode(&mut buf);
        let _ = self.sock.send(&buf);
    }

    /// Called ~every 100 ms: re-sends what is held (fast while something is held, slowly
    /// otherwise), so any lost key-up/down datagram is corrected quickly.
    pub fn tick(&self) {
        let snapshot = {
            let mut held = self.held.lock().unwrap();
            let busy = !held.keys.is_empty() || held.buttons != 0;
            let due = if busy { 100 } else { 1000 };
            if held.last_snapshot.elapsed().as_millis() < due {
                return;
            }
            held.last_snapshot = Instant::now();
            InputEvent::Snapshot { keys: held.keys.iter().copied().collect(), mouse_buttons: held.buttons }
        };
        self.send_raw(snapshot);
    }
}

/// Turns raw key presses into either forwarded keys or local hotkeys.
#[derive(Default)]
pub struct KeyFilter {
    ctrl: [bool; 2],
    shift: [bool; 2],
    alt: [bool; 2],
}

impl KeyFilter {
    /// Returns `Some(hotkey)` if this key press is a hotkey (and must not be forwarded).
    pub fn on_key(&mut self, hid: u16, down: bool) -> Option<Hotkey> {
        match hid {
            HID_LCTRL => self.ctrl[0] = down,
            HID_RCTRL => self.ctrl[1] = down,
            HID_LSHIFT => self.shift[0] = down,
            HID_RSHIFT => self.shift[1] = down,
            HID_LALT => self.alt[0] = down,
            HID_RALT => self.alt[1] = down,
            _ => {}
        }
        let combo = self.ctrl.iter().any(|b| *b) && self.shift.iter().any(|b| *b) && self.alt.iter().any(|b| *b);
        if !combo {
            return None;
        }
        let hk = match hid {
            0x14 => Hotkey::Disconnect,       // Q
            0x1D => Hotkey::ToggleCapture,    // Z
            0x10 => Hotkey::ToggleMouseMode,  // M
            0x09 => Hotkey::ToggleFullscreen, // F
            0x16 => Hotkey::ToggleStats,      // S
            _ => return None,
        };
        // Only the press triggers; the matching release is dropped by `InputSender::send`
        // because the press was never forwarded.
        down.then_some(hk)
    }

    pub fn reset(&mut self) {
        *self = KeyFilter::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_detection() {
        let mut f = KeyFilter::default();
        assert_eq!(f.on_key(HID_LCTRL, true), None);
        assert_eq!(f.on_key(HID_LALT, true), None);
        assert_eq!(f.on_key(0x14, true), None); // Q without shift: normal key
        assert_eq!(f.on_key(HID_RSHIFT, true), None);
        assert_eq!(f.on_key(0x14, true), Some(Hotkey::Disconnect));
        assert_eq!(f.on_key(0x14, false), None);
        assert_eq!(f.on_key(0x04, true), None); // A is not a hotkey
    }
}
