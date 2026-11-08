//! Host mode: share this PC's screen, audio and accept remote input.

mod audio;
mod input;
#[cfg(target_os = "linux")]
mod input_linux;
#[cfg(windows)]
mod input_windows;
#[cfg(target_os = "linux")]
mod portal;
pub mod video;

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::config::{Config, PairedClient};
use crate::net::{ControlReader, ControlWriter, tune_control_stream};
use crate::protocol::*;
use input::{Injector, InputReceiver, RumbleCallback};

#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub addresses: Vec<String>,
    pub client: Option<String>,
    pub stream: Option<StreamConfig>,
    /// PIN to type on a new device (shown while a device is waiting to pair).
    pub pin: Option<String>,
    pub pin_for: Option<String>,
    pub encode_ms: f32,
    pub fps: f32,
    pub kbps: f32,
    pub client_decode_ms: f32,
    pub client_loss_pct: f32,
    pub warnings: Vec<String>,
    pub last_error: Option<String>,
    pub encoders: Vec<String>,
}

struct Shared {
    cfg: Arc<Mutex<Config>>,
    status: Arc<Mutex<HostStatus>>,
    stop: Arc<AtomicBool>,
    injector: Arc<Mutex<Box<dyn Injector>>>,
    input_sock: UdpSocket,
    /// Incremented for each session; a session ends when this no longer matches its id.
    active_session: AtomicU64,
    session_lock: Mutex<()>,
    pin: Mutex<Option<(String, Instant, u32)>>,
    discovery: Arc<Mutex<DiscoveryReply>>,
}

pub struct HostServer {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl HostServer {
    pub fn start(cfg: Arc<Mutex<Config>>) -> Result<HostServer> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, CONTROL_PORT))
            .with_context(|| format!("cannot listen on TCP port {CONTROL_PORT} (is another host already running?)"))?;
        listener.set_nonblocking(true)?;
        let input_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, INPUT_PORT))
            .with_context(|| format!("cannot listen on UDP port {INPUT_PORT}"))?;
        crate::net::set_buffers(&input_sock, 1 << 20, 0);

        let (injector, warning) = input::create_injector();
        let name = cfg.lock().unwrap().host.name.clone();
        let status = Arc::new(Mutex::new(HostStatus {
            addresses: crate::net::local_ipv4_addrs().iter().map(|a| a.to_string()).collect(),
            warnings: warning.into_iter().collect(),
            ..Default::default()
        }));
        let discovery = Arc::new(Mutex::new(DiscoveryReply {
            app: "pc-connector".into(),
            protocol: PROTOCOL_VERSION,
            name,
            os: crate::net::os_name().into(),
            control_port: CONTROL_PORT,
            busy: false,
        }));
        let shared = Arc::new(Shared {
            cfg,
            status,
            stop: Arc::new(AtomicBool::new(false)),
            injector: Arc::new(Mutex::new(injector)),
            input_sock,
            active_session: AtomicU64::new(0),
            session_lock: Mutex::new(()),
            pin: Mutex::new(None),
            discovery: discovery.clone(),
        });

        let mut threads = Vec::new();
        match crate::discovery::spawn_responder(discovery, shared.stop.clone()) {
            Ok(t) => threads.push(t),
            Err(e) => log::warn!("LAN discovery disabled (UDP {DISCOVERY_PORT}): {e}"),
        }
        // Probe encoders in the background so the first connection is quick.
        {
            let st = shared.status.clone();
            threads.push(std::thread::spawn(move || {
                let names: Vec<String> =
                    video::working_encoders().iter().map(|e| format!("{} ({})", e.element, e.codec.label())).collect();
                let mut s = st.lock().unwrap();
                if names.is_empty() {
                    s.warnings.push(
                        "No working video encoder found - install the GStreamer plugins listed in the README.".into(),
                    );
                }
                s.encoders = names;
            }));
        }
        let sh = shared.clone();
        threads
            .push(std::thread::Builder::new().name("control-accept".into()).spawn(move || accept_loop(listener, sh))?);
        log::info!("Host ready. Addresses: {:?}, control port {CONTROL_PORT}", shared.status.lock().unwrap().addresses);
        Ok(HostServer { shared, threads })
    }

    pub fn status(&self) -> HostStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Disconnect the current client, if any.
    pub fn kick(&self) {
        self.shared.active_session.fetch_add(1, Ordering::SeqCst);
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.active_session.fetch_add(1, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for HostServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let sh = shared.clone();
                let _ = std::thread::Builder::new().name("control-session".into()).spawn(move || {
                    if let Err(e) = handle_connection(stream, peer, &sh) {
                        log::warn!("Session with {peer} ended with error: {e:#}");
                        sh.status.lock().unwrap().last_error = Some(format!("{e:#}"));
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::warn!("accept failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn notify_desktop(title: &str, body: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=PC Connector", "--urgency=critical", title, body])
            .spawn();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (title, body);
    }
}

/// Returns Ok(true) if the client is (now) paired.
fn check_pairing(shared: &Shared, client_id: &str, client_name: &str, pin: Option<&str>) -> bool {
    {
        let cfg = shared.cfg.lock().unwrap();
        if !cfg.host.require_pairing || cfg.host.paired_clients.iter().any(|c| c.id == client_id) {
            return true;
        }
    }
    let mut guard = shared.pin.lock().unwrap();
    // Expire PINs after 5 minutes.
    if guard.as_ref().is_some_and(|(_, t, _)| t.elapsed() > Duration::from_secs(300)) {
        *guard = None;
    }
    if let (Some(given), Some((expected, _, attempts))) = (pin, guard.as_mut()) {
        if given.trim() == expected {
            *guard = None;
            drop(guard);
            let mut cfg = shared.cfg.lock().unwrap();
            cfg.host.paired_clients.retain(|c| c.id != client_id);
            cfg.host.paired_clients.push(PairedClient { id: client_id.to_string(), name: client_name.to_string() });
            cfg.save();
            let mut st = shared.status.lock().unwrap();
            st.pin = None;
            st.pin_for = None;
            log::info!("Paired new device '{client_name}'");
            return true;
        }
        *attempts += 1;
        if *attempts >= 5 {
            *guard = None; // too many wrong guesses: issue a fresh PIN
        }
    }
    if guard.is_none() {
        let n: u32 = rand::random_range(0..10000);
        let new_pin = format!("{n:04}");
        log::warn!("==> Pairing request from '{client_name}'. PIN: {new_pin}");
        notify_desktop(
            &format!("PC Connector pairing PIN: {new_pin}"),
            &format!("Enter this PIN on '{client_name}' to allow it to control this PC."),
        );
        *guard = Some((new_pin, Instant::now(), 0));
    }
    let mut st = shared.status.lock().unwrap();
    st.pin = guard.as_ref().map(|(p, _, _)| p.clone());
    st.pin_for = Some(client_name.to_string());
    false
}

fn handle_connection(stream: TcpStream, peer: SocketAddr, shared: &Arc<Shared>) -> Result<()> {
    tune_control_stream(&stream);
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let writer = ControlWriter::new(stream.try_clone()?);
    let mut reader = ControlReader::new(stream.try_clone()?);

    let hello = match reader.read()? {
        Some(m @ ControlMsg::Hello { .. }) => m,
        Some(other) => return Err(anyhow!("expected hello, got {other:?}")),
        None => return Ok(()),
    };
    let ControlMsg::Hello {
        protocol,
        app_version,
        client_id,
        client_name,
        pin,
        video_port,
        audio_port,
        decoders,
        hw_decoders,
        request,
    } = hello
    else {
        unreachable!()
    };
    log::info!("Connection from '{client_name}' ({peer}), app {app_version}");
    if protocol != PROTOCOL_VERSION {
        writer.send(&ControlMsg::Rejected {
            reason: format!(
                "Version mismatch: host uses protocol {PROTOCOL_VERSION}, client {protocol}. Install the same PC Connector version on both."
            ),
        })?;
        return Ok(());
    }
    if !check_pairing(shared, &client_id, &client_name, pin.as_deref()) {
        let host_name = shared.cfg.lock().unwrap().host.name.clone();
        writer.send(&ControlMsg::PairingRequired {
            message: format!(
                "First time connecting: type the 4-digit PIN shown on '{host_name}' (on its screen or in the host window/terminal)."
            ),
        })?;
        return Ok(());
    }

    // One client at a time: take over from any existing session.
    let my_id = shared.active_session.fetch_add(1, Ordering::SeqCst) + 1;
    let _session_guard = shared.session_lock.lock().unwrap();
    if shared.active_session.load(Ordering::SeqCst) != my_id || shared.stop.load(Ordering::Relaxed) {
        return Ok(()); // an even newer connection arrived meanwhile
    }
    let still_active = || shared.active_session.load(Ordering::SeqCst) == my_id && !shared.stop.load(Ordering::Relaxed);
    // Short read timeout so the session loop reacts quickly to errors and stop requests.
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;

    let result = run_session(
        shared,
        &writer,
        &mut reader,
        peer,
        &client_name,
        video_port,
        audio_port,
        &decoders,
        &hw_decoders,
        &request,
        &still_active,
    );
    if let Err(e) = &result {
        let _ = writer.send(&ControlMsg::Bye { reason: format!("{e:#}") });
    }
    writer.shutdown();
    {
        let mut st = shared.status.lock().unwrap();
        st.client = None;
        st.stream = None;
        st.fps = 0.0;
        st.kbps = 0.0;
    }
    shared.discovery.lock().unwrap().busy = false;
    log::info!("Session with '{client_name}' closed");
    result
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    shared: &Arc<Shared>,
    writer: &ControlWriter,
    reader: &mut ControlReader,
    peer: SocketAddr,
    client_name: &str,
    video_port: u16,
    audio_port: u16,
    decoders: &[Codec],
    hw_decoders: &[Codec],
    request: &StreamRequest,
    still_active: &dyn Fn() -> bool,
) -> Result<()> {
    let settings = shared.cfg.lock().unwrap().host.clone();
    let encoder = video::choose_encoder(settings.encoder, request.codec, decoders, hw_decoders)?;

    let (capture, token) = video::Capture::open(&settings)?;
    if let Some(token) = token {
        let mut cfg = shared.cfg.lock().unwrap();
        if cfg.host.portal_restore_token.as_deref() != Some(token.as_str()) {
            cfg.host.portal_restore_token = Some(token);
            cfg.save();
        }
    }
    let native = capture.native_size().unwrap_or((1920, 1080));
    let (width, height) = video::fit_size(native, (request.width, request.height));
    let fps = request.fps.clamp(10, 240);
    let bitrate_kbps = if request.bitrate_kbps > 0 {
        request.bitrate_kbps.clamp(1000, 500_000)
    } else {
        default_bitrate_kbps(width, height, fps, encoder.codec)
    };

    let session_error: Arc<Mutex<Option<String>>> = Default::default();
    let err_sink = |label: &'static str| {
        let e = session_error.clone();
        move |msg: String| {
            let mut slot = e.lock().unwrap();
            if slot.is_none() {
                *slot = Some(format!("{label}: {msg}"));
            }
        }
    };

    let video = video::VideoPipeline::start(
        &video::VideoParams {
            capture: &capture,
            encoder,
            width,
            height,
            fps,
            bitrate_kbps,
            keyframe_interval_s: settings.keyframe_interval_s,
            show_cursor: settings.show_cursor,
            client_ip: peer.ip(),
            client_port: video_port,
        },
        err_sink("video"),
    )?;

    let _mute = if request.audio && settings.mute_host_audio { audio::HostAudioMute::engage() } else { None };
    let audio = if request.audio {
        match audio::AudioPipeline::start(peer.ip(), audio_port, settings.audio_bitrate_kbps, |m| {
            log::warn!("audio stopped: {m}")
        }) {
            Ok(a) => Some(a),
            Err(e) => {
                log::warn!("Audio disabled: {e:#}");
                shared.status.lock().unwrap().warnings.push(format!("Audio disabled: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    let token: u64 = rand::random();
    let rumble_writer = writer.clone();
    let rumble: RumbleCallback = Arc::new(move |pad, strong, weak, duration_ms| {
        let _ = rumble_writer.send(&ControlMsg::Rumble { pad, strong, weak, duration_ms });
    });
    let _input =
        InputReceiver::start(shared.input_sock.try_clone()?, token, peer.ip(), shared.injector.clone(), rumble)?;

    let stream = StreamConfig {
        width,
        height,
        fps,
        bitrate_kbps,
        codec: encoder.codec,
        encoder: encoder.element.to_string(),
        capture: capture.label(),
        audio: audio.is_some(),
    };
    log::info!("Streaming to '{client_name}': {stream:?}");
    writer.send(&ControlMsg::Accepted {
        session_token: token,
        host_name: settings.name.clone(),
        input_port: INPUT_PORT,
        stream: stream.clone(),
    })?;
    {
        let mut st = shared.status.lock().unwrap();
        st.client = Some(format!("{client_name} ({})", peer.ip()));
        st.stream = Some(stream);
        st.last_error = None;
    }
    shared.discovery.lock().unwrap().busy = true;

    // Control loop.
    let mut last_rx = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_keyframe = Instant::now() - Duration::from_secs(1);
    loop {
        if !still_active() {
            let _ = writer.send(&ControlMsg::Bye { reason: "Host ended the session".into() });
            return Ok(());
        }
        if let Some(err) = session_error.lock().unwrap().take() {
            return Err(anyhow!(err));
        }
        if last_rx.elapsed() > Duration::from_secs(6) {
            return Err(anyhow!("client stopped responding"));
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let (encode_ms, fps, kbps) = video.stats();
            {
                let mut st = shared.status.lock().unwrap();
                st.encode_ms = encode_ms;
                st.fps = fps;
                st.kbps = kbps;
            }
            writer.send(&ControlMsg::HostStats { encode_ms, fps, bitrate_kbps: kbps })?;
        }
        match reader.read() {
            Ok(Some(msg)) => {
                last_rx = Instant::now();
                match msg {
                    ControlMsg::Ping { t_us } => writer.send(&ControlMsg::Pong { t_us })?,
                    ControlMsg::RequestKeyframe => {
                        // Rate-limit: a burst of loss reports should cost one keyframe.
                        if last_keyframe.elapsed() > Duration::from_millis(200) {
                            last_keyframe = Instant::now();
                            log::debug!("Keyframe requested by client");
                            video.force_keyframe();
                        }
                    }
                    ControlMsg::ClientStats { decode_ms, loss_pct, .. } => {
                        let mut st = shared.status.lock().unwrap();
                        st.client_decode_ms = decode_ms;
                        st.client_loss_pct = loss_pct;
                    }
                    ControlMsg::Bye { reason } => {
                        log::info!("Client disconnected: {reason}");
                        return Ok(());
                    }
                    other => log::debug!("Ignoring {other:?}"),
                }
            }
            Ok(None) => return Ok(()), // connection closed
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }
}
