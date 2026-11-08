//! egui screens: the launcher (play / host) and the in-stream overlay.

use std::time::Instant;

use egui::{Align, Color32, CornerRadius, Layout, RichText, Ui};

use crate::client::SessionStats;
use crate::config::{CaptureMethod, Config, EncoderPreference, ResolutionChoice};
use crate::discovery::{Browser, FoundHost};
use crate::host::HostStatus;
use crate::protocol::{CodecPreference, StreamConfig};

pub const ACCENT: Color32 = Color32::from_rgb(64, 156, 255);
const GOOD: Color32 = Color32::from_rgb(90, 200, 120);
const WARN: Color32 = Color32::from_rgb(240, 180, 60);
const BAD: Color32 = Color32::from_rgb(235, 90, 90);
const MUTED: Color32 = Color32::from_rgb(150, 156, 166);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Play,
    Host,
}

pub struct Connecting {
    pub label: String,
    pub started: Instant,
}

pub struct Pairing {
    pub address: String,
    pub label: String,
    pub message: String,
    pub pin: String,
}

pub struct Launcher {
    pub tab: Tab,
    pub browser: Option<Browser>,
    pub manual_addr: String,
    /// (message, is_error)
    pub notice: Option<(String, bool)>,
    pub connecting: Option<Connecting>,
    pub pairing: Option<Pairing>,
    pub adapter: String,
}

impl Launcher {
    pub fn new(cfg: &Config) -> Launcher {
        let tab = match cfg.last_role.as_str() {
            "host" => Tab::Host,
            "client" => Tab::Play,
            _ if cfg!(target_os = "linux") => Tab::Host,
            _ => Tab::Play,
        };
        Launcher {
            tab,
            browser: None,
            manual_addr: String::new(),
            notice: None,
            connecting: None,
            pairing: None,
            adapter: String::new(),
        }
    }
}

pub enum Action {
    Connect { address: String, label: String, pin: Option<String> },
    CancelConnect,
    StartHost,
    StopHost,
    KickClient,
    SaveConfig,
}

pub fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.visuals.selection.bg_fill = ACCENT;
        s.visuals.hyperlink_color = ACCENT;
        s.visuals.panel_fill = Color32::from_rgb(18, 20, 24);
        s.visuals.window_fill = Color32::from_rgb(26, 29, 34);
        for font in s.text_styles.values_mut() {
            font.size *= 1.12;
        }
    });
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(Color32::from_rgb(28, 31, 37))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(44, 48, 56)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn big_button(ui: &mut Ui, text: &str, primary: bool) -> egui::Response {
    let mut b = egui::Button::new(RichText::new(text).size(17.0).strong()).min_size(egui::vec2(150.0, 36.0));
    if primary {
        b = b.fill(ACCENT);
    }
    ui.add(b)
}

pub fn launcher(ui: &mut Ui, l: &mut Launcher, cfg: &mut Config, host: Option<&HostStatus>) -> Vec<Action> {
    let mut actions = Vec::new();
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let width = ui.available_width().min(760.0);
            let pad = ((ui.available_width() - width) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    header(ui);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let play = ui.add(egui::Button::selectable(
                            l.tab == Tab::Play,
                            RichText::new("🎮  Play on this device").size(16.0),
                        ));
                        let hostb = ui.add(egui::Button::selectable(
                            l.tab == Tab::Host,
                            RichText::new("🖥  Share this PC").size(16.0),
                        ));
                        if play.clicked() && l.tab != Tab::Play {
                            l.tab = Tab::Play;
                            cfg.last_role = "client".into();
                            actions.push(Action::SaveConfig);
                        }
                        if hostb.clicked() && l.tab != Tab::Host {
                            l.tab = Tab::Host;
                            cfg.last_role = "host".into();
                            actions.push(Action::SaveConfig);
                        }
                    });
                    ui.add_space(4.0);
                    if let Some((msg, err)) = &l.notice {
                        let color = if *err { BAD } else { GOOD };
                        let mut close = false;
                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(msg).color(color));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    close = ui.small_button("✕").clicked();
                                });
                            });
                        });
                        if close {
                            l.notice = None;
                        }
                    }
                    match l.tab {
                        Tab::Play => play_tab(ui, l, cfg, &mut actions),
                        Tab::Host => host_tab(ui, cfg, host, &mut actions),
                    }
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!(
                            "PC Connector {} · {} · settings: {}",
                            env!("CARGO_PKG_VERSION"),
                            l.adapter,
                            crate::config::config_path().display()
                        ))
                        .small()
                        .color(MUTED),
                    );
                });
            });
        });
    });

    if let Some(c) = &l.connecting {
        let mut cancel = false;
        modal(ui.ctx(), "connecting", |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(format!("Connecting to {}…", c.label)).size(18.0));
            });
            if c.started.elapsed().as_secs() >= 3 {
                ui.label(
                    RichText::new(
                        "Waiting for the host. If its screen shows a \"share your screen\" dialog, choose the screen and click Share.",
                    )
                    .color(MUTED),
                );
            }
            ui.add_space(6.0);
            cancel = ui.button("Cancel").clicked();
        });
        if cancel {
            actions.push(Action::CancelConnect);
        }
    } else if let Some(p) = &mut l.pairing {
        let mut submit = false;
        let mut cancel = false;
        modal(ui.ctx(), "pairing", |ui| {
            ui.label(RichText::new(format!("Pair with {}", p.label)).size(20.0).strong());
            ui.label(&p.message);
            ui.add_space(4.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut p.pin)
                    .hint_text("PIN")
                    .font(egui::FontId::monospace(28.0))
                    .desired_width(140.0)
                    .char_limit(4),
            );
            edit.request_focus();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.horizontal(|ui| {
                submit |= big_button(ui, "Pair", true).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if submit && p.pin.trim().len() == 4 {
            actions.push(Action::Connect {
                address: p.address.clone(),
                label: p.label.clone(),
                pin: Some(p.pin.trim().to_string()),
            });
            l.pairing = None;
        } else if cancel {
            l.pairing = None;
        }
    }
    actions
}

fn modal(ctx: &egui::Context, id: &str, add: impl FnOnce(&mut Ui)) {
    egui::Modal::new(egui::Id::unique(id)).show(ctx, |ui| {
        ui.set_max_width(460.0);
        add(ui);
    });
}

fn header(ui: &mut Ui) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("PC Connector").size(30.0).strong().color(Color32::WHITE));
    });
    ui.label(RichText::new("Stream games from your PC to this screen with the lowest possible latency.").color(MUTED));
}

fn play_tab(ui: &mut Ui, l: &mut Launcher, cfg: &mut Config, actions: &mut Vec<Action>) {
    if l.browser.is_none() {
        l.browser = Some(Browser::start());
    }
    let found: Vec<FoundHost> = l.browser.as_ref().map(|b| b.hosts()).unwrap_or_default();
    card(ui, |ui| {
        ui.label(RichText::new("PCs on your network").size(18.0).strong());
        if found.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(
                        "Looking for hosts… On the gaming PC, open PC Connector and choose \"Share this PC\".",
                    )
                    .color(MUTED),
                );
            });
        }
        for h in &found {
            ui.separator();
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&h.reply.name).size(17.0).strong());
                    let state = if h.reply.busy { "in use" } else { "ready" };
                    ui.label(RichText::new(format!("{} · {} · {}", h.addr.ip(), h.reply.os, state)).color(MUTED));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if big_button(ui, "Connect", true).clicked() {
                        actions.push(Action::Connect {
                            address: h.addr.to_string(),
                            label: h.reply.name.clone(),
                            pin: None,
                        });
                    }
                });
            });
        }
    });
    card(ui, |ui| {
        ui.label(RichText::new("Connect by address").size(16.0).strong());
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut l.manual_addr)
                    .hint_text("192.168.1.50 or my-pc.local")
                    .desired_width(300.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Connect").clicked() || enter) && !l.manual_addr.trim().is_empty() {
                actions.push(Action::Connect {
                    address: l.manual_addr.trim().to_string(),
                    label: l.manual_addr.trim().to_string(),
                    pin: None,
                });
            }
        });
        if !cfg.recent_hosts.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Recent:").color(MUTED));
                for h in cfg.recent_hosts.clone() {
                    if ui.link(format!("{} ({})", h.name, h.address)).clicked() {
                        actions.push(Action::Connect { address: h.address.clone(), label: h.name.clone(), pin: None });
                    }
                }
            });
        }
    });
    let mut changed = false;
    card(ui, |ui| {
        egui::CollapsingHeader::new(RichText::new("Stream settings").size(16.0).strong()).default_open(false).show(
            ui,
            |ui| {
                let s = &mut cfg.client;
                egui::Grid::new("client_settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Resolution");
                    egui::ComboBox::from_id_salt("res").selected_text(s.resolution.label()).show_ui(ui, |ui| {
                        for r in ResolutionChoice::ALL {
                            changed |= ui.selectable_value(&mut s.resolution, r, r.label()).changed();
                        }
                    });
                    ui.end_row();

                    ui.label("Frame rate");
                    egui::ComboBox::from_id_salt("fps").selected_text(format!("{} fps", s.fps)).show_ui(ui, |ui| {
                        for f in [30, 60, 90, 120, 144] {
                            changed |= ui.selectable_value(&mut s.fps, f, format!("{f} fps")).changed();
                        }
                    });
                    ui.end_row();

                    ui.label("Bitrate");
                    ui.horizontal(|ui| {
                        changed |= ui.add(egui::Slider::new(&mut s.bitrate_mbps, 0..=150).suffix(" Mbps")).changed();
                        if s.bitrate_mbps == 0 {
                            ui.label(RichText::new("auto").color(MUTED));
                        }
                    });
                    ui.end_row();

                    ui.label("Codec");
                    egui::ComboBox::from_id_salt("codec")
                        .selected_text(match s.codec {
                            CodecPreference::Auto => "Automatic",
                            CodecPreference::H264 => "H.264",
                            CodecPreference::H265 => "H.265 / HEVC",
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui.selectable_value(&mut s.codec, CodecPreference::Auto, "Automatic").changed();
                            changed |= ui.selectable_value(&mut s.codec, CodecPreference::H264, "H.264").changed();
                            changed |=
                                ui.selectable_value(&mut s.codec, CodecPreference::H265, "H.265 / HEVC").changed();
                        });
                    ui.end_row();

                    ui.label("Audio buffer");
                    changed |= ui.add(egui::Slider::new(&mut s.audio_buffer_ms, 10..=150).suffix(" ms")).changed();
                    ui.end_row();
                });
                ui.add_space(4.0);
                changed |= ui.checkbox(&mut s.fullscreen, "Fullscreen while streaming").changed();
                changed |= ui.checkbox(&mut s.vsync, "V-Sync (no tearing, slightly more latency)").changed();
                changed |= ui.checkbox(&mut s.audio, "Stream audio").changed();
                changed |= ui.checkbox(&mut s.show_stats, "Show performance overlay").changed();
                changed |= ui.checkbox(&mut s.relative_mouse, "Game mouse mode (locked, raw movement)").changed();
                if cfg!(windows) {
                    changed |= ui
                        .checkbox(&mut s.capture_system_keys, "Send Windows key, Alt+Tab, etc. to the host")
                        .changed();
                    changed |= ui
                        .checkbox(&mut s.wifi_low_latency, "Wi-Fi low-latency mode (pauses background scans)")
                        .changed();
                }
                ui.horizontal(|ui| {
                    ui.label("Decoder override");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut s.decoder_override)
                                .hint_text("automatic")
                                .desired_width(160.0),
                        )
                        .changed();
                });
            },
        );
    });
    if changed {
        actions.push(Action::SaveConfig);
    }
    card(ui, |ui| {
        ui.label(RichText::new("While streaming").size(16.0).strong());
        egui::Grid::new("shortcuts").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (k, v) in [
                ("Ctrl+Alt+Shift+Z", "Release / capture mouse and keyboard"),
                ("Ctrl+Alt+Shift+M", "Switch game / desktop mouse mode"),
                ("Ctrl+Alt+Shift+F", "Toggle fullscreen"),
                ("Ctrl+Alt+Shift+S", "Toggle performance overlay"),
                ("Ctrl+Alt+Shift+Q", "Disconnect"),
            ] {
                ui.label(RichText::new(k).monospace().color(ACCENT));
                ui.label(v);
                ui.end_row();
            }
        });
        ui.label(RichText::new("Game controllers connected to this device are forwarded automatically.").color(MUTED));
    });
}

fn host_tab(ui: &mut Ui, cfg: &mut Config, host: Option<&HostStatus>, actions: &mut Vec<Action>) {
    card(ui, |ui| match host {
        None => {
            ui.label(RichText::new("Hosting is off").size(18.0).strong());
            ui.label(
                RichText::new("Start hosting to let your other devices find this PC, see its screen and control it.")
                    .color(MUTED),
            );
            ui.add_space(6.0);
            if big_button(ui, "Start hosting", true).clicked() {
                actions.push(Action::StartHost);
            }
        }
        Some(st) => {
            ui.horizontal(|ui| {
                let (text, color) =
                    if st.client.is_some() { ("● Streaming", GOOD) } else { ("● Waiting for a device", ACCENT) };
                ui.label(RichText::new(text).size(18.0).strong().color(color));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Stop hosting").clicked() {
                        actions.push(Action::StopHost);
                    }
                    if st.client.is_some() && ui.button("Disconnect device").clicked() {
                        actions.push(Action::KickClient);
                    }
                });
            });
            ui.label(format!(
                "This PC: {}   ·   Address: {}",
                cfg.host.name,
                if st.addresses.is_empty() { "unknown".into() } else { st.addresses.join(", ") }
            ));
            if let Some(pin) = &st.pin {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(Color32::from_rgb(40, 48, 66))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(14.0)
                    .show(ui, |ui| {
                        ui.label(format!(
                            "'{}' wants to pair. Enter this PIN on that device:",
                            st.pin_for.clone().unwrap_or_default()
                        ));
                        ui.label(RichText::new(pin).size(48.0).monospace().strong().color(Color32::WHITE));
                    });
            }
            if let (Some(client), Some(s)) = (&st.client, &st.stream) {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Connected: {client}")).strong());
                ui.label(stream_line(s));
                ui.label(
                    RichText::new(format!(
                        "{:.0} fps · {:.1} Mbps · encode {:.1} ms · client decode {:.1} ms · loss {:.1}%",
                        st.fps,
                        st.kbps / 1000.0,
                        st.encode_ms,
                        st.client_decode_ms,
                        st.client_loss_pct
                    ))
                    .color(MUTED),
                );
            }
            for w in &st.warnings {
                ui.label(RichText::new(format!("⚠ {w}")).color(WARN));
            }
            if let Some(e) = &st.last_error {
                ui.label(RichText::new(format!("Last error: {e}")).color(BAD));
            }
            if !st.encoders.is_empty() {
                ui.label(RichText::new(format!("Encoders: {}", st.encoders.join(", "))).small().color(MUTED));
            }
        }
    });

    let mut changed = false;
    card(ui, |ui| {
        egui::CollapsingHeader::new(RichText::new("Host settings").size(16.0).strong()).default_open(false).show(
            ui,
            |ui| {
                let h = &mut cfg.host;
                egui::Grid::new("host_settings").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("PC name");
                    changed |= ui.text_edit_singleline(&mut h.name).changed();
                    ui.end_row();

                    ui.label("Screen capture");
                    egui::ComboBox::from_id_salt("capture")
                        .selected_text(match h.capture {
                            CaptureMethod::Auto => "Automatic",
                            CaptureMethod::X11 => "X11",
                            CaptureMethod::Portal => "Wayland / PipeWire portal",
                            CaptureMethod::Windows => "Windows Desktop Duplication",
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui.selectable_value(&mut h.capture, CaptureMethod::Auto, "Automatic").changed();
                            if cfg!(target_os = "linux") {
                                changed |= ui.selectable_value(&mut h.capture, CaptureMethod::X11, "X11").changed();
                                changed |= ui
                                    .selectable_value(
                                        &mut h.capture,
                                        CaptureMethod::Portal,
                                        "Wayland / PipeWire portal",
                                    )
                                    .changed();
                            }
                            if cfg!(windows) {
                                changed |= ui
                                    .selectable_value(
                                        &mut h.capture,
                                        CaptureMethod::Windows,
                                        "Windows Desktop Duplication",
                                    )
                                    .changed();
                            }
                        });
                    ui.end_row();

                    ui.label("Video encoder");
                    egui::ComboBox::from_id_salt("encoder").selected_text(format!("{:?}", h.encoder)).show_ui(
                        ui,
                        |ui| {
                            for e in [
                                EncoderPreference::Auto,
                                EncoderPreference::Nvidia,
                                EncoderPreference::Vaapi,
                                EncoderPreference::IntelQsv,
                                EncoderPreference::Amd,
                                EncoderPreference::MediaFoundation,
                                EncoderPreference::Software,
                            ] {
                                changed |= ui.selectable_value(&mut h.encoder, e, format!("{e:?}")).changed();
                            }
                        },
                    );
                    ui.end_row();

                    if cfg!(windows) {
                        ui.label("Monitor");
                        changed |= ui.add(egui::DragValue::new(&mut h.monitor_index).range(0..=8)).changed();
                        ui.end_row();
                    }

                    ui.label("Audio bitrate");
                    changed |= ui.add(egui::Slider::new(&mut h.audio_bitrate_kbps, 64..=510).suffix(" kbps")).changed();
                    ui.end_row();

                    ui.label("Keyframe safety interval");
                    changed |= ui.add(egui::Slider::new(&mut h.keyframe_interval_s, 1..=30).suffix(" s")).changed();
                    ui.end_row();
                });
                changed |= ui.checkbox(&mut h.show_cursor, "Show mouse cursor in the stream").changed();
                if cfg!(target_os = "linux") {
                    changed |= ui.checkbox(&mut h.mute_host_audio, "Mute this PC's speakers while streaming").changed();
                }
                changed |=
                    ui.checkbox(&mut h.require_pairing, "Require a PIN to pair new devices (recommended)").changed();
                if h.portal_restore_token.is_some() && ui.button("Forget screen-share permission").clicked() {
                    h.portal_restore_token = None;
                    changed = true;
                }
                if !h.paired_clients.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Paired devices").strong());
                    let mut remove = None;
                    for (i, c) in h.paired_clients.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(&c.name);
                            if ui.small_button("Remove").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        h.paired_clients.remove(i);
                        changed = true;
                    }
                }
                ui.label(RichText::new("Changes apply to the next connection.").small().color(MUTED));
            },
        );
    });
    if changed {
        actions.push(Action::SaveConfig);
    }
}

pub fn stream_line(s: &StreamConfig) -> String {
    format!(
        "{}x{} @ {} fps · {} · {:.0} Mbps · {} · {}",
        s.width,
        s.height,
        s.fps,
        s.codec.label(),
        s.bitrate_kbps as f32 / 1000.0,
        s.encoder,
        s.capture
    )
}

pub struct OverlayInfo<'a> {
    pub host_name: &'a str,
    pub stream: &'a StreamConfig,
    pub stats: &'a SessionStats,
    pub show_stats: bool,
    pub captured: bool,
    pub relative: bool,
    pub toast: Option<&'a str>,
    pub render_fps: f32,
    pub present_ms: f32,
    pub waiting_for_video: bool,
}

pub enum OverlayAction {
    Resume,
    Disconnect,
    ToggleStats,
    ToggleFullscreen,
    ToggleMouseMode,
}

pub fn overlay(ctx: &egui::Context, info: &OverlayInfo) -> Vec<OverlayAction> {
    let mut actions = Vec::new();
    let panel = egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(10, 12, 16, 200))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(10.0);
    if info.show_stats {
        egui::Area::new(egui::Id::unique("stats"))
            .anchor(egui::Align2::LEFT_TOP, [12.0, 12.0])
            .interactable(false)
            .show(ctx, |ui| {
                panel.show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    let s = info.stats;
                    let net_color = if s.loss_pct > 1.0 {
                        BAD
                    } else if s.loss_pct > 0.1 {
                        WARN
                    } else {
                        GOOD
                    };
                    let mono = |t: String| RichText::new(t).monospace().size(13.0);
                    ui.label(mono(format!("{}  ·  {}", info.host_name, stream_line(info.stream))));
                    ui.label(mono(format!(
                        "Frames   host {:5.1} fps   decoded {:5.1} fps   shown {:5.1} fps",
                        s.host_fps, s.decode_fps, info.render_fps
                    )));
                    ui.label(mono(format!(
                        "Latency  encode {:4.1} ms + network {:4.1} ms + decode {:4.1} ms + display {:4.1} ms",
                        s.host_encode_ms,
                        s.rtt_ms / 2.0,
                        s.decode_ms,
                        info.present_ms
                    )));
                    ui.label(
                        mono(format!(
                            "Network  {:5.1} Mbps   loss {:4.2}%   RTT {:4.1} ms",
                            s.received_kbps / 1000.0,
                            s.loss_pct,
                            s.rtt_ms
                        ))
                        .color(net_color),
                    );
                    ui.label(mono(format!(
                        "Decoder  {}   mouse: {}",
                        s.decoder,
                        if info.relative { "game (raw)" } else { "desktop (absolute)" }
                    )));
                    if !s.gamepads.is_empty() {
                        ui.label(mono(format!("Pads     {}", s.gamepads.join(", "))));
                    }
                });
            });
    }
    if let Some(t) = info.toast {
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -40.0])
            .interactable(false)
            .show(ctx, |ui| {
                panel.show(ui, |ui| {
                    ui.label(RichText::new(t).size(16.0));
                });
            });
    }
    if info.waiting_for_video {
        egui::Area::new(egui::Id::unique("waiting"))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .interactable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Waiting for video…").size(18.0));
                });
            });
    }
    if !info.captured {
        egui::Area::new(egui::Id::unique("paused")).anchor(egui::Align2::CENTER_TOP, [0.0, 16.0]).show(ctx, |ui| {
            panel.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Input released — click the picture to control the host").size(15.0));
                    if ui.button("Resume").clicked() {
                        actions.push(OverlayAction::Resume);
                    }
                    if ui.button(if info.show_stats { "Hide stats" } else { "Stats" }).clicked() {
                        actions.push(OverlayAction::ToggleStats);
                    }
                    if ui.button(if info.relative { "Desktop mouse" } else { "Game mouse" }).clicked() {
                        actions.push(OverlayAction::ToggleMouseMode);
                    }
                    if ui.button("Fullscreen").clicked() {
                        actions.push(OverlayAction::ToggleFullscreen);
                    }
                    if ui.button(RichText::new("Disconnect").color(BAD)).clicked() {
                        actions.push(OverlayAction::Disconnect);
                    }
                });
            });
        });
    }
    actions
}
