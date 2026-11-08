//! Reads local game controllers (Xbox, PlayStation, Switch Pro, ...) and forwards them
//! as Xbox-layout gamepads. Rumble requests from the host are played back here.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gilrs::{Axis, Button, EventType, GamepadId, Gilrs, GilrsBuilder};

use super::input::InputSender;
use crate::protocol::{GamepadState, InputEvent, pad_button};

pub struct RumbleRequest {
    pub pad: u8,
    pub strong: u16,
    pub weak: u16,
    pub duration_ms: u32,
}

pub struct GamepadForwarder {
    stop: Arc<AtomicBool>,
    rumble_tx: Sender<RumbleRequest>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub connected: Arc<std::sync::Mutex<Vec<String>>>,
}

impl GamepadForwarder {
    pub fn start(sender: Arc<InputSender>) -> GamepadForwarder {
        let stop = Arc::new(AtomicBool::new(false));
        let (rumble_tx, rumble_rx) = std::sync::mpsc::channel();
        let connected: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let (s, c) = (stop.clone(), connected.clone());
        let thread =
            std::thread::Builder::new().name("gamepads".into()).spawn(move || run(sender, s, rumble_rx, c)).ok();
        GamepadForwarder { stop, rumble_tx, thread, connected }
    }

    pub fn rumble(&self, req: RumbleRequest) {
        let _ = self.rumble_tx.send(req);
    }
}

impl Drop for GamepadForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Slot {
    index: u8,
    state: GamepadState,
    effect: Option<gilrs::ff::Effect>,
}

fn axis_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn button_bit(b: Button) -> Option<u16> {
    Some(match b {
        Button::South => pad_button::A,
        Button::East => pad_button::B,
        Button::West => pad_button::X,
        Button::North => pad_button::Y,
        Button::LeftTrigger => pad_button::LB,
        Button::RightTrigger => pad_button::RB,
        Button::Select => pad_button::BACK,
        Button::Start => pad_button::START,
        Button::Mode => pad_button::GUIDE,
        Button::LeftThumb => pad_button::LS,
        Button::RightThumb => pad_button::RS,
        Button::DPadUp => pad_button::DPAD_UP,
        Button::DPadDown => pad_button::DPAD_DOWN,
        Button::DPadLeft => pad_button::DPAD_LEFT,
        Button::DPadRight => pad_button::DPAD_RIGHT,
        _ => return None,
    })
}

fn set_bit(buttons: &mut u16, bit: u16, on: bool) {
    if on {
        *buttons |= bit;
    } else {
        *buttons &= !bit;
    }
}

fn run(
    sender: Arc<InputSender>,
    stop: Arc<AtomicBool>,
    rumble_rx: Receiver<RumbleRequest>,
    connected: Arc<std::sync::Mutex<Vec<String>>>,
) {
    // No client-side deadzone/jitter filters: the game applies its own deadzone.
    let mut gilrs = match GilrsBuilder::new().with_default_filters(false).build() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("Gamepad support unavailable: {e}");
            return;
        }
    };
    let mut slots: HashMap<GamepadId, Slot> = HashMap::new();
    let refresh_names = |gilrs: &Gilrs, slots: &HashMap<GamepadId, Slot>| {
        let mut names: Vec<(u8, String)> =
            slots.iter().map(|(id, s)| (s.index, gilrs.gamepad(*id).name().to_string())).collect();
        names.sort();
        *connected.lock().unwrap() = names.into_iter().map(|(i, n)| format!("#{} {n}", i + 1)).collect();
    };
    let assign = |slots: &mut HashMap<GamepadId, Slot>, id: GamepadId| -> Option<u8> {
        if let Some(s) = slots.get(&id) {
            return Some(s.index);
        }
        let used: Vec<u8> = slots.values().map(|s| s.index).collect();
        let index = (0..4u8).find(|i| !used.contains(i))?;
        slots.insert(id, Slot { index, state: GamepadState::default(), effect: None });
        Some(index)
    };

    for (id, pad) in gilrs.gamepads() {
        if pad.is_connected()
            && let Some(i) = assign(&mut slots, id)
        {
            log::info!("Controller #{} connected: {}", i + 1, pad.name());
        }
    }
    refresh_names(&gilrs, &slots);

    let mut last_heartbeat = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut dirty: Vec<GamepadId> = Vec::new();
        // Block for the first event (or 20 ms), then drain everything queued.
        let mut next = gilrs.next_event_blocking(Some(Duration::from_millis(20)));
        while let Some(ev) = next {
            let id = ev.id;
            match ev.event {
                EventType::Connected => {
                    if let Some(i) = assign(&mut slots, id) {
                        log::info!("Controller #{} connected: {}", i + 1, gilrs.gamepad(id).name());
                    }
                    refresh_names(&gilrs, &slots);
                }
                EventType::Disconnected => {
                    if let Some(slot) = slots.remove(&id) {
                        log::info!("Controller #{} disconnected", slot.index + 1);
                        for _ in 0..3 {
                            sender.send(InputEvent::GamepadRemoved { index: slot.index });
                        }
                    }
                    refresh_names(&gilrs, &slots);
                }
                other => {
                    if assign(&mut slots, id).is_none() {
                        next = gilrs.next_event();
                        continue;
                    }
                    let slot = slots.get_mut(&id).unwrap();
                    let st = &mut slot.state;
                    let before = *st;
                    match other {
                        EventType::ButtonPressed(b, _) | EventType::ButtonRepeated(b, _) => {
                            if let Some(bit) = button_bit(b) {
                                set_bit(&mut st.buttons, bit, true);
                            }
                            match b {
                                Button::LeftTrigger2 => st.left_trigger = 255,
                                Button::RightTrigger2 => st.right_trigger = 255,
                                _ => {}
                            }
                        }
                        EventType::ButtonReleased(b, _) => {
                            if let Some(bit) = button_bit(b) {
                                set_bit(&mut st.buttons, bit, false);
                            }
                            match b {
                                Button::LeftTrigger2 => st.left_trigger = 0,
                                Button::RightTrigger2 => st.right_trigger = 0,
                                _ => {}
                            }
                        }
                        EventType::ButtonChanged(b, value, _) => match b {
                            Button::LeftTrigger2 => st.left_trigger = (value.clamp(0.0, 1.0) * 255.0).round() as u8,
                            Button::RightTrigger2 => st.right_trigger = (value.clamp(0.0, 1.0) * 255.0).round() as u8,
                            _ => {
                                if let Some(bit) = button_bit(b) {
                                    set_bit(&mut st.buttons, bit, value > 0.5);
                                }
                            }
                        },
                        EventType::AxisChanged(axis, value, _) => match axis {
                            Axis::LeftStickX => st.lx = axis_i16(value),
                            // gilrs: up is positive; the wire uses Linux convention (up negative)
                            Axis::LeftStickY => st.ly = axis_i16(-value),
                            Axis::RightStickX => st.rx = axis_i16(value),
                            Axis::RightStickY => st.ry = axis_i16(-value),
                            Axis::LeftZ => st.left_trigger = (value.clamp(0.0, 1.0) * 255.0).round() as u8,
                            Axis::RightZ => st.right_trigger = (value.clamp(0.0, 1.0) * 255.0).round() as u8,
                            Axis::DPadX => {
                                set_bit(&mut st.buttons, pad_button::DPAD_LEFT, value < -0.5);
                                set_bit(&mut st.buttons, pad_button::DPAD_RIGHT, value > 0.5);
                            }
                            Axis::DPadY => {
                                set_bit(&mut st.buttons, pad_button::DPAD_UP, value > 0.5);
                                set_bit(&mut st.buttons, pad_button::DPAD_DOWN, value < -0.5);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                    if *st != before && !dirty.contains(&id) {
                        dirty.push(id);
                    }
                }
            }
            next = gilrs.next_event();
        }

        for id in dirty {
            if let Some(slot) = slots.get(&id) {
                sender.send(InputEvent::Gamepad { index: slot.index, state: slot.state });
            }
        }
        // Heartbeat: full state every 100 ms repairs any lost datagram.
        if last_heartbeat.elapsed() >= Duration::from_millis(100) {
            last_heartbeat = Instant::now();
            for slot in slots.values() {
                sender.send(InputEvent::Gamepad { index: slot.index, state: slot.state });
            }
        }
        while let Ok(req) = rumble_rx.try_recv() {
            play_rumble(&mut gilrs, &mut slots, req);
        }
    }
    for slot in slots.values() {
        sender.send(InputEvent::GamepadRemoved { index: slot.index });
    }
}

fn play_rumble(gilrs: &mut Gilrs, slots: &mut HashMap<GamepadId, Slot>, req: RumbleRequest) {
    let Some((&id, _)) = slots.iter().find(|(_, s)| s.index == req.pad) else { return };
    if !gilrs.gamepad(id).is_ff_supported() {
        return;
    }
    let slot = slots.get_mut(&id).unwrap();
    slot.effect = None; // dropping stops the previous effect
    if req.strong == 0 && req.weak == 0 {
        return;
    }
    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Repeat, Replay, Ticks};
    // 0 = "until stopped"; cap it so a lost stop can never rumble forever.
    let ms = if req.duration_ms == 0 { 5000 } else { req.duration_ms.min(10_000) };
    let replay = Replay { play_for: Ticks::from_ms(ms), ..Default::default() };
    let result = EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong { magnitude: req.strong },
            scheduling: replay,
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak { magnitude: req.weak },
            scheduling: replay,
            ..Default::default()
        })
        .repeat(Repeat::For(Ticks::from_ms(ms)))
        .gamepads(&[id])
        .finish(gilrs);
    match result {
        Ok(effect) => {
            let _ = effect.play();
            slot.effect = Some(effect);
        }
        Err(e) => log::debug!("rumble failed: {e}"),
    }
}
