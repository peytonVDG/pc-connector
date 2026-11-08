//! Host audio: capture what the PC is playing -> Opus -> RTP -> UDP.

use std::net::IpAddr;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::media::{self, BusWatch, try_set};
use crate::protocol::AUDIO_PAYLOAD_TYPE;

pub struct AudioPipeline {
    pipeline: gst::Pipeline,
    _bus: BusWatch,
}

fn source_desc() -> Result<String> {
    if cfg!(windows) {
        if media::has_element("wasapi2src") {
            return Ok("wasapi2src name=asrc loopback=true".into());
        }
        if media::has_element("wasapisrc") {
            return Ok("wasapisrc name=asrc loopback=true".into());
        }
        return Err(anyhow!("no WASAPI source in the GStreamer runtime"));
    }
    if media::has_element("pulsesrc") {
        // Works with PulseAudio and with PipeWire's pulse server (Ubuntu's default).
        return Ok("pulsesrc name=asrc device=@DEFAULT_MONITOR@".into());
    }
    if media::has_element("pipewiresrc") {
        return Ok("pipewiresrc name=asrc stream-properties=\"props,stream.capture.sink=true\"".into());
    }
    Err(anyhow!("no audio capture element (install gstreamer1.0-pulseaudio)"))
}

impl AudioPipeline {
    pub fn start(
        client_ip: IpAddr,
        client_port: u16,
        bitrate_kbps: u32,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Result<AudioPipeline> {
        let desc = format!(
            "{src} ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
             ! opusenc name=aenc ! rtpopuspay pt={pt} ! udpsink name=asink host={client_ip} port={client_port} sync=false async=false",
            src = source_desc()?,
            pt = AUDIO_PAYLOAD_TYPE,
        );
        log::info!("Audio pipeline: {desc}");
        let pipeline = gst::parse::launch(&desc)
            .context("building audio pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        if let Some(src) = pipeline.by_name("asrc") {
            // Small capture buffers: 5 ms periods, 20 ms total.
            try_set(&src, "buffer-time", "20000");
            try_set(&src, "latency-time", "5000");
            try_set(&src, "low-latency", "true");
        }
        if let Some(enc) = pipeline.by_name("aenc") {
            try_set(&enc, "bitrate", &(bitrate_kbps * 1000).to_string());
            try_set(&enc, "audio-type", "restricted-lowdelay");
            try_set(&enc, "frame-size", "10");
            try_set(&enc, "bitrate-type", "cbr");
        }
        if let Some(sink) = pipeline.by_name("asink") {
            try_set(&sink, "qos-dscp", "46"); // EF -> Wi-Fi voice access category
        }
        let bus = BusWatch::spawn("audio", &pipeline, on_error);
        pipeline.set_state(gst::State::Playing).context("starting audio pipeline")?;
        Ok(AudioPipeline { pipeline, _bus: bus })
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        media::shutdown_pipeline(&self.pipeline);
    }
}

/// While alive, the host's audio goes to a silent virtual sink (and is streamed from its
/// monitor), so the room downstairs stays quiet. Linux (PulseAudio/PipeWire) only.
pub struct HostAudioMute {
    #[allow(dead_code)]
    module: String,
    #[allow(dead_code)]
    previous_default: String,
}

impl HostAudioMute {
    #[cfg(target_os = "linux")]
    pub fn engage() -> Option<HostAudioMute> {
        use std::process::Command;
        let out = |args: &[&str]| -> Option<String> {
            let o = Command::new("pactl").args(args).output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let previous_default = out(&["get-default-sink"])?;
        if previous_default == "pcconnector_sink" {
            return None;
        }
        let module = out(&[
            "load-module",
            "module-null-sink",
            "sink_name=pcconnector_sink",
            "sink_properties=device.description=PC-Connector-Stream",
        ])?;
        out(&["set-default-sink", "pcconnector_sink"])?;
        log::info!("Host audio muted (playing into virtual sink while streaming)");
        Some(HostAudioMute { module, previous_default })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn engage() -> Option<HostAudioMute> {
        log::warn!("Muting host audio is only supported on Linux hosts");
        None
    }
}

impl Drop for HostAudioMute {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use std::process::Command;
            let _ = Command::new("pactl").args(["set-default-sink", &self.previous_default]).status();
            let _ = Command::new("pactl").args(["unload-module", &self.module]).status();
            log::info!("Host audio restored");
        }
    }
}
