// Release builds on Windows are GUI apps (no console window); CLI commands re-attach
// to the parent console so their output is still visible.
#![cfg_attr(all(windows, not(debug_assertions), not(test)), windows_subsystem = "windows")]

mod client;
mod config;
mod discovery;
mod host;
mod keymap;
mod media;
mod net;
mod protocol;
mod ui;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pc-connector",
    version,
    about = "Low-latency game streaming between your own PCs. Run with no arguments for the app."
)]
struct Cli {
    /// More detailed logs (or set RUST_LOG=debug).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the app and start sharing this PC right away.
    Host {
        /// Run without a window (e.g. over SSH or from autostart). Pairing PINs are printed here.
        #[arg(long)]
        headless: bool,
    },
    /// Connect to a host and start streaming immediately.
    Connect {
        /// Host address, e.g. 192.168.1.50 or my-pc.local
        address: String,
        /// Pairing PIN shown on the host (first connection only).
        #[arg(long)]
        pin: Option<String>,
    },
    /// Show which capture methods, encoders and decoders work on this machine.
    Probe,
    /// Connect without a window and print live stream statistics (tests network + decoder).
    Bench {
        address: String,
        #[arg(long)]
        pin: Option<String>,
        /// How long to run.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
}

/// Logs go to stderr and to a file in the config directory (useful on Windows, where
/// the GUI build has no console).
struct Tee(Option<std::fs::File>);

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        if let Some(f) = &mut self.0 {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = &mut self.0 {
            let _ = f.flush();
        }
        Ok(())
    }
}

fn init_logging(verbose: bool) {
    let dir = config::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(dir.join("pc-connector.log")).ok();
    let default =
        if verbose { "debug,wgpu_core=warn,wgpu_hal=warn,naga=warn" } else { "info,wgpu_core=warn,wgpu_hal=warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .target(env_logger::Target::Pipe(Box::new(Tee(file))))
        .init();
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain Win32 call; failure (no parent console) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    #[cfg(windows)]
    if std::env::args().len() > 1 {
        attach_console();
    }
    let cli = Cli::parse();
    init_logging(cli.verbose);
    log::info!("PC Connector {} starting", env!("CARGO_PKG_VERSION"));
    if let Err(e) = media::init() {
        fail(&format!("{e:#}\n\nInstall GStreamer (see README) and try again."));
    }
    let cfg = Arc::new(Mutex::new(config::Config::load()));

    let result = match cli.command {
        None => ui::run(cfg, None, false),
        Some(Command::Host { headless: false }) => ui::run(cfg, None, true),
        Some(Command::Host { headless: true }) => run_headless_host(cfg),
        Some(Command::Connect { address, pin }) => ui::run(cfg, Some((address, pin)), false),
        Some(Command::Probe) => {
            probe();
            Ok(())
        }
        Some(Command::Bench { address, pin, seconds }) => bench(cfg, address, pin, seconds),
    };
    if let Err(e) = result {
        fail(&format!("{e:#}"));
    }
}

fn fail(msg: &str) -> ! {
    log::error!("{msg}");
    eprintln!("Error: {msg}");
    #[cfg(windows)]
    {
        // GUI build: make sure the user sees why nothing happened.
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
        let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = "PC Connector".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: valid null-terminated UTF-16 strings.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONERROR);
        }
    }
    std::process::exit(1);
}

fn run_headless_host(cfg: Arc<Mutex<config::Config>>) -> anyhow::Result<()> {
    let server = host::HostServer::start(cfg.clone())?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, std::sync::atomic::Ordering::SeqCst));
    }
    let name = cfg.lock().unwrap().host.name.clone();
    println!("PC Connector is hosting '{name}'. Press Ctrl+C to stop.");
    let mut last = String::new();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let st = server.status();
        let mut line =
            format!("Addresses: {}", if st.addresses.is_empty() { "?".into() } else { st.addresses.join(", ") });
        if let Some(pin) = &st.pin {
            line += &format!(" | PAIRING PIN for '{}': {pin}", st.pin_for.clone().unwrap_or_default());
        }
        if let (Some(c), Some(s)) = (&st.client, &st.stream) {
            line += &format!(" | Streaming to {c}: {}x{}@{} {}", s.width, s.height, s.fps, s.encoder);
        } else {
            line += " | waiting for a device";
        }
        for w in &st.warnings {
            line += &format!("\n  warning: {w}");
        }
        if line != last {
            println!("{line}");
            last = line;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("Stopping...");
    server.stop();
    Ok(())
}

fn bench(cfg: Arc<Mutex<config::Config>>, address: String, pin: Option<String>, seconds: u64) -> anyhow::Result<()> {
    let req = {
        let c = cfg.lock().unwrap();
        client::ConnectRequest {
            address,
            pin,
            client_id: c.client_id.clone(),
            client_name: format!("{} (bench)", c.client_name),
            settings: config::ClientSettings { wifi_low_latency: false, ..c.client.clone() },
            display_size: (1920, 1080),
        }
    };
    let session = match client::connect(req, Arc::new(|| {}))? {
        client::ConnectOutcome::Connected(s) => s,
        client::ConnectOutcome::PairingRequired { message } => {
            anyhow::bail!("{message}\nRun again with --pin <PIN>.")
        }
        client::ConnectOutcome::Rejected { reason } => anyhow::bail!("Rejected: {reason}"),
    };
    println!("Connected to '{}': {:?}", session.host_name, session.stream);
    let start = std::time::Instant::now();
    let mut frames = 0u32;
    let mut first_frame = None;
    let mut last_print = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        if let Some(reason) = session.ended() {
            anyhow::bail!("session ended: {reason}");
        }
        if let Some(f) = session.frames.take() {
            frames += 1;
            if first_frame.is_none() {
                let info = gstreamer_video::VideoInfo::from_caps(f.sample.caps().unwrap())?;
                println!(
                    "First frame after {} ms: {}x{} {:?}",
                    start.elapsed().as_millis(),
                    info.width(),
                    info.height(),
                    info.format()
                );
                first_frame = Some(std::time::Instant::now());
            }
        }
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = std::time::Instant::now();
            let s = session.stats();
            println!(
                "host {:5.1} fps encode {:4.1} ms {:6.1} Mbps | decoded {:5.1} fps decode {:4.1} ms ({}) | {:6.1} Mbps loss {:4.2}% rtt {:4.1} ms",
                s.host_fps,
                s.host_encode_ms,
                s.host_kbps / 1000.0,
                s.decode_fps,
                s.decode_ms,
                s.decoder,
                s.received_kbps / 1000.0,
                s.loss_pct,
                s.rtt_ms
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("Received {frames} frames in {seconds} s");
    if frames == 0 {
        anyhow::bail!("no video frames were received (check firewall for UDP ports 48802-48803)");
    }
    Ok(())
}

fn probe() {
    println!("PC Connector {} on {}", env!("CARGO_PKG_VERSION"), net::os_name());
    println!("GStreamer {}", gstreamer::version_string());
    println!("\nAs a host (sharing this PC):");
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if cfg!(target_os = "linux") {
        println!("  desktop session: {}", if session.is_empty() { "unknown" } else { &session });
        println!("  X11 capture (ximagesrc): {}", media::has_element("ximagesrc"));
        println!("  Wayland capture (pipewiresrc): {}", media::has_element("pipewiresrc"));
        let uinput = std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok();
        println!("  input injection (/dev/uinput writable): {uinput}");
    } else if cfg!(windows) {
        println!("  capture (d3d11screencapturesrc): {}", media::has_element("d3d11screencapturesrc"));
    }
    print!("  testing encoders... ");
    let _ = std::io::stdout().flush();
    let encoders = host::video::working_encoders();
    println!();
    if encoders.is_empty() {
        println!("  NO working encoder");
    }
    for e in encoders {
        println!("  encoder: {:<16} {:<14} {:?}", e.element, e.codec.label(), e.kind);
    }
    println!("\nAs a client (playing on this device):");
    for codec in [protocol::Codec::H264, protocol::Codec::H265] {
        match media::pick_decoder(codec, "") {
            Some(d) => println!(
                "  {:<14} decoder {d}{}",
                codec.label(),
                if media::is_hardware_decoder(d) { " (hardware)" } else { " (software)" }
            ),
            None => println!("  {:<14} no decoder", codec.label()),
        }
    }
    println!("\nLocal addresses: {:?}", net::local_ipv4_addrs());
    println!("Config file: {}", config::config_path().display());
}
