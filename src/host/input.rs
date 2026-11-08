//! Host side of input: receive datagrams from the client and inject them.

use std::collections::HashSet;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::protocol::{GamepadState, InputEvent, InputPacket, seq_newer};

/// Called by an injector when a game asks a virtual gamepad to rumble:
/// (pad index, strong magnitude, weak magnitude, duration in ms; 0 = until stopped).
pub type RumbleCallback = Arc<dyn Fn(u8, u16, u16, u32) + Send + Sync>;

/// Platform specific input injection.
pub trait Injector: Send {
    fn key(&mut self, hid: u16, down: bool);
    fn mouse_move(&mut self, dx: i32, dy: i32);
    fn mouse_abs(&mut self, x: u16, y: u16);
    fn mouse_button(&mut self, button: u8, down: bool);
    fn scroll(&mut self, vertical: i16, horizontal: i16);
    fn gamepad(&mut self, index: u8, state: GamepadState);
    fn gamepad_removed(&mut self, index: u8);
    /// Remove all gamepads (session ended).
    fn remove_all_gamepads(&mut self);
    fn set_rumble_callback(&mut self, cb: Option<RumbleCallback>);
}

/// A no-op injector used when the platform backend could not be initialised,
/// so video still streams.
#[cfg_attr(windows, allow(dead_code))]
pub struct NullInjector;
impl Injector for NullInjector {
    fn key(&mut self, _: u16, _: bool) {}
    fn mouse_move(&mut self, _: i32, _: i32) {}
    fn mouse_abs(&mut self, _: u16, _: u16) {}
    fn mouse_button(&mut self, _: u8, _: bool) {}
    fn scroll(&mut self, _: i16, _: i16) {}
    fn gamepad(&mut self, _: u8, _: GamepadState) {}
    fn gamepad_removed(&mut self, _: u8) {}
    fn remove_all_gamepads(&mut self) {}
    fn set_rumble_callback(&mut self, _: Option<RumbleCallback>) {}
}

/// Create the injector for this platform. Returns the injector plus a warning to show the
/// user if input injection is unavailable.
pub fn create_injector() -> (Box<dyn Injector>, Option<String>) {
    #[cfg(target_os = "linux")]
    {
        match super::input_linux::UinputInjector::new() {
            Ok(inj) => (Box::new(inj), None),
            Err(e) => {
                let msg = format!(
                    "Keyboard/mouse/gamepad input is disabled: cannot use /dev/uinput ({e}). \
                     Run scripts/install-linux.sh (installs a udev rule) or add your user to the 'input' group, then log out and back in."
                );
                log::error!("{msg}");
                (Box::new(NullInjector), Some(msg))
            }
        }
    }
    #[cfg(windows)]
    {
        (Box::new(super::input_windows::SendInputInjector::new()), None)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        (Box::new(NullInjector), Some("Input injection is not supported on this OS".into()))
    }
}

/// Tracks what is held down so lost packets and disconnects never leave keys stuck.
struct HeldState {
    keys: HashSet<u16>,
    buttons: u8,
}

impl HeldState {
    fn apply(&mut self, inj: &mut dyn Injector, event: InputEvent) {
        match event {
            InputEvent::Key { hid, down } => {
                if down {
                    self.keys.insert(hid);
                } else {
                    self.keys.remove(&hid);
                }
                inj.key(hid, down);
            }
            InputEvent::MouseMove { dx, dy } => inj.mouse_move(dx, dy),
            InputEvent::MouseAbs { x, y } => inj.mouse_abs(x, y),
            InputEvent::MouseButton { button, down } => {
                if (1..=8).contains(&button) {
                    let bit = 1u8 << (button - 1);
                    if down {
                        self.buttons |= bit;
                    } else {
                        self.buttons &= !bit;
                    }
                }
                inj.mouse_button(button, down);
            }
            InputEvent::Scroll { vertical, horizontal } => inj.scroll(vertical, horizontal),
            InputEvent::Gamepad { index, state } => inj.gamepad(index, state),
            InputEvent::GamepadRemoved { index } => inj.gamepad_removed(index),
            InputEvent::Snapshot { keys, mouse_buttons } => {
                let want: HashSet<u16> = keys.into_iter().collect();
                let stale: Vec<u16> = self.keys.difference(&want).copied().collect();
                for hid in stale {
                    log::debug!("snapshot: releasing stuck key {hid:#x}");
                    self.keys.remove(&hid);
                    inj.key(hid, false);
                }
                let missing: Vec<u16> = want.difference(&self.keys).copied().collect();
                for hid in missing {
                    self.keys.insert(hid);
                    inj.key(hid, true);
                }
                for b in 1..=8u8 {
                    let bit = 1u8 << (b - 1);
                    let want_down = mouse_buttons & bit != 0;
                    let is_down = self.buttons & bit != 0;
                    if want_down != is_down {
                        inj.mouse_button(b, want_down);
                    }
                }
                self.buttons = mouse_buttons;
            }
            InputEvent::ReleaseAll => self.release_all(inj),
        }
    }

    fn release_all(&mut self, inj: &mut dyn Injector) {
        for hid in self.keys.drain() {
            inj.key(hid, false);
        }
        for b in 1..=8u8 {
            if self.buttons & (1 << (b - 1)) != 0 {
                inj.mouse_button(b, false);
            }
        }
        self.buttons = 0;
    }
}

/// Receives input for one session. Dropping it stops the thread and releases everything.
pub struct InputReceiver {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputReceiver {
    pub fn start(
        sock: UdpSocket,
        token: u64,
        client_ip: IpAddr,
        injector: Arc<Mutex<Box<dyn Injector>>>,
        rumble: RumbleCallback,
    ) -> std::io::Result<InputReceiver> {
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        injector.lock().unwrap().set_rumble_callback(Some(rumble));
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new().name("input-rx".into()).spawn(move || {
            raise_thread_priority();
            let mut held = HeldState { keys: HashSet::new(), buttons: 0 };
            let mut last_seq: Option<u32> = None;
            let mut buf = [0u8; 1500];
            while !s.load(Ordering::Relaxed) {
                let (n, from) = match sock.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        log::debug!("input socket: {e}");
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                if from.ip() != client_ip && !(from.ip().is_loopback() && client_ip.is_loopback()) {
                    continue;
                }
                let Some(pkt) = InputPacket::decode(&buf[..n]) else { continue };
                if pkt.token != token {
                    continue;
                }
                if let Some(prev) = last_seq
                    && !seq_newer(pkt.seq, prev)
                {
                    continue; // duplicate or reordered; snapshots repair any gap
                }
                last_seq = Some(pkt.seq);
                let mut inj = injector.lock().unwrap();
                held.apply(inj.as_mut(), pkt.event);
            }
            let mut inj = injector.lock().unwrap();
            held.release_all(inj.as_mut());
            inj.remove_all_gamepads();
            inj.set_rumble_callback(None);
        })?;
        Ok(InputReceiver { stop, thread: Some(thread) })
    }
}

impl Drop for InputReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn raise_thread_priority() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        log: Vec<String>,
    }
    impl Injector for Recorder {
        fn key(&mut self, hid: u16, down: bool) {
            self.log.push(format!("key {hid} {down}"));
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) {
            self.log.push(format!("move {dx} {dy}"));
        }
        fn mouse_abs(&mut self, _: u16, _: u16) {}
        fn mouse_button(&mut self, b: u8, down: bool) {
            self.log.push(format!("btn {b} {down}"));
        }
        fn scroll(&mut self, _: i16, _: i16) {}
        fn gamepad(&mut self, _: u8, _: GamepadState) {}
        fn gamepad_removed(&mut self, _: u8) {}
        fn remove_all_gamepads(&mut self) {}
        fn set_rumble_callback(&mut self, _: Option<RumbleCallback>) {}
    }

    #[test]
    fn snapshot_repairs_lost_key_up() {
        let mut held = HeldState { keys: HashSet::new(), buttons: 0 };
        let mut rec = Recorder::default();
        held.apply(&mut rec, InputEvent::Key { hid: 4, down: true });
        held.apply(&mut rec, InputEvent::MouseButton { button: 1, down: true });
        // key-up for 4 and button-up were "lost"; snapshot says only key 5 is held
        held.apply(&mut rec, InputEvent::Snapshot { keys: vec![5], mouse_buttons: 0 });
        assert_eq!(rec.log, vec!["key 4 true", "btn 1 true", "key 4 false", "key 5 true", "btn 1 false"]);
        rec.log.clear();
        held.apply(&mut rec, InputEvent::ReleaseAll);
        assert_eq!(rec.log, vec!["key 5 false"]);
    }

    /// Shares the recorded log with the test while the receiver owns the injector.
    struct Shared(Arc<Mutex<Vec<String>>>);
    impl Injector for Shared {
        fn key(&mut self, hid: u16, down: bool) {
            self.0.lock().unwrap().push(format!("key {hid} {down}"));
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) {
            self.0.lock().unwrap().push(format!("move {dx} {dy}"));
        }
        fn mouse_abs(&mut self, _: u16, _: u16) {}
        fn mouse_button(&mut self, b: u8, down: bool) {
            self.0.lock().unwrap().push(format!("btn {b} {down}"));
        }
        fn scroll(&mut self, _: i16, _: i16) {}
        fn gamepad(&mut self, i: u8, s: GamepadState) {
            self.0.lock().unwrap().push(format!("pad {i} {:#x}", s.buttons));
        }
        fn gamepad_removed(&mut self, _: u8) {}
        fn remove_all_gamepads(&mut self) {}
        fn set_rumble_callback(&mut self, _: Option<RumbleCallback>) {}
    }

    #[test]
    fn udp_end_to_end() {
        use crate::client::input::InputSender;
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let injector: Arc<Mutex<Box<dyn Injector>>> = Arc::new(Mutex::new(Box::new(Shared(log.clone()))));
        let rx = InputReceiver::start(sock, 42, addr.ip(), injector, Arc::new(|_, _, _, _| {})).unwrap();

        // Packets with the wrong session token must be ignored.
        let intruder = InputSender::new(addr, 7).unwrap();
        intruder.send(InputEvent::Key { hid: 0x04, down: true });

        let tx = InputSender::new(addr, 42).unwrap();
        tx.send(InputEvent::Key { hid: 0x04, down: true });
        tx.send(InputEvent::Key { hid: 0x04, down: true }); // auto-repeat: not re-sent
        tx.send(InputEvent::MouseMove { dx: 3, dy: -2 });
        tx.send(InputEvent::Gamepad { index: 0, state: GamepadState { buttons: 1, ..Default::default() } });
        tx.send(InputEvent::Key { hid: 0x04, down: false });
        tx.send(InputEvent::Key { hid: 0x05, down: true });
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(rx); // session end: everything still held is released
        let log = log.lock().unwrap().clone();
        assert_eq!(log, vec!["key 4 true", "move 3 -2", "pad 0 0x1", "key 4 false", "key 5 true", "key 5 false"]);
    }
}
