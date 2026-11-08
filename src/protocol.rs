//! Wire protocol shared by host and client.
//!
//! * Control channel: TCP, newline-delimited JSON ([`ControlMsg`]).
//! * Video / audio: RTP over UDP (host -> client), produced and consumed by GStreamer.
//! * Input: small binary UDP datagrams (client -> host), see [`InputEvent`].
//! * Discovery: UDP broadcast probe + JSON reply ([`DiscoveryReply`]).

use serde::{Deserialize, Serialize};

/// Bumped whenever the control protocol or input packet layout changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

pub const CONTROL_PORT: u16 = 48800;
pub const DISCOVERY_PORT: u16 = 48801;
pub const VIDEO_PORT: u16 = 48802;
pub const AUDIO_PORT: u16 = 48803;
pub const INPUT_PORT: u16 = 48804;

pub const DISCOVERY_PROBE: &[u8] = b"PCCONNECTOR-DISCOVER-1";

pub const VIDEO_PAYLOAD_TYPE: u8 = 96;
pub const AUDIO_PAYLOAD_TYPE: u8 = 97;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265 / HEVC",
        }
    }
    pub fn rtp_encoding_name(self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodecPreference {
    #[default]
    Auto,
    H264,
    H265,
}

/// What the client would like to receive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 0 = let the host pick a bitrate for the resolution / frame rate.
    pub bitrate_kbps: u32,
    pub codec: CodecPreference,
    pub audio: bool,
}

/// What the host actually decided to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub encoder: String,
    pub capture: String,
    pub audio: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    // ---- client -> host ----
    Hello {
        protocol: u32,
        app_version: String,
        client_id: String,
        client_name: String,
        /// Pairing PIN typed by the user (only needed the first time).
        pin: Option<String>,
        video_port: u16,
        audio_port: u16,
        /// Codecs the client can decode (hardware decoders first).
        decoders: Vec<Codec>,
        /// Codecs the client can decode in hardware.
        hw_decoders: Vec<Codec>,
        request: StreamRequest,
    },
    RequestKeyframe,
    Ping {
        t_us: u64,
    },
    ClientStats {
        fps: f32,
        decode_ms: f32,
        loss_pct: f32,
    },
    // ---- host -> client ----
    PairingRequired {
        message: String,
    },
    Rejected {
        reason: String,
    },
    Accepted {
        session_token: u64,
        host_name: String,
        input_port: u16,
        stream: StreamConfig,
    },
    Pong {
        t_us: u64,
    },
    HostStats {
        encode_ms: f32,
        fps: f32,
        bitrate_kbps: f32,
    },
    Rumble {
        pad: u8,
        strong: u16,
        weak: u16,
        duration_ms: u32,
    },
    // ---- either direction ----
    Bye {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryReply {
    pub app: String,
    pub protocol: u32,
    pub name: String,
    pub os: String,
    pub control_port: u16,
    pub busy: bool,
}

// ---------------------------------------------------------------------------
// Input packets
// ---------------------------------------------------------------------------

pub const INPUT_MAGIC: [u8; 4] = *b"PCI1";
const HEADER_LEN: usize = 4 + 8 + 4 + 1;

/// Mouse buttons, matching the order used on the wire.
pub mod mouse_button {
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const RIGHT: u8 = 3;
    pub const BACK: u8 = 4;
    pub const FORWARD: u8 = 5;
}

/// Xbox-style gamepad buttons bitmask.
pub mod pad_button {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const X: u16 = 1 << 2;
    pub const Y: u16 = 1 << 3;
    pub const LB: u16 = 1 << 4;
    pub const RB: u16 = 1 << 5;
    pub const BACK: u16 = 1 << 6;
    pub const START: u16 = 1 << 7;
    pub const GUIDE: u16 = 1 << 8;
    pub const LS: u16 = 1 << 9;
    pub const RS: u16 = 1 << 10;
    pub const DPAD_UP: u16 = 1 << 11;
    pub const DPAD_DOWN: u16 = 1 << 12;
    pub const DPAD_LEFT: u16 = 1 << 13;
    pub const DPAD_RIGHT: u16 = 1 << 14;
}

/// Full state of one gamepad. Sent whenever it changes (and periodically), so a lost
/// datagram is corrected by the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Stick axes, -32768..=32767. Y axes follow the Linux convention: up is negative.
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// USB HID keyboard usage id (same numbering as SDL scancodes).
    Key {
        hid: u16,
        down: bool,
    },
    MouseMove {
        dx: i32,
        dy: i32,
    },
    /// Absolute position normalised to 0..=65535 over the streamed image.
    MouseAbs {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    /// 120 units per wheel notch (like Windows / Linux hi-res wheel).
    Scroll {
        vertical: i16,
        horizontal: i16,
    },
    Gamepad {
        index: u8,
        state: GamepadState,
    },
    GamepadRemoved {
        index: u8,
    },
    /// Everything currently held down on the client. The host releases anything
    /// not in this list, which repairs the effect of any lost key-up packet.
    Snapshot {
        keys: Vec<u16>,
        mouse_buttons: u8,
    },
    /// Release every key / button (focus lost, capture released, ...).
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputPacket {
    pub token: u64,
    pub seq: u32,
    pub event: InputEvent,
}

impl InputPacket {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&INPUT_MAGIC);
        out.extend_from_slice(&self.token.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        match &self.event {
            InputEvent::Key { hid, down } => {
                out.push(1);
                out.extend_from_slice(&hid.to_le_bytes());
                out.push(*down as u8);
            }
            InputEvent::MouseMove { dx, dy } => {
                out.push(2);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            InputEvent::MouseAbs { x, y } => {
                out.push(3);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            InputEvent::MouseButton { button, down } => {
                out.push(4);
                out.push(*button);
                out.push(*down as u8);
            }
            InputEvent::Scroll { vertical, horizontal } => {
                out.push(5);
                out.extend_from_slice(&vertical.to_le_bytes());
                out.extend_from_slice(&horizontal.to_le_bytes());
            }
            InputEvent::Gamepad { index, state } => {
                out.push(6);
                out.push(*index);
                out.extend_from_slice(&state.buttons.to_le_bytes());
                out.push(state.left_trigger);
                out.push(state.right_trigger);
                for v in [state.lx, state.ly, state.rx, state.ry] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            InputEvent::GamepadRemoved { index } => {
                out.push(7);
                out.push(*index);
            }
            InputEvent::Snapshot { keys, mouse_buttons } => {
                out.push(8);
                out.push(*mouse_buttons);
                let n = keys.len().min(255);
                out.push(n as u8);
                for k in &keys[..n] {
                    out.extend_from_slice(&k.to_le_bytes());
                }
            }
            InputEvent::ReleaseAll => out.push(9),
        }
    }

    pub fn decode(buf: &[u8]) -> Option<InputPacket> {
        if buf.len() < HEADER_LEN || buf[..4] != INPUT_MAGIC {
            return None;
        }
        let token = u64::from_le_bytes(buf[4..12].try_into().ok()?);
        let seq = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        let kind = buf[16];
        let p = &buf[HEADER_LEN..];
        let u16_at = |i: usize| -> Option<u16> { Some(u16::from_le_bytes(p.get(i..i + 2)?.try_into().ok()?)) };
        let i16_at = |i: usize| -> Option<i16> { Some(i16::from_le_bytes(p.get(i..i + 2)?.try_into().ok()?)) };
        let i32_at = |i: usize| -> Option<i32> { Some(i32::from_le_bytes(p.get(i..i + 4)?.try_into().ok()?)) };
        let event = match kind {
            1 => InputEvent::Key { hid: u16_at(0)?, down: *p.get(2)? != 0 },
            2 => InputEvent::MouseMove { dx: i32_at(0)?, dy: i32_at(4)? },
            3 => InputEvent::MouseAbs { x: u16_at(0)?, y: u16_at(2)? },
            4 => InputEvent::MouseButton { button: *p.first()?, down: *p.get(1)? != 0 },
            5 => InputEvent::Scroll { vertical: i16_at(0)?, horizontal: i16_at(2)? },
            6 => InputEvent::Gamepad {
                index: *p.first()?,
                state: GamepadState {
                    buttons: u16_at(1)?,
                    left_trigger: *p.get(3)?,
                    right_trigger: *p.get(4)?,
                    lx: i16_at(5)?,
                    ly: i16_at(7)?,
                    rx: i16_at(9)?,
                    ry: i16_at(11)?,
                },
            },
            7 => InputEvent::GamepadRemoved { index: *p.first()? },
            8 => {
                let mouse_buttons = *p.first()?;
                let n = *p.get(1)? as usize;
                let mut keys = Vec::with_capacity(n);
                for i in 0..n {
                    keys.push(u16_at(2 + i * 2)?);
                }
                InputEvent::Snapshot { keys, mouse_buttons }
            }
            9 => InputEvent::ReleaseAll,
            _ => return None,
        };
        Some(InputPacket { token, seq, event })
    }
}

/// `true` if sequence number `a` is newer than `b` (wrapping arithmetic).
pub fn seq_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// Bitrate the host uses when the client asks for "auto".
pub fn default_bitrate_kbps(width: u32, height: u32, fps: u32, codec: Codec) -> u32 {
    // ~0.3 bits per pixel per frame for H.264 is visually transparent for games;
    // HEVC needs roughly 30% less for the same quality.
    let bpp = match codec {
        Codec::H264 => 0.30,
        Codec::H265 => 0.21,
    };
    let kbps = (width as f64 * height as f64 * fps as f64 * bpp / 1000.0) as u32;
    kbps.clamp(8_000, 150_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: InputEvent) {
        let pkt = InputPacket { token: 0xdead_beef_1234_5678, seq: 42, event };
        let mut buf = Vec::new();
        pkt.encode(&mut buf);
        assert_eq!(InputPacket::decode(&buf), Some(pkt));
    }

    #[test]
    fn input_roundtrip() {
        roundtrip(InputEvent::Key { hid: 0x04, down: true });
        roundtrip(InputEvent::MouseMove { dx: -5, dy: 70000 });
        roundtrip(InputEvent::MouseAbs { x: 1, y: 65535 });
        roundtrip(InputEvent::MouseButton { button: 3, down: false });
        roundtrip(InputEvent::Scroll { vertical: -120, horizontal: 240 });
        roundtrip(InputEvent::Gamepad {
            index: 2,
            state: GamepadState {
                buttons: pad_button::A | pad_button::DPAD_LEFT,
                left_trigger: 10,
                right_trigger: 255,
                lx: -32768,
                ly: 32767,
                rx: 0,
                ry: -1,
            },
        });
        roundtrip(InputEvent::GamepadRemoved { index: 1 });
        roundtrip(InputEvent::Snapshot { keys: vec![4, 5, 0xe1], mouse_buttons: 0b101 });
        roundtrip(InputEvent::ReleaseAll);
    }

    #[test]
    fn rejects_garbage() {
        assert!(InputPacket::decode(b"hello").is_none());
        assert!(InputPacket::decode(&[0u8; 40]).is_none());
        let mut buf = Vec::new();
        InputPacket { token: 1, seq: 1, event: InputEvent::Key { hid: 4, down: true } }.encode(&mut buf);
        buf.truncate(buf.len() - 1);
        assert!(InputPacket::decode(&buf).is_none());
    }

    #[test]
    fn sequence_wraps() {
        assert!(seq_newer(1, 0));
        assert!(!seq_newer(0, 1));
        assert!(seq_newer(0, u32::MAX));
        assert!(!seq_newer(5, 5));
    }

    #[test]
    fn control_json() {
        let msg = ControlMsg::Ping { t_us: 7 };
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(s, r#"{"type":"ping","t_us":7}"#);
        let back: ControlMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ControlMsg::Ping { t_us: 7 }));
    }
}
