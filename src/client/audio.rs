//! Client audio: UDP -> RTP -> Opus decode -> low-latency audio output.

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::media::{self, BusWatch, try_set};
use crate::protocol::AUDIO_PAYLOAD_TYPE;

pub struct AudioReceiver {
    pipeline: gst::Pipeline,
    _bus: BusWatch,
}

fn sink_element() -> &'static str {
    let candidates: &[&str] = if cfg!(windows) {
        &["wasapi2sink", "wasapisink", "directsoundsink", "autoaudiosink"]
    } else {
        &["pulsesink", "pipewiresink", "alsasink", "autoaudiosink"]
    };
    candidates.iter().copied().find(|n| media::has_element(n)).unwrap_or("autoaudiosink")
}

impl AudioReceiver {
    pub fn start(port: u16, buffer_ms: u32, on_error: impl Fn(String) + Send + 'static) -> Result<AudioReceiver> {
        let sink = sink_element();
        let desc = format!(
            "udpsrc name=asrc port={port} caps=\"application/x-rtp,media=audio,clock-rate=48000,encoding-name=OPUS,payload={pt}\" \
             ! rtpjitterbuffer latency={jb} drop-on-latency=true \
             ! rtpopusdepay ! opusdec name=adec ! audioconvert ! audioresample ! {sink} name=asink",
            pt = AUDIO_PAYLOAD_TYPE,
            jb = buffer_ms.clamp(5, 500),
        );
        log::info!("Audio receive pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("building audio receive pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        if let Some(dec) = pipeline.by_name("adec") {
            try_set(&dec, "plc", "true"); // conceal lost packets instead of clicking
        }
        if let Some(s) = pipeline.by_name("asink") {
            try_set(&s, "low-latency", "true");
            try_set(&s, "buffer-time", "40000");
            try_set(&s, "latency-time", "10000");
        }
        let bus = BusWatch::spawn("audio-rx", &pipeline, on_error);
        pipeline.set_state(gst::State::Playing).context("starting audio receiver")?;
        Ok(AudioReceiver { pipeline, _bus: bus })
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        media::shutdown_pipeline(&self.pipeline);
    }
}
