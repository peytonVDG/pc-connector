//! Host video: capture -> (GPU) convert/scale -> hardware encode -> RTP -> UDP.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::config::{CaptureMethod, EncoderPreference, HostSettings};
use crate::media::{self, BusWatch, try_set};
use crate::protocol::{Codec, VIDEO_PAYLOAD_TYPE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncKind {
    Nvidia,
    Va,
    VaapiLegacy,
    Qsv,
    Amf,
    MediaFoundation,
    Software,
}

impl EncKind {
    fn matches(self, pref: EncoderPreference) -> bool {
        match pref {
            EncoderPreference::Auto => true,
            EncoderPreference::Nvidia => self == EncKind::Nvidia,
            EncoderPreference::Vaapi => matches!(self, EncKind::Va | EncKind::VaapiLegacy),
            EncoderPreference::IntelQsv => self == EncKind::Qsv,
            EncoderPreference::Amd => self == EncKind::Amf || self == EncKind::Va,
            EncoderPreference::MediaFoundation => self == EncKind::MediaFoundation,
            EncoderPreference::Software => self == EncKind::Software,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderInfo {
    pub element: &'static str,
    pub codec: Codec,
    pub kind: EncKind,
}

fn candidates() -> Vec<EncoderInfo> {
    use Codec::*;
    use EncKind::*;
    let list: &[(&'static str, Codec, EncKind)] = if cfg!(windows) {
        &[
            ("nvd3d11h265enc", H265, Nvidia),
            ("nvd3d11h264enc", H264, Nvidia),
            ("qsvh265enc", H265, Qsv),
            ("qsvh264enc", H264, Qsv),
            ("amfh265enc", H265, Amf),
            ("amfh264enc", H264, Amf),
            ("mfh265enc", H265, MediaFoundation),
            ("mfh264enc", H264, MediaFoundation),
            ("x264enc", H264, Software),
            ("x265enc", H265, Software),
        ]
    } else {
        &[
            ("nvh265enc", H265, Nvidia),
            ("nvh264enc", H264, Nvidia),
            ("vah265enc", H265, Va),
            ("vah264enc", H264, Va),
            ("vah265lpenc", H265, Va),
            ("vah264lpenc", H264, Va),
            ("qsvh265enc", H265, Qsv),
            ("qsvh264enc", H264, Qsv),
            ("vaapih265enc", H265, VaapiLegacy),
            ("vaapih264enc", H264, VaapiLegacy),
            ("x264enc", H264, Software),
            ("x265enc", H265, Software),
            ("openh264enc", H264, Software),
        ]
    };
    list.iter().map(|&(element, codec, kind)| EncoderInfo { element, codec, kind }).collect()
}

/// Encoders that exist *and* successfully encode a few test frames. Probed once (slow).
pub fn working_encoders() -> &'static [EncoderInfo] {
    static CACHE: OnceLock<Vec<EncoderInfo>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut ok = Vec::new();
        for enc in candidates() {
            if !media::has_element(enc.element) {
                continue;
            }
            let desc = format!(
                "videotestsrc num-buffers=5 ! video/x-raw,format=BGRx,width=640,height=360,framerate=30/1 {upload} ! {convert} ! {element} ! fakesink",
                upload = if cfg!(windows) { "! d3d11upload" } else { "" }, // real capture is GPU memory on Windows
                convert = convert_chain(enc.kind, 640, 360),
                element = enc.element
            );
            let started = Instant::now();
            match media::run_test_pipeline(&desc, Duration::from_secs(8)) {
                Ok(()) => {
                    log::info!("Encoder {} works ({} ms to test)", enc.element, started.elapsed().as_millis());
                    ok.push(enc);
                }
                Err(e) => log::info!("Encoder {} unusable: {e}", enc.element),
            }
        }
        if ok.is_empty() {
            log::error!("No working video encoder found. Install GStreamer plugins (see README).");
        }
        ok
    })
}

/// Pick the encoder for a session.
pub fn choose_encoder(
    pref: EncoderPreference,
    codec_pref: crate::protocol::CodecPreference,
    client_decoders: &[Codec],
    client_hw_decoders: &[Codec],
) -> Result<EncoderInfo> {
    use crate::protocol::CodecPreference as CP;
    let encoders: Vec<EncoderInfo> = working_encoders()
        .iter()
        .copied()
        .filter(|e| e.kind.matches(pref))
        .filter(|e| client_decoders.contains(&e.codec))
        .collect();
    let encoders = if encoders.is_empty() {
        // Preference could not be satisfied; fall back to anything that works.
        working_encoders().iter().copied().filter(|e| client_decoders.contains(&e.codec)).collect()
    } else {
        encoders
    };
    let wanted: Option<Codec> = match codec_pref {
        CP::H264 => Some(Codec::H264),
        CP::H265 => Some(Codec::H265),
        CP::Auto => None,
    };
    let hw_first = |e: &&EncoderInfo| e.kind != EncKind::Software;
    let pick = if let Some(c) = wanted {
        encoders.iter().filter(|e| e.codec == c).find(hw_first).or_else(|| encoders.iter().find(|e| e.codec == c))
    } else {
        // Auto: HEVC gives better quality per bit, but only if both ends do it in hardware.
        encoders
            .iter()
            .filter(hw_first)
            .find(|e| e.codec == Codec::H265 && client_hw_decoders.contains(&Codec::H265))
            .or_else(|| encoders.iter().filter(hw_first).find(|e| e.codec == Codec::H264))
            .or_else(|| encoders.iter().find(|e| e.codec == Codec::H264))
            .or_else(|| encoders.first())
    };
    pick.copied().ok_or_else(|| anyhow!("no encoder on this PC produces a codec the client can decode"))
}

/// Convert + scale into what the encoder wants, on the GPU where possible.
fn convert_chain(kind: EncKind, w: u32, h: u32) -> String {
    if cfg!(windows) {
        return match kind {
            EncKind::Software => format!(
                "d3d11download ! videoconvert n-threads=4 ! videoscale ! video/x-raw,format=I420,width={w},height={h}"
            ),
            _ => format!("d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12,width={w},height={h}"),
        };
    }
    match kind {
        EncKind::Nvidia if media::has_element("cudaconvertscale") && media::has_element("cudaupload") => {
            format!("cudaupload ! cudaconvertscale ! video/x-raw(memory:CUDAMemory),format=NV12,width={w},height={h}")
        }
        EncKind::Va | EncKind::Qsv if media::has_element("vapostproc") => {
            format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width={w},height={h}")
        }
        EncKind::VaapiLegacy if media::has_element("vaapipostproc") => {
            format!("vaapipostproc ! video/x-raw(memory:VASurface),format=NV12,width={w},height={h}")
        }
        EncKind::Software => {
            format!("videoconvert n-threads=4 ! videoscale n-threads=4 ! video/x-raw,format=I420,width={w},height={h}")
        }
        _ => {
            format!("videoconvert n-threads=4 ! videoscale n-threads=4 ! video/x-raw,format=NV12,width={w},height={h}")
        }
    }
}

/// Low-latency settings per encoder family. Unknown properties are skipped, so this
/// works across GStreamer versions.
fn configure_encoder(enc: &gst::Element, info: &EncoderInfo, bitrate_kbps: u32, fps: u32, gop: u32) {
    let b = bitrate_kbps.to_string();
    let g = gop.to_string();
    // Allow roughly two frames worth of data per frame burst: keeps frame sizes (and so
    // network bursts and latency) bounded while leaving room for detail.
    let vbv_kbits = (bitrate_kbps / fps.max(1) * 2).max(100).to_string();
    match info.kind {
        EncKind::Nvidia => {
            let _ = try_set(enc, "preset", "p4") || try_set(enc, "preset", "low-latency-hq");
            try_set(enc, "tune", "ultra-low-latency");
            let _ = try_set(enc, "rc-mode", "cbr") || try_set(enc, "rc-mode", "cbr-ld-hq");
            try_set(enc, "bitrate", &b);
            try_set(enc, "max-bitrate", &b);
            try_set(enc, "vbv-buffer-size", &vbv_kbits);
            try_set(enc, "gop-size", &g);
            try_set(enc, "bframes", "0");
            try_set(enc, "b-frames", "0");
            try_set(enc, "rc-lookahead", "0");
            try_set(enc, "zerolatency", "true");
            try_set(enc, "zero-reorder-delay", "true");
            try_set(enc, "aud", "false");
            try_set(enc, "repeat-sequence-header", "true");
        }
        EncKind::Va => {
            try_set(enc, "rate-control", "cbr");
            try_set(enc, "bitrate", &b);
            try_set(enc, "cpb-size", &vbv_kbits);
            try_set(enc, "key-int-max", &g);
            try_set(enc, "b-frames", "0");
            try_set(enc, "ref-frames", "1");
            try_set(enc, "target-usage", "4");
        }
        EncKind::VaapiLegacy => {
            try_set(enc, "rate-control", "cbr");
            try_set(enc, "bitrate", &b);
            try_set(enc, "keyframe-period", &g);
            try_set(enc, "max-bframes", "0");
            try_set(enc, "refs", "1");
            try_set(enc, "quality-level", "4");
            try_set(enc, "cpb-length", &(2000 / fps.max(1)).max(1).to_string());
        }
        EncKind::Qsv => {
            try_set(enc, "rate-control", "cbr");
            try_set(enc, "bitrate", &b);
            try_set(enc, "gop-size", &g);
            try_set(enc, "b-frames", "0");
            try_set(enc, "ref-frames", "1");
            try_set(enc, "low-latency", "true");
            try_set(enc, "target-usage", "4");
        }
        EncKind::Amf => {
            try_set(enc, "usage", "ultra-low-latency");
            try_set(enc, "rate-control", "cbr");
            try_set(enc, "preset", "balanced");
            try_set(enc, "bitrate", &b);
            try_set(enc, "max-bitrate", &b);
            try_set(enc, "gop-size", &g);
            try_set(enc, "b-frames", "0");
        }
        EncKind::MediaFoundation => {
            try_set(enc, "low-latency", "true");
            try_set(enc, "rc-mode", "cbr");
            try_set(enc, "bitrate", &b);
            try_set(enc, "gop-size", &g);
            try_set(enc, "bframes", "0");
            try_set(enc, "quality-vs-speed", "50");
        }
        EncKind::Software => match info.element {
            "x264enc" => {
                try_set(enc, "tune", "zerolatency");
                try_set(enc, "speed-preset", "superfast");
                try_set(enc, "bitrate", &b);
                try_set(enc, "key-int-max", &g);
                try_set(enc, "vbv-buf-capacity", &(2000 / fps.max(1)).max(10).to_string());
                try_set(enc, "byte-stream", "true");
                try_set(enc, "threads", "0");
            }
            "x265enc" => {
                try_set(enc, "tune", "zerolatency");
                try_set(enc, "speed-preset", "ultrafast");
                try_set(enc, "bitrate", &b);
                try_set(enc, "key-int-max", &g);
                try_set(enc, "option-string", "bframes=0:repeat-headers=1");
            }
            "openh264enc" => {
                try_set(enc, "bitrate", &(bitrate_kbps as u64 * 1000).to_string());
                try_set(enc, "gop-size", &g);
                try_set(enc, "complexity", "low");
                try_set(enc, "rate-control", "bitrate");
            }
            _ => {}
        },
    }
}

/// Where frames come from.
pub enum Capture {
    X11 {
        display: String,
    },
    #[cfg(target_os = "linux")]
    Portal(super::portal::PortalSession),
    Windows {
        monitor: u32,
    },
}

impl Capture {
    pub fn label(&self) -> String {
        match self {
            Capture::X11 { display } => format!("X11 ({display})"),
            #[cfg(target_os = "linux")]
            Capture::Portal(_) => "PipeWire screen cast (Wayland)".into(),
            Capture::Windows { monitor } => format!("Desktop Duplication (monitor {monitor})"),
        }
    }

    /// Open the capture source configured in settings. May show the desktop's
    /// screen-share dialog on Wayland. Returns a new portal restore token, if any.
    pub fn open(settings: &HostSettings) -> Result<(Capture, Option<String>)> {
        let method = match settings.capture {
            CaptureMethod::Auto => {
                if cfg!(windows) {
                    CaptureMethod::Windows
                } else if std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
                    || std::env::var_os("WAYLAND_DISPLAY").is_some()
                {
                    CaptureMethod::Portal
                } else {
                    CaptureMethod::X11
                }
            }
            m => m,
        };
        match method {
            CaptureMethod::Windows => {
                if !media::has_element("d3d11screencapturesrc") {
                    return Err(anyhow!("d3d11screencapturesrc is missing from the GStreamer runtime"));
                }
                Ok((Capture::Windows { monitor: settings.monitor_index }, None))
            }
            CaptureMethod::X11 => {
                if !media::has_element("ximagesrc") {
                    return Err(anyhow!("ximagesrc missing: install gstreamer1.0-plugins-good / gstreamer1.0-x"));
                }
                let display = if settings.x11_display.is_empty() {
                    std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into())
                } else {
                    settings.x11_display.clone()
                };
                Ok((Capture::X11 { display }, None))
            }
            CaptureMethod::Portal => {
                #[cfg(target_os = "linux")]
                {
                    if !media::has_element("pipewiresrc") {
                        return Err(anyhow!("pipewiresrc missing: install gstreamer1.0-pipewire"));
                    }
                    let (session, token) = super::portal::PortalSession::open(
                        settings.portal_restore_token.as_deref(),
                        settings.show_cursor,
                    )?;
                    Ok((Capture::Portal(session), token))
                }
                #[cfg(not(target_os = "linux"))]
                Err(anyhow!("PipeWire portal capture is only available on Linux"))
            }
            CaptureMethod::Auto => unreachable!(),
        }
    }

    fn source_desc(&self, fps: u32, show_cursor: bool) -> String {
        match self {
            Capture::X11 { display } => format!(
                "ximagesrc name=src display-name={display} use-damage=false show-pointer={show_cursor} ! video/x-raw,framerate={fps}/1"
            ),
            #[cfg(target_os = "linux")]
            Capture::Portal(p) => {
                // The portal delivers frames only when the screen changes and advertises a
                // variable frame rate (0/1). Encoders size their rate control by frame rate,
                // so stamp the nominal rate onto the caps (buffers are untouched).
                let setter = if media::has_element("capssetter") {
                    format!(" ! capssetter caps=\"video/x-raw,framerate={fps}/1\"")
                } else {
                    String::new()
                };
                format!("pipewiresrc name=src fd={} path={} do-timestamp=true{setter}", p.fd(), p.node_id)
            }
            Capture::Windows { monitor } => format!(
                "d3d11screencapturesrc name=src monitor-index={monitor} show-cursor={show_cursor} ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
            ),
        }
    }

    /// Native size of the captured screen.
    pub fn native_size(&self) -> Option<(u32, u32)> {
        match self {
            #[cfg(target_os = "linux")]
            Capture::Portal(p) => p.size.filter(|(w, h)| *w > 0 && *h > 0),
            Capture::X11 { display } => {
                let el = gst::ElementFactory::make("ximagesrc").build().ok()?;
                try_set(&el, "display-name", display);
                query_src_size(&el)
            }
            Capture::Windows { monitor } => {
                let el = gst::ElementFactory::make("d3d11screencapturesrc").build().ok()?;
                try_set(&el, "monitor-index", &monitor.to_string());
                query_src_size(&el)
            }
        }
    }
}

fn query_src_size(el: &gst::Element) -> Option<(u32, u32)> {
    el.set_state(gst::State::Ready).ok()?;
    let caps = el.static_pad("src")?.query_caps(None);
    let _ = el.set_state(gst::State::Null);
    let s = caps.structure(0)?;
    let w = s.get::<i32>("width").ok()?;
    let h = s.get::<i32>("height").ok()?;
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// Fit the captured size into the requested box, keeping aspect ratio and never upscaling.
pub fn fit_size(native: (u32, u32), requested: (u32, u32)) -> (u32, u32) {
    let (cw, ch) = native;
    let (rw, rh) = requested;
    if rw == 0 || rh == 0 {
        return (cw & !1, ch & !1);
    }
    let scale = (rw as f64 / cw as f64).min(rh as f64 / ch as f64).min(1.0);
    let w = ((cw as f64 * scale).round() as u32).max(2) & !1;
    let h = ((ch as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

#[derive(Default)]
struct EncStats {
    /// Arrival times of frames inside the encoder. Encoders shift output timestamps
    /// (to keep DTS positive), so frames are matched in order instead (no B-frames).
    pending: VecDeque<Instant>,
    encode_ms: f32,
    frames: u32,
    bytes: u64,
    window_start: Option<Instant>,
    fps: f32,
    kbps: f32,
}

pub struct VideoParams<'a> {
    pub capture: &'a Capture,
    pub encoder: EncoderInfo,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_s: u32,
    pub show_cursor: bool,
    pub client_ip: IpAddr,
    pub client_port: u16,
}

pub struct VideoPipeline {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    stats: Arc<Mutex<EncStats>>,
    _bus: BusWatch,
}

impl VideoPipeline {
    pub fn start(p: &VideoParams, on_error: impl Fn(String) + Send + 'static) -> Result<VideoPipeline> {
        let pay = match p.encoder.codec {
            Codec::H264 => "rtph264pay",
            Codec::H265 => "rtph265pay",
        };
        let codec_caps = match p.encoder.codec {
            Codec::H264 => "video/x-h264,stream-format=byte-stream",
            Codec::H265 => "video/x-h265,stream-format=byte-stream",
        };
        // The leaky single-buffer queue drops frames if the encoder ever falls behind
        // instead of letting latency build up.
        let desc = format!(
            "{src} ! queue name=capq max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {convert} ! {enc} name=venc ! {codec_caps} ! {pay} name=vpay pt={pt} mtu=1400 config-interval=-1 \
             ! udpsink name=vsink host={host} port={port} sync=false async=false",
            src = p.capture.source_desc(p.fps, p.show_cursor),
            convert = convert_chain(p.encoder.kind, p.width, p.height),
            enc = p.encoder.element,
            pt = VIDEO_PAYLOAD_TYPE,
            host = p.client_ip,
            port = p.client_port,
        );
        log::info!("Video pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("building video pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;

        let encoder = pipeline.by_name("venc").unwrap();
        let gop = (p.fps * p.keyframe_interval_s.max(1)).max(30);
        configure_encoder(&encoder, &p.encoder, p.bitrate_kbps, p.fps, gop);
        if let Some(pay) = pipeline.by_name("vpay") {
            try_set(&pay, "aggregate-mode", "zero-latency");
        }
        if let Some(sink) = pipeline.by_name("vsink") {
            try_set(&sink, "qos-dscp", "34"); // AF41 -> Wi-Fi video access category
            try_set(&sink, "buffer-size", "4194304");
        }
        if let Some(src) = pipeline.by_name("src") {
            // pipewiresrc: resend the last frame if the screen is static so the client
            // never waits long for a picture (e.g. after a keyframe request).
            try_set(&src, "keepalive-time", "250");
        }

        let stats: Arc<Mutex<EncStats>> = Default::default();
        install_encoder_probes(&encoder, stats.clone());

        let bus = BusWatch::spawn("video", &pipeline, on_error);
        pipeline.set_state(gst::State::Playing).context("starting video pipeline")?;
        Ok(VideoPipeline { pipeline, encoder, stats, _bus: bus })
    }

    pub fn force_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        if let Some(pad) = self.encoder.static_pad("src") {
            pad.send_event(event);
        }
    }

    /// (encode ms, fps, kbps)
    pub fn stats(&self) -> (f32, f32, f32) {
        let s = self.stats.lock().unwrap();
        (s.encode_ms, s.fps, s.kbps)
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        media::shutdown_pipeline(&self.pipeline);
    }
}

fn install_encoder_probes(enc: &gst::Element, stats: Arc<Mutex<EncStats>>) {
    if let Some(sink) = enc.static_pad("sink") {
        let s = stats.clone();
        sink.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let mut st = s.lock().unwrap();
            if st.pending.len() > 16 {
                st.pending.clear(); // encoder dropped frames; resynchronise
            }
            st.pending.push_back(Instant::now());
            gst::PadProbeReturn::Ok
        });
    }
    if let Some(src) = enc.static_pad("src") {
        src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                let mut st = stats.lock().unwrap();
                if !buf.flags().contains(gst::BufferFlags::HEADER)
                    && let Some(t0) = st.pending.pop_front()
                {
                    let ms = t0.elapsed().as_secs_f32() * 1000.0;
                    st.encode_ms = if st.encode_ms == 0.0 { ms } else { st.encode_ms * 0.9 + ms * 0.1 };
                }
                st.frames += 1;
                st.bytes += buf.size() as u64;
                let now = Instant::now();
                let start = *st.window_start.get_or_insert(now);
                let el = now.duration_since(start).as_secs_f32();
                if el >= 1.0 {
                    st.fps = st.frames as f32 / el;
                    st.kbps = st.bytes as f32 * 8.0 / el / 1000.0;
                    st.frames = 0;
                    st.bytes = 0;
                    st.window_start = Some(now);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit() {
        assert_eq!(fit_size((3840, 2160), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_size((1920, 1080), (2880, 1920)), (1920, 1080)); // never upscale
        assert_eq!(fit_size((2560, 1440), (1920, 1200)), (1920, 1080)); // keep 16:9
        assert_eq!(fit_size((1920, 1080), (0, 0)), (1920, 1080));
        assert_eq!(fit_size((1366, 768), (1280, 720)), (1280, 720));
    }
}
