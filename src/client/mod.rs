//! Client mode: connect to a host, show its screen, play its audio, send input.

mod audio;
pub mod gamepad;
pub mod input;
pub mod kbhook;
pub mod video;
mod wlan;

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::config::{ClientSettings, ResolutionChoice};
use crate::net::{ControlReader, ControlWriter, now_us, tune_control_stream};
use crate::protocol::*;
use input::InputSender;
use video::{FrameSlot, VideoReceiver, Waker};

pub struct ConnectRequest {
    pub address: String,
    pub pin: Option<String>,
    pub client_id: String,
    pub client_name: String,
    pub settings: ClientSettings,
    /// Physical size of the monitor the window is on.
    pub display_size: (u32, u32),
}

#[allow(clippy::large_enum_variant)] // returned once per connection attempt
pub enum ConnectOutcome {
    Connected(ClientSession),
    PairingRequired { message: String },
    Rejected { reason: String },
}

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub rtt_ms: f32,
    pub host_encode_ms: f32,
    pub host_fps: f32,
    pub host_kbps: f32,
    pub decode_ms: f32,
    pub decode_fps: f32,
    pub received_kbps: f32,
    pub loss_pct: f32,
    pub decoder: String,
    pub gamepads: Vec<String>,
}

pub struct ClientSession {
    pub host_name: String,
    pub stream: StreamConfig,
    pub frames: FrameSlot,
    pub input: Arc<InputSender>,
    writer: ControlWriter,
    stats: Arc<Mutex<SessionStats>>,
    ended: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    // Field order = drop order: input stops first, pipelines last.
    _gamepads: Option<Arc<gamepad::GamepadForwarder>>,
    _audio: Option<audio::AudioReceiver>,
    _video: Arc<VideoReceiver>,
    _wlan: Option<wlan::WlanLowLatency>,
}

/// Use the preferred port if it is free, otherwise any free port.
fn pick_udp_port(preferred: u16) -> u16 {
    if UdpSocket::bind((Ipv4Addr::UNSPECIFIED, preferred)).is_ok() {
        return preferred;
    }
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(preferred)
}

pub fn requested_size(choice: ResolutionChoice, display: (u32, u32)) -> (u32, u32) {
    match choice {
        ResolutionChoice::MatchDisplay => display,
        ResolutionChoice::HostNative => (0, 0),
        c => c.fixed().unwrap_or(display),
    }
}

/// Connect and negotiate a stream. Blocks (run it off the UI thread).
pub fn connect(req: ConnectRequest, waker: Waker) -> Result<ConnectOutcome> {
    let addr = crate::net::resolve(&req.address, CONTROL_PORT)?;
    log::info!("Connecting to {addr}");
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .with_context(|| format!("could not reach {addr} - is PC Connector hosting on that PC?"))?;
    tune_control_stream(&stream);
    let writer = ControlWriter::new(stream.try_clone()?);
    let mut reader = ControlReader::new(stream.try_clone()?);

    let video_port = pick_udp_port(VIDEO_PORT);
    let audio_port = pick_udp_port(if video_port == VIDEO_PORT { AUDIO_PORT } else { 0 });
    let (decoders, hw_decoders) = crate::media::probe_decoders();
    if decoders.is_empty() {
        return Err(anyhow!("no H.264/H.265 decoder found in the GStreamer installation"));
    }
    let s = &req.settings;
    let (width, height) = requested_size(s.resolution, req.display_size);
    writer.send(&ControlMsg::Hello {
        protocol: PROTOCOL_VERSION,
        app_version: env!("CARGO_PKG_VERSION").into(),
        client_id: req.client_id.clone(),
        client_name: req.client_name.clone(),
        pin: req.pin.clone(),
        video_port,
        audio_port,
        decoders,
        hw_decoders,
        request: StreamRequest {
            width,
            height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_mbps * 1000,
            codec: s.codec,
            audio: s.audio,
        },
    })?;

    // The host may be waiting for someone to approve screen sharing on its desktop.
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let reply = reader.read()?.ok_or_else(|| anyhow!("host closed the connection"))?;
    let (token, host_name, input_port, config) = match reply {
        ControlMsg::Accepted { session_token, host_name, input_port, stream } => {
            (session_token, host_name, input_port, stream)
        }
        ControlMsg::PairingRequired { message } => return Ok(ConnectOutcome::PairingRequired { message }),
        ControlMsg::Rejected { reason } | ControlMsg::Bye { reason } => return Ok(ConnectOutcome::Rejected { reason }),
        other => return Err(anyhow!("unexpected reply from host: {other:?}")),
    };
    log::info!("Host '{host_name}' accepted: {config:?}");
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;

    let ended: Arc<Mutex<Option<String>>> = Default::default();
    let end_with = |label: &'static str| {
        let e = ended.clone();
        move |msg: String| {
            let mut slot = e.lock().unwrap();
            if slot.is_none() {
                *slot = Some(format!("{label}: {msg}"));
            }
        }
    };

    let frames = FrameSlot::default();
    let video = Arc::new(VideoReceiver::start(
        video_port,
        config.codec,
        &s.decoder_override,
        frames.clone(),
        waker,
        end_with("Video"),
    )?);
    let audio = if config.audio {
        match audio::AudioReceiver::start(audio_port, s.audio_buffer_ms, |m| log::warn!("audio stopped: {m}")) {
            Ok(a) => Some(a),
            Err(e) => {
                log::warn!("Audio playback unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let input = Arc::new(InputSender::new(SocketAddr::new(addr.ip(), input_port), token)?);
    let gamepads = Some(Arc::new(gamepad::GamepadForwarder::start(input.clone())));
    let wlan = if s.wifi_low_latency { wlan::WlanLowLatency::engage() } else { None };

    let stats = Arc::new(Mutex::new(SessionStats { decoder: video.decoder_name.clone(), ..Default::default() }));
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    // Receive thread: pongs, host stats, rumble, goodbye.
    {
        let (stop, stats, ended) = (stop.clone(), stats.clone(), ended.clone());
        let pads = gamepads.clone();
        threads.push(std::thread::Builder::new().name("control-rx".into()).spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match reader.read() {
                    Ok(Some(msg)) => match msg {
                        ControlMsg::Pong { t_us } => {
                            let rtt = now_us().saturating_sub(t_us) as f32 / 1000.0;
                            let mut st = stats.lock().unwrap();
                            st.rtt_ms = if st.rtt_ms == 0.0 { rtt } else { st.rtt_ms * 0.8 + rtt * 0.2 };
                        }
                        ControlMsg::HostStats { encode_ms, fps, bitrate_kbps } => {
                            let mut st = stats.lock().unwrap();
                            st.host_encode_ms = encode_ms;
                            st.host_fps = fps;
                            st.host_kbps = bitrate_kbps;
                        }
                        ControlMsg::Rumble { pad, strong, weak, duration_ms } => {
                            if let Some(p) = &pads {
                                p.rumble(gamepad::RumbleRequest { pad, strong, weak, duration_ms });
                            }
                        }
                        ControlMsg::Bye { reason } => {
                            ended.lock().unwrap().get_or_insert(reason);
                            break;
                        }
                        _ => {}
                    },
                    Ok(None) => {
                        ended.lock().unwrap().get_or_insert("The host closed the connection".into());
                        break;
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        ended.lock().unwrap().get_or_insert(format!("Connection lost: {e}"));
                        break;
                    }
                }
            }
        })?);
    }

    // Transmit thread: pings, stats, keyframe requests, input snapshots.
    {
        let (stop, stats, ended, writer, video, input) =
            (stop.clone(), stats.clone(), ended.clone(), writer.clone(), video.clone(), input.clone());
        let pads = gamepads.clone();
        threads.push(std::thread::Builder::new().name("control-tx".into()).spawn(move || {
            let start = Instant::now();
            let mut last_ping = Instant::now() - Duration::from_secs(1);
            let mut last_stats = Instant::now();
            let mut last_kf = Instant::now() - Duration::from_secs(1);
            let (mut last_lost, mut last_pushed) = video.packet_counts();
            let mut kf_lost_mark = last_lost;
            let mut last_bytes = 0u64;
            let mut ever_decoded = false;
            let mut request_kf = true; // the first keyframe may have been sent before we listened
            while !stop.load(Ordering::Relaxed) {
                input.tick();
                if video.take_keyframe_request() {
                    request_kf = true;
                }
                let (decode_ms, decode_fps) = video.stats();
                if decode_fps > 0.0 {
                    ever_decoded = true;
                }
                if !ever_decoded && start.elapsed() > Duration::from_millis(1500) {
                    request_kf = true;
                }
                let (lost, pushed) = video.packet_counts();
                if lost > kf_lost_mark {
                    kf_lost_mark = lost;
                    request_kf = true; // packet loss corrupts the picture until the next keyframe
                }
                if request_kf && last_kf.elapsed() > Duration::from_millis(300) {
                    last_kf = Instant::now();
                    request_kf = false;
                    if writer.send(&ControlMsg::RequestKeyframe).is_err() {
                        break;
                    }
                }
                if last_ping.elapsed() >= Duration::from_millis(500) {
                    last_ping = Instant::now();
                    if writer.send(&ControlMsg::Ping { t_us: now_us() }).is_err() {
                        ended.lock().unwrap().get_or_insert("Connection lost".into());
                        break;
                    }
                }
                if last_stats.elapsed() >= Duration::from_secs(1) {
                    let el = last_stats.elapsed().as_secs_f32();
                    last_stats = Instant::now();
                    let dl = lost.saturating_sub(last_lost) as f32;
                    let dp = pushed.saturating_sub(last_pushed) as f32;
                    let loss_pct = if dl + dp > 0.0 { dl * 100.0 / (dl + dp) } else { 0.0 };
                    last_lost = lost;
                    last_pushed = pushed;
                    let bytes = video.received_bytes();
                    let kbps = (bytes - last_bytes) as f32 * 8.0 / el / 1000.0;
                    last_bytes = bytes;
                    {
                        let mut st = stats.lock().unwrap();
                        st.decode_ms = decode_ms;
                        st.decode_fps = decode_fps;
                        st.loss_pct = loss_pct;
                        st.received_kbps = kbps;
                        if let Some(p) = &pads {
                            st.gamepads = p.connected.lock().unwrap().clone();
                        }
                    }
                    let _ = writer.send(&ControlMsg::ClientStats { fps: decode_fps, decode_ms, loss_pct });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })?);
    }

    Ok(ConnectOutcome::Connected(ClientSession {
        host_name,
        stream: config,
        frames,
        input,
        writer,
        stats,
        ended,
        stop,
        threads,
        _gamepads: gamepads,
        _audio: audio,
        _video: video,
        _wlan: wlan,
    }))
}

impl ClientSession {
    /// Why the session ended, if it has.
    pub fn ended(&self) -> Option<String> {
        self.ended.lock().unwrap().clone()
    }

    pub fn stats(&self) -> SessionStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.input.send(InputEvent::ReleaseAll);
        let _ = self.writer.send(&ControlMsg::Bye { reason: "Client disconnected".into() });
        self.stop.store(true, Ordering::Relaxed);
        self.writer.shutdown();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
