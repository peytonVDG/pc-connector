//! Linux input injection through /dev/uinput: a virtual keyboard, a relative mouse,
//! an absolute pointer (for desktop use) and Xbox 360 compatible gamepads with rumble.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode, FFEffectKind, InputEvent,
    InputId, KeyCode, RelativeAxisCode, UinputAbsSetup,
};

use super::input::{Injector, RumbleCallback};
use crate::keymap::{all_evdev_keys, hid_to_evdev};
use crate::protocol::{GamepadState, pad_button};

const MAX_PADS: usize = 4;
const ABS_MAX: i32 = 32767;

struct Pad {
    device: Arc<Mutex<VirtualDevice>>,
    last: GamepadState,
    stop_ff: Arc<AtomicBool>,
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.stop_ff.store(true, Ordering::Relaxed);
    }
}

pub struct UinputInjector {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    pointer: VirtualDevice,
    pads: [Option<Pad>; MAX_PADS],
    /// Whether the last pointer motion was absolute (buttons go to that device).
    absolute_active: bool,
    rumble: Option<RumbleCallback>,
    pad_error_logged: bool,
}

fn keys(codes: impl IntoIterator<Item = u16>) -> AttributeSet<KeyCode> {
    codes.into_iter().map(KeyCode).collect()
}

const MOUSE_BUTTONS: [u16; 5] =
    [KeyCode::BTN_LEFT.0, KeyCode::BTN_RIGHT.0, KeyCode::BTN_MIDDLE.0, KeyCode::BTN_SIDE.0, KeyCode::BTN_EXTRA.0];

impl UinputInjector {
    pub fn new() -> Result<Self> {
        let keyboard = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name("PC Connector Keyboard")
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0x5043, 1))
            .with_keys(&keys(all_evdev_keys()))?
            .build()
            .context("create virtual keyboard")?;

        let rel: AttributeSet<RelativeAxisCode> = [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ]
        .into_iter()
        .collect();
        let mouse = VirtualDevice::builder()?
            .name("PC Connector Mouse")
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0x5044, 1))
            .with_keys(&keys(MOUSE_BUTTONS))?
            .with_relative_axes(&rel)?
            .build()
            .context("create virtual mouse")?;

        // Modelled on QEMU's USB tablet, which every desktop treats as an absolute pointer.
        let abs = |code| UinputAbsSetup::new(code, AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0));
        let wheel: AttributeSet<RelativeAxisCode> = [RelativeAxisCode::REL_WHEEL].into_iter().collect();
        let pointer = VirtualDevice::builder()?
            .name("PC Connector Absolute Pointer")
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0x5045, 1))
            .with_keys(&keys(MOUSE_BUTTONS))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y))?
            .with_relative_axes(&wheel)?
            .build()
            .context("create virtual absolute pointer")?;

        log::info!("uinput virtual keyboard/mouse ready");
        Ok(Self {
            keyboard,
            mouse,
            pointer,
            pads: Default::default(),
            absolute_active: false,
            rumble: None,
            pad_error_logged: false,
        })
    }

    fn create_pad(&mut self, index: usize) -> Result<Pad> {
        let stick = |code| UinputAbsSetup::new(code, AbsInfo::new(0, -32768, 32767, 16, 128, 0));
        let trigger = |code| UinputAbsSetup::new(code, AbsInfo::new(0, 0, 255, 0, 0, 0));
        let hat = |code| UinputAbsSetup::new(code, AbsInfo::new(0, -1, 1, 0, 0, 0));
        let buttons = keys([
            KeyCode::BTN_SOUTH.0,
            KeyCode::BTN_EAST.0,
            KeyCode::BTN_NORTH.0,
            KeyCode::BTN_WEST.0,
            KeyCode::BTN_TL.0,
            KeyCode::BTN_TR.0,
            KeyCode::BTN_SELECT.0,
            KeyCode::BTN_START.0,
            KeyCode::BTN_MODE.0,
            KeyCode::BTN_THUMBL.0,
            KeyCode::BTN_THUMBR.0,
        ]);
        let ff: AttributeSet<FFEffectCode> = [FFEffectCode::FF_RUMBLE].into_iter().collect();
        // Same identity as the kernel's xpad driver so SDL/Steam/Proton use their Xbox mapping.
        let device = VirtualDevice::builder()?
            .name("Microsoft X-Box 360 pad")
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&buttons)?
            .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_X))?
            .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_Y))?
            .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RX))?
            .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RY))?
            .with_absolute_axis(&trigger(AbsoluteAxisCode::ABS_Z))?
            .with_absolute_axis(&trigger(AbsoluteAxisCode::ABS_RZ))?
            .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0X))?
            .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0Y))?
            .with_ff(&ff)?
            .with_ff_effects_max(16)
            .build()
            .context("create virtual gamepad")?;
        let fd = device.as_raw_fd();
        let device = Arc::new(Mutex::new(device));
        let stop_ff = Arc::new(AtomicBool::new(false));
        spawn_ff_thread(index as u8, fd, device.clone(), stop_ff.clone(), self.rumble.clone());
        log::info!("Virtual Xbox 360 gamepad #{index} connected");
        Ok(Pad { device, last: GamepadState::default(), stop_ff })
    }

    fn emit(dev: &mut VirtualDevice, events: &[InputEvent]) {
        if let Err(e) = dev.emit(events) {
            log::debug!("uinput emit failed: {e}");
        }
    }
}

fn ev(kind: EventType, code: u16, value: i32) -> InputEvent {
    InputEvent::new(kind.0, code, value)
}

impl Injector for UinputInjector {
    fn key(&mut self, hid: u16, down: bool) {
        if let Some(code) = hid_to_evdev(hid) {
            Self::emit(&mut self.keyboard, &[ev(EventType::KEY, code, down as i32)]);
        }
    }

    fn mouse_move(&mut self, dx: i32, dy: i32) {
        self.absolute_active = false;
        let mut events = Vec::with_capacity(2);
        if dx != 0 {
            events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_X.0, dx));
        }
        if dy != 0 {
            events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_Y.0, dy));
        }
        if !events.is_empty() {
            Self::emit(&mut self.mouse, &events);
        }
    }

    fn mouse_abs(&mut self, x: u16, y: u16) {
        self.absolute_active = true;
        let scale = |v: u16| (v as i64 * ABS_MAX as i64 / 65535) as i32;
        Self::emit(
            &mut self.pointer,
            &[
                ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, scale(x)),
                ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_Y.0, scale(y)),
            ],
        );
    }

    fn mouse_button(&mut self, button: u8, down: bool) {
        let code = match button {
            1 => KeyCode::BTN_LEFT,
            2 => KeyCode::BTN_MIDDLE,
            3 => KeyCode::BTN_RIGHT,
            4 => KeyCode::BTN_SIDE,
            5 => KeyCode::BTN_EXTRA,
            _ => return,
        };
        let dev = if self.absolute_active { &mut self.pointer } else { &mut self.mouse };
        Self::emit(dev, &[ev(EventType::KEY, code.0, down as i32)]);
    }

    fn scroll(&mut self, vertical: i16, horizontal: i16) {
        let mut events = Vec::new();
        if vertical != 0 {
            events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL_HI_RES.0, vertical as i32));
            let notches = vertical as i32 / 120;
            if notches != 0 {
                events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, notches));
            }
        }
        if horizontal != 0 {
            events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL_HI_RES.0, horizontal as i32));
            let notches = horizontal as i32 / 120;
            if notches != 0 {
                events.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL.0, notches));
            }
        }
        if !events.is_empty() {
            Self::emit(&mut self.mouse, &events);
        }
    }

    fn gamepad(&mut self, index: u8, state: GamepadState) {
        let i = index as usize;
        if i >= MAX_PADS {
            return;
        }
        if self.pads[i].is_none() {
            match self.create_pad(i) {
                Ok(p) => self.pads[i] = Some(p),
                Err(e) => {
                    if !self.pad_error_logged {
                        log::error!("Could not create virtual gamepad: {e:#}");
                        self.pad_error_logged = true;
                    }
                    return;
                }
            }
        }
        let pad = self.pads[i].as_mut().unwrap();
        let prev = pad.last;
        let first = prev == GamepadState::default() && state != prev;
        let mut events = Vec::with_capacity(16);
        let button_map = [
            (pad_button::A, KeyCode::BTN_SOUTH),
            (pad_button::B, KeyCode::BTN_EAST),
            (pad_button::X, KeyCode::BTN_NORTH), // xpad reports X as BTN_X (== BTN_NORTH)
            (pad_button::Y, KeyCode::BTN_WEST),  // and Y as BTN_Y (== BTN_WEST)
            (pad_button::LB, KeyCode::BTN_TL),
            (pad_button::RB, KeyCode::BTN_TR),
            (pad_button::BACK, KeyCode::BTN_SELECT),
            (pad_button::START, KeyCode::BTN_START),
            (pad_button::GUIDE, KeyCode::BTN_MODE),
            (pad_button::LS, KeyCode::BTN_THUMBL),
            (pad_button::RS, KeyCode::BTN_THUMBR),
        ];
        for (bit, code) in button_map {
            let now = state.buttons & bit != 0;
            if now != (prev.buttons & bit != 0) || first {
                events.push(ev(EventType::KEY, code.0, now as i32));
            }
        }
        let hat_x =
            (state.buttons & pad_button::DPAD_RIGHT != 0) as i32 - (state.buttons & pad_button::DPAD_LEFT != 0) as i32;
        let hat_y =
            (state.buttons & pad_button::DPAD_DOWN != 0) as i32 - (state.buttons & pad_button::DPAD_UP != 0) as i32;
        let prev_hat_x =
            (prev.buttons & pad_button::DPAD_RIGHT != 0) as i32 - (prev.buttons & pad_button::DPAD_LEFT != 0) as i32;
        let prev_hat_y =
            (prev.buttons & pad_button::DPAD_DOWN != 0) as i32 - (prev.buttons & pad_button::DPAD_UP != 0) as i32;
        let axes = [
            (AbsoluteAxisCode::ABS_X, state.lx as i32, prev.lx as i32),
            (AbsoluteAxisCode::ABS_Y, state.ly as i32, prev.ly as i32),
            (AbsoluteAxisCode::ABS_RX, state.rx as i32, prev.rx as i32),
            (AbsoluteAxisCode::ABS_RY, state.ry as i32, prev.ry as i32),
            (AbsoluteAxisCode::ABS_Z, state.left_trigger as i32, prev.left_trigger as i32),
            (AbsoluteAxisCode::ABS_RZ, state.right_trigger as i32, prev.right_trigger as i32),
            (AbsoluteAxisCode::ABS_HAT0X, hat_x, prev_hat_x),
            (AbsoluteAxisCode::ABS_HAT0Y, hat_y, prev_hat_y),
        ];
        for (code, now, before) in axes {
            if now != before || first {
                events.push(ev(EventType::ABSOLUTE, code.0, now));
            }
        }
        pad.last = state;
        if !events.is_empty() {
            let mut dev = pad.device.lock().unwrap();
            Self::emit(&mut dev, &events);
        }
    }

    fn gamepad_removed(&mut self, index: u8) {
        if let Some(slot) = self.pads.get_mut(index as usize)
            && slot.take().is_some()
        {
            log::info!("Virtual gamepad #{index} removed");
        }
    }

    fn remove_all_gamepads(&mut self) {
        for i in 0..MAX_PADS {
            self.gamepad_removed(i as u8);
        }
    }

    fn set_rumble_callback(&mut self, cb: Option<RumbleCallback>) {
        self.rumble = cb;
    }
}

/// Services force-feedback requests from games: uploads/erases must be acknowledged
/// (the game blocks until they are) and "play" requests are forwarded to the client.
fn spawn_ff_thread(
    pad: u8,
    fd: RawFd,
    device: Arc<Mutex<VirtualDevice>>,
    stop: Arc<AtomicBool>,
    rumble: Option<RumbleCallback>,
) {
    let _ = std::thread::Builder::new().name(format!("ff-pad{pad}")).spawn(move || {
        let mut effects: HashMap<i16, (u16, u16, u32)> = HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: valid pollfd array of length 1; the fd stays open while `device` lives.
            let r = unsafe { libc::poll(&mut pfd, 1, 100) };
            if r <= 0 || pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut dev = device.lock().unwrap();
            let events: Vec<InputEvent> = match dev.fetch_events() {
                Ok(it) => it.collect(),
                Err(_) => continue,
            };
            for event in events {
                match event.destructure() {
                    EventSummary::UInput(uev, code, _) if code == evdev::UInputCode::UI_FF_UPLOAD => {
                        if let Ok(mut upload) = dev.process_ff_upload(uev) {
                            let data = upload.effect();
                            let (strong, weak) = match data.kind {
                                FFEffectKind::Rumble { strong_magnitude, weak_magnitude } => {
                                    (strong_magnitude, weak_magnitude)
                                }
                                _ => (0, 0),
                            };
                            effects.insert(upload.effect_id(), (strong, weak, data.replay.length as u32));
                            upload.set_retval(0);
                        }
                    }
                    EventSummary::UInput(uev, code, _) if code == evdev::UInputCode::UI_FF_ERASE => {
                        if let Ok(mut erase) = dev.process_ff_erase(uev) {
                            effects.remove(&(erase.effect_id() as i16));
                            erase.set_retval(0);
                        }
                    }
                    EventSummary::ForceFeedback(_, code, value) => {
                        if code == FFEffectCode::FF_GAIN {
                            continue;
                        }
                        let id = code.0 as i16;
                        if let (Some(cb), Some(&(strong, weak, len))) = (&rumble, effects.get(&id)) {
                            if value > 0 {
                                cb(pad, strong, weak, len);
                            } else {
                                cb(pad, 0, 0, 0);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });
}
