//! Persistent settings (JSON in the platform config directory).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::CodecPreference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// Wayland -> PipeWire portal, X11 -> XSHM, Windows -> Desktop Duplication.
    #[default]
    Auto,
    X11,
    Portal,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncoderPreference {
    #[default]
    Auto,
    Nvidia,
    Vaapi,
    IntelQsv,
    Amd,
    MediaFoundation,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionChoice {
    /// The resolution of the monitor the client window is on.
    #[default]
    MatchDisplay,
    /// Whatever the host's desktop is (no scaling).
    HostNative,
    R720p,
    R1080p,
    R1440p,
    R2160p,
}

impl ResolutionChoice {
    pub const ALL: [ResolutionChoice; 6] = [
        ResolutionChoice::MatchDisplay,
        ResolutionChoice::HostNative,
        ResolutionChoice::R720p,
        ResolutionChoice::R1080p,
        ResolutionChoice::R1440p,
        ResolutionChoice::R2160p,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ResolutionChoice::MatchDisplay => "Match this display",
            ResolutionChoice::HostNative => "Host native (no scaling)",
            ResolutionChoice::R720p => "1280 x 720",
            ResolutionChoice::R1080p => "1920 x 1080",
            ResolutionChoice::R1440p => "2560 x 1440",
            ResolutionChoice::R2160p => "3840 x 2160",
        }
    }
    pub fn fixed(self) -> Option<(u32, u32)> {
        match self {
            ResolutionChoice::R720p => Some((1280, 720)),
            ResolutionChoice::R1080p => Some((1920, 1080)),
            ResolutionChoice::R1440p => Some((2560, 1440)),
            ResolutionChoice::R2160p => Some((3840, 2160)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub resolution: ResolutionChoice,
    pub fps: u32,
    /// 0 = automatic.
    pub bitrate_mbps: u32,
    pub codec: CodecPreference,
    pub audio: bool,
    pub fullscreen: bool,
    /// Wait for vertical blank (no tearing, ~+1 frame latency in the worst case).
    pub vsync: bool,
    pub show_stats: bool,
    /// Start in relative (game) mouse mode instead of absolute (desktop) mode.
    pub relative_mouse: bool,
    /// Windows only: capture system keys (Win, Alt+Tab, ...) while streaming.
    pub capture_system_keys: bool,
    /// Windows only: put Wi-Fi into low-latency media streaming mode while streaming.
    pub wifi_low_latency: bool,
    /// Audio jitter buffer in milliseconds.
    pub audio_buffer_ms: u32,
    /// Override the decoder element (empty = automatic).
    pub decoder_override: String,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            resolution: ResolutionChoice::MatchDisplay,
            fps: 60,
            bitrate_mbps: 0,
            codec: CodecPreference::Auto,
            audio: true,
            fullscreen: true,
            vsync: false,
            show_stats: false,
            relative_mouse: true,
            capture_system_keys: true,
            wifi_low_latency: true,
            audio_buffer_ms: 30,
            decoder_override: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    pub name: String,
    pub capture: CaptureMethod,
    pub encoder: EncoderPreference,
    /// X11 display to capture (empty = $DISPLAY).
    pub x11_display: String,
    /// Windows: monitor index to capture.
    pub monitor_index: u32,
    pub show_cursor: bool,
    /// Route the host's audio to a silent virtual sink while a client is connected.
    pub mute_host_audio: bool,
    pub require_pairing: bool,
    pub paired_clients: Vec<PairedClient>,
    /// PipeWire portal restore token, so the "share screen" dialog only appears once.
    pub portal_restore_token: Option<String>,
    /// Audio bitrate for Opus.
    pub audio_bitrate_kbps: u32,
    /// Keyframe safety-net interval in seconds (keyframes are also sent on demand).
    pub keyframe_interval_s: u32,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            name: crate::net::hostname(),
            capture: CaptureMethod::Auto,
            encoder: EncoderPreference::Auto,
            x11_display: String::new(),
            monitor_index: 0,
            show_cursor: true,
            mute_host_audio: false,
            require_pairing: true,
            paired_clients: Vec::new(),
            portal_restore_token: None,
            audio_bitrate_kbps: 192,
            keyframe_interval_s: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentHost {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Random id identifying this installation when it acts as a client.
    pub client_id: String,
    pub client_name: String,
    pub client: ClientSettings,
    pub host: HostSettings,
    pub recent_hosts: Vec<RecentHost>,
    /// Which tab the launcher opens on ("client" / "host"); empty = choose by OS.
    pub last_role: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: random_hex(16),
            client_name: crate::net::hostname(),
            client: ClientSettings::default(),
            host: HostSettings::default(),
            recent_hosts: Vec::new(),
            last_role: String::new(),
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "pc-connector")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    if cfg.client_id.len() < 16 {
                        cfg.client_id = random_hex(16);
                    }
                    cfg
                }
                Err(e) => {
                    log::warn!("Ignoring unreadable config {}: {e}", path.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                cfg.save();
                cfg
            }
        }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                // Write atomically so a crash never leaves a truncated config behind.
                let tmp = path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
                    log::warn!("Could not save config to {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("Could not serialise config: {e}"),
        }
    }

    pub fn remember_host(&mut self, name: &str, address: &str) {
        self.recent_hosts.retain(|h| h.address != address);
        self.recent_hosts.insert(0, RecentHost { name: name.to_string(), address: address.to_string() });
        self.recent_hosts.truncate(8);
    }
}
