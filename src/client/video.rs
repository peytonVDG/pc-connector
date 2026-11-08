//! Client video: UDP -> RTP depay -> hardware decode -> latest-frame slot for the renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::media::{self, BusWatch, try_set};
use crate::protocol::{Codec, VIDEO_PAYLOAD_TYPE};

/// A decoded frame waiting to be drawn. Only the newest one is kept: if the display
/// is slower than the stream, older frames are dropped rather than queued.
pub struct DecodedFrame {
    pub sample: gst::Sample,
    pub decoded_at: Instant,
}

#[derive(Clone, Default)]
pub struct FrameSlot(Arc<Mutex<Option<DecodedFrame>>>);

impl FrameSlot {
    pub fn put(&self, f: DecodedFrame) {
        *self.0.lock().unwrap() = Some(f);
    }
    pub fn take(&self) -> Option<DecodedFrame> {
        self.0.lock().unwrap().take()
    }
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct DecStats {
    pending: HashMap<u64, Instant>,
    decode_ms: f32,
    frames: u32,
    window_start: Option<Instant>,
    fps: f32,
}

pub struct VideoReceiver {
    pipeline: gst::Pipeline,
    jitterbuffer: gst::Element,
    stats: Arc<Mutex<DecStats>>,
    keyframe_wanted: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    pub decoder_name: String,
    _bus: BusWatch,
}

impl VideoReceiver {
    pub fn start(
        port: u16,
        codec: Codec,
        decoder_override: &str,
        slot: FrameSlot,
        waker: Waker,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Result<VideoReceiver> {
        let decoder = media::pick_decoder(codec, decoder_override)
            .ok_or_else(|| anyhow!("no {} decoder available", codec.label()))?;
        let (depay, parse) = match codec {
            Codec::H264 => ("rtph264depay", "h264parse"),
            Codec::H265 => ("rtph265depay", "h265parse"),
        };
        let desc = format!(
            "udpsrc name=vsrc port={port} caps=\"application/x-rtp,media=video,clock-rate=90000,encoding-name={enc},payload={pt}\" \
             ! rtpjitterbuffer name=jb latency=10 drop-on-latency=true do-lost=true \
             ! {depay} name=depay ! {parse} ! {decoder} name=vdec ! videoconvert \
             ! video/x-raw,format=(string){{NV12,I420}} ! appsink name=vsink sync=false max-buffers=1 drop=true",
            enc = codec.rtp_encoding_name(),
            pt = VIDEO_PAYLOAD_TYPE,
        );
        log::info!("Video receive pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("building video receive pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;

        let src = pipeline.by_name("vsrc").unwrap();
        // Big receive buffer: keyframes arrive as a burst of hundreds of packets.
        try_set(&src, "buffer-size", "8388608");
        let jitterbuffer = pipeline.by_name("jb").unwrap();
        let depay_el = pipeline.by_name("depay").unwrap();
        try_set(&depay_el, "request-keyframe", "true");
        try_set(&depay_el, "wait-for-keyframe", "true");
        let dec = pipeline.by_name("vdec").unwrap();
        // Software decoders: slice threads only (frame threads add a frame of latency each).
        try_set(&dec, "thread-type", "slice");
        try_set(&dec, "automatic-request-sync-points", "true");

        let keyframe_wanted = Arc::new(AtomicBool::new(false));
        // Any element downstream of the jitter buffer that notices corruption asks for a
        // keyframe with an upstream event; forward that request to the host.
        if let Some(pad) = jitterbuffer.static_pad("src") {
            let kw = keyframe_wanted.clone();
            pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                if let Some(ev) = info.event()
                    && gstreamer_video::ForceKeyUnitEvent::is(ev)
                {
                    kw.store(true, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        }
        let bytes = Arc::new(AtomicU64::new(0));
        if let Some(pad) = src.static_pad("src") {
            let b = bytes.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buf) = info.buffer() {
                    b.fetch_add(buf.size() as u64, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        }

        let stats: Arc<Mutex<DecStats>> = Default::default();
        if let Some(pad) = dec.static_pad("sink") {
            let s = stats.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                    let mut st = s.lock().unwrap();
                    if st.pending.len() > 64 {
                        st.pending.clear();
                    }
                    st.pending.insert(pts.nseconds(), Instant::now());
                }
                gst::PadProbeReturn::Ok
            });
        }

        let appsink = pipeline
            .by_name("vsink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("vsink is not an appsink"))?;
        let s = stats.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let now = Instant::now();
                    {
                        let mut st = s.lock().unwrap();
                        if let Some(pts) = sample.buffer().and_then(|b| b.pts())
                            && let Some(t0) = st.pending.remove(&pts.nseconds())
                        {
                            let ms = now.duration_since(t0).as_secs_f32() * 1000.0;
                            st.decode_ms = if st.decode_ms == 0.0 { ms } else { st.decode_ms * 0.9 + ms * 0.1 };
                        }
                        st.frames += 1;
                        let start = *st.window_start.get_or_insert(now);
                        let el = now.duration_since(start).as_secs_f32();
                        if el >= 1.0 {
                            st.fps = st.frames as f32 / el;
                            st.frames = 0;
                            st.window_start = Some(now);
                        }
                    }
                    slot.put(DecodedFrame { sample, decoded_at: now });
                    waker();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let bus = BusWatch::spawn("video-rx", &pipeline, on_error);
        pipeline.set_state(gst::State::Playing).context("starting video receiver")?;
        Ok(VideoReceiver {
            pipeline,
            jitterbuffer,
            stats,
            keyframe_wanted,
            bytes,
            decoder_name: decoder.to_string(),
            _bus: bus,
        })
    }

    /// Returns true (once) if a keyframe should be requested from the host.
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_wanted.swap(false, Ordering::Relaxed)
    }

    /// (lost packets, pushed packets) so far, from the jitter buffer.
    pub fn packet_counts(&self) -> (u64, u64) {
        let s = self.jitterbuffer.property::<gst::Structure>("stats");
        let lost = s.get::<u64>("num-lost").unwrap_or(0);
        let pushed = s.get::<u64>("num-pushed").unwrap_or(0);
        (lost, pushed)
    }

    pub fn received_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// (decode ms, decoded fps)
    pub fn stats(&self) -> (f32, f32) {
        let s = self.stats.lock().unwrap();
        (s.decode_ms, s.fps)
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        media::shutdown_pipeline(&self.pipeline);
    }
}
