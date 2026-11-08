//! GStreamer helpers shared by host and client.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::protocol::Codec;

/// Initialise GStreamer. On Windows, a GStreamer runtime shipped next to the executable
/// (the release zip layout) is used automatically.
pub fn init() -> Result<()> {
    #[cfg(windows)]
    windows_bundle_env();
    gst::init().context("failed to initialise GStreamer (is the GStreamer runtime installed?)")?;
    Ok(())
}

#[cfg(windows)]
fn windows_bundle_env() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let bundle = dir.join("gstreamer");
    let plugins = bundle.join("lib").join("gstreamer-1.0");
    if !plugins.is_dir() {
        return;
    }
    // Plugins are loaded with an altered DLL search path, so make sure the bundled
    // dependency DLLs (next to the exe and in gstreamer\bin) can be found.
    let bin = bundle.join("bin");
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf(), bin.clone()];
    paths.extend(std::env::split_paths(&old_path));
    if let Ok(joined) = std::env::join_paths(paths) {
        // SAFETY: called at start-up before any other thread exists.
        unsafe { std::env::set_var("PATH", joined) };
    }
    let registry = crate::config::config_dir().join("gst-registry.bin");
    let scanner = bundle.join("libexec").join("gstreamer-1.0").join("gst-plugin-scanner.exe");
    // SAFETY: as above, single-threaded start-up.
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins);
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH", &plugins);
        std::env::set_var("GST_REGISTRY_1_0", &registry);
        if scanner.is_file() {
            std::env::set_var("GST_PLUGIN_SCANNER_1_0", &scanner);
        }
    }
}

pub fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Set a property from its string form if the element has it. Returns whether it was set.
pub fn try_set(el: &gst::Element, name: &str, value: &str) -> bool {
    let Some(pspec) = el.find_property(name) else {
        log::debug!("{}: no property '{name}'", el.name());
        return false;
    };
    if !pspec.flags().contains(gst::glib::ParamFlags::WRITABLE) {
        return false;
    }
    match <gst::glib::Value as gst::prelude::GstValueExt>::deserialize(value, pspec.value_type()) {
        Ok(v) => {
            el.set_property_from_value(name, &v);
            log::debug!("{}: {name}={value}", el.name());
            true
        }
        Err(_) => {
            log::debug!("{}: cannot set {name}={value}", el.name());
            false
        }
    }
}

pub fn is_hardware_decoder(name: &str) -> bool {
    !(name.starts_with("avdec_") || name.starts_with("openh264") || name.starts_with("libde265"))
}

/// Decoder elements to try, best first.
pub fn decoder_candidates(codec: Codec) -> Vec<&'static str> {
    match (codec, cfg!(windows)) {
        (Codec::H264, true) => vec!["d3d11h264dec", "d3d12h264dec", "nvh264dec", "qsvh264dec", "avdec_h264"],
        (Codec::H265, true) => vec!["d3d11h265dec", "d3d12h265dec", "nvh265dec", "qsvh265dec", "avdec_h265"],
        (Codec::H264, false) => vec!["nvh264dec", "vah264dec", "vaapih264dec", "avdec_h264", "openh264dec"],
        (Codec::H265, false) => vec!["nvh265dec", "vah265dec", "vaapih265dec", "avdec_h265"],
    }
}

pub fn pick_decoder(codec: Codec, override_name: &str) -> Option<&'static str> {
    if !override_name.is_empty() {
        if let Some(found) = decoder_candidates(codec).into_iter().find(|n| *n == override_name)
            && has_element(found)
        {
            return Some(found);
        }
        log::warn!("Decoder override '{override_name}' not usable for {codec:?}, choosing automatically");
    }
    decoder_candidates(codec).into_iter().find(|n| has_element(n))
}

/// Codecs this machine can decode, and the subset with hardware decoders.
pub fn probe_decoders() -> (Vec<Codec>, Vec<Codec>) {
    let mut all = Vec::new();
    let mut hw = Vec::new();
    for codec in [Codec::H264, Codec::H265] {
        if let Some(dec) = pick_decoder(codec, "") {
            all.push(codec);
            if is_hardware_decoder(dec) {
                hw.push(codec);
            }
        }
    }
    (all, hw)
}

/// Watches a pipeline bus on a background thread and reports fatal errors.
pub struct BusWatch {
    stop: Arc<AtomicBool>,
}

impl BusWatch {
    pub fn spawn(name: &str, pipeline: &gst::Pipeline, on_error: impl Fn(String) + Send + 'static) -> BusWatch {
        let stop = Arc::new(AtomicBool::new(false));
        let bus = pipeline.bus().expect("pipeline without bus");
        let s = stop.clone();
        let label = name.to_string();
        let _ = std::thread::Builder::new().name(format!("bus-{name}")).spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
                    continue;
                };
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        let text = format!(
                            "{} pipeline error from {}: {} ({})",
                            label,
                            err.src().map(|s| s.path_string().to_string()).unwrap_or_default(),
                            err.error(),
                            err.debug().map(|d| d.to_string()).unwrap_or_default()
                        );
                        log::error!("{text}");
                        on_error(text);
                        break;
                    }
                    MessageView::Warning(w) => {
                        log::warn!(
                            "{label} pipeline warning: {} ({})",
                            w.error(),
                            w.debug().map(|d| d.to_string()).unwrap_or_default()
                        );
                    }
                    MessageView::Eos(_) => {
                        log::info!("{label} pipeline reached end of stream");
                        on_error(format!("{label} stream ended"));
                        break;
                    }
                    _ => {}
                }
            }
        });
        BusWatch { stop }
    }
}

impl Drop for BusWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Stop a pipeline, waiting briefly for the state change.
pub fn shutdown_pipeline(p: &gst::Pipeline) {
    let _ = p.set_state(gst::State::Null);
    let _ = p.state(gst::ClockTime::from_seconds(2));
}

/// Run a short pipeline to completion; used to verify encoders actually work on this machine.
pub fn run_test_pipeline(desc: &str, timeout: Duration) -> Result<()> {
    let pipeline = gst::parse::launch(desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("not a pipeline"))?;
    let bus = pipeline.bus().unwrap();
    pipeline.set_state(gst::State::Playing)?;
    let result = match bus.timed_pop_filtered(
        gst::ClockTime::from_mseconds(timeout.as_millis() as u64),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(msg) => match msg.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(e) => Err(anyhow!("{}", e.error())),
            _ => Ok(()),
        },
        None => Err(anyhow!("timed out")),
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}
