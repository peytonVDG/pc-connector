//! The application window: launcher UI and the streaming view in one winit window.

mod gfx;
mod screens;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

use crate::client::input::{Hotkey, KeyFilter};
use crate::client::{self, ClientSession, ConnectOutcome, ConnectRequest};
use crate::config::Config;
use crate::host::HostServer;
use crate::protocol::{InputEvent, mouse_button};
use gfx::{EguiFrame, Gfx};
use screens::{Action, Connecting, Launcher, OverlayAction, OverlayInfo, Pairing};

#[allow(clippy::large_enum_variant)] // rare event; boxing buys nothing
pub enum UserEvent {
    Frame,
    #[cfg_attr(not(windows), allow(dead_code))] // sent by the Windows keyboard hook
    Hotkey(Hotkey),
    ConnectDone(u64, String, String, Result<ConnectOutcome, String>),
}

struct Stream {
    session: ClientSession,
    captured: bool,
    relative: bool,
    filter: KeyFilter,
    acc: (f64, f64),
    scroll_acc: (f64, f64),
    show_stats: bool,
    toast: Option<(String, Instant)>,
    #[cfg(windows)]
    hook: Option<crate::client::kbhook::KeyboardHook>,
    hook_active: bool,
    has_video: bool,
    started: Instant,
    shown: u32,
    shown_window: Instant,
    render_fps: f32,
    present_ms: f32,
    last_overlay: Instant,
}

impl Stream {
    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }
    fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(5)).map(|(s, _)| s.as_str())
    }
    fn overlay_visible(&self) -> bool {
        self.show_stats || !self.captured || self.toast_text().is_some() || !self.has_video
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    proxy_shared: Arc<Mutex<EventLoopProxy<UserEvent>>>,
    cfg: Arc<Mutex<Config>>,
    window: Option<Arc<Window>>,
    gfx: Option<Gfx>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    launcher: Launcher,
    stream: Option<Stream>,
    host: Option<HostServer>,
    connect_gen: u64,
    frame_pending: Arc<AtomicBool>,
    autoconnect: Option<(String, Option<String>)>,
    next_refresh: Instant,
    fatal: Option<String>,
}

pub fn run(cfg: Arc<Mutex<Config>>, autoconnect: Option<(String, Option<String>)>, start_host: bool) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let launcher = Launcher::new(&cfg.lock().unwrap());
    let egui_ctx = egui::Context::default();
    screens::style(&egui_ctx);
    let mut app = App {
        proxy_shared: Arc::new(Mutex::new(proxy.clone())),
        proxy,
        cfg,
        window: None,
        gfx: None,
        egui_ctx,
        egui_state: None,
        launcher,
        stream: None,
        host: None,
        connect_gen: 0,
        frame_pending: Arc::new(AtomicBool::new(false)),
        autoconnect,
        next_refresh: Instant::now(),
        fatal: None,
    };
    if start_host {
        app.start_host();
    }
    event_loop.run_app(&mut app)?;
    if let Some(e) = app.fatal {
        anyhow::bail!(e);
    }
    Ok(())
}

impl App {
    fn window(&self) -> &Arc<Window> {
        self.window.as_ref().expect("window")
    }

    fn start_host(&mut self) {
        if self.host.is_some() {
            return;
        }
        match HostServer::start(self.cfg.clone()) {
            Ok(h) => {
                self.host = Some(h);
                self.launcher.notice = None;
            }
            Err(e) => self.launcher.notice = Some((format!("Could not start hosting: {e:#}"), true)),
        }
    }

    fn connect(&mut self, address: String, label: String, pin: Option<String>) {
        if self.stream.is_some() {
            return;
        }
        self.connect_gen += 1;
        let generation = self.connect_gen;
        let display_size = self
            .window
            .as_ref()
            .and_then(|w| w.current_monitor())
            .map(|m| (m.size().width, m.size().height))
            .unwrap_or((1920, 1080));
        let req = {
            let cfg = self.cfg.lock().unwrap();
            ConnectRequest {
                address: address.clone(),
                pin,
                client_id: cfg.client_id.clone(),
                client_name: cfg.client_name.clone(),
                settings: cfg.client.clone(),
                display_size,
            }
        };
        self.launcher.connecting = Some(Connecting { label: label.clone(), started: Instant::now() });
        self.launcher.notice = None;
        let proxy = self.proxy.clone();
        let shared = self.proxy_shared.clone();
        let pending = self.frame_pending.clone();
        let waker: client::video::Waker = Arc::new(move || {
            // Coalesce: one wake-up per frame is enough even if frames arrive faster than we draw.
            if !pending.swap(true, Ordering::AcqRel) {
                let _ = shared.lock().unwrap().send_event(UserEvent::Frame);
            }
        });
        std::thread::spawn(move || {
            let result = client::connect(req, waker).map_err(|e| format!("{e:#}"));
            let _ = proxy.send_event(UserEvent::ConnectDone(generation, address, label, result));
        });
    }

    fn on_connect_done(&mut self, address: String, label: String, result: Result<ConnectOutcome, String>) {
        self.launcher.connecting = None;
        match result {
            Ok(ConnectOutcome::Connected(session)) => self.begin_stream(session, &address),
            Ok(ConnectOutcome::PairingRequired { message }) => {
                self.launcher.pairing = Some(Pairing { address, label, message, pin: String::new() });
            }
            Ok(ConnectOutcome::Rejected { reason }) => self.launcher.notice = Some((reason, true)),
            Err(e) => self.launcher.notice = Some((e, true)),
        }
        self.window().request_redraw();
    }

    fn begin_stream(&mut self, session: ClientSession, address: &str) {
        let settings = {
            let mut cfg = self.cfg.lock().unwrap();
            cfg.remember_host(&session.host_name, address);
            cfg.save();
            cfg.client.clone()
        };
        // Stop LAN discovery broadcasts while streaming (broadcasts go out at the slowest
        // Wi-Fi rate and steal a little airtime).
        self.launcher.browser = None;
        let window = self.window().clone();
        window.set_title(&format!("PC Connector — {}", session.host_name));
        if settings.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        if let Some(g) = &mut self.gfx {
            g.set_vsync(settings.vsync);
            g.video.clear();
        }
        let mut stream = Stream {
            session,
            captured: false,
            relative: settings.relative_mouse,
            filter: KeyFilter::default(),
            acc: (0.0, 0.0),
            scroll_acc: (0.0, 0.0),
            show_stats: settings.show_stats,
            toast: None,
            #[cfg(windows)]
            hook: None,
            hook_active: false,
            has_video: false,
            started: Instant::now(),
            shown: 0,
            shown_window: Instant::now(),
            render_fps: 0.0,
            present_ms: 0.0,
            last_overlay: Instant::now(),
        };
        #[cfg(windows)]
        if settings.capture_system_keys {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = window.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    let proxy = self.proxy_shared.clone();
                    stream.hook = crate::client::kbhook::install(
                        stream.session.input.clone(),
                        w.hwnd.get(),
                        Box::new(move |hk| {
                            let _ = proxy.lock().unwrap().send_event(UserEvent::Hotkey(hk));
                        }),
                    );
                    stream.hook_active = stream.hook.is_some();
                }
            }
        }
        stream.toast("Ctrl+Alt+Shift+Z releases the mouse · Ctrl+Alt+Shift+Q disconnects");
        self.stream = Some(stream);
        self.set_captured(true);
        window.request_redraw();
    }

    fn end_stream(&mut self, reason: Option<String>) {
        let Some(stream) = self.stream.take() else { return };
        let window = self.window().clone();
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor_visible(true);
        window.set_fullscreen(None);
        window.set_title("PC Connector");
        #[cfg(windows)]
        crate::client::kbhook::set_active(false);
        drop(stream); // sends goodbye, releases keys, stops pipelines
        if let Some(g) = &mut self.gfx {
            g.video.clear();
            g.set_vsync(true);
        }
        if let Some(r) = reason {
            self.launcher.notice = Some((format!("Disconnected: {r}"), true));
        }
        window.request_redraw();
    }

    fn set_captured(&mut self, captured: bool) {
        let window = self.window().clone();
        let Some(s) = &mut self.stream else { return };
        s.captured = captured;
        s.filter.reset();
        if captured {
            window.set_cursor_visible(false);
            if s.relative {
                if window.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                    let _ = window.set_cursor_grab(CursorGrabMode::Confined);
                }
            } else {
                let _ = window.set_cursor_grab(CursorGrabMode::None);
            }
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            s.session.input.send(InputEvent::ReleaseAll);
        }
        #[cfg(windows)]
        crate::client::kbhook::set_active(captured && s.hook_active);
        window.request_redraw();
    }

    fn handle_hotkey(&mut self, hk: Hotkey) {
        let window = self.window().clone();
        let Some(s) = &mut self.stream else { return };
        match hk {
            Hotkey::Disconnect => self.end_stream(None),
            Hotkey::ToggleCapture => {
                let c = !s.captured;
                self.set_captured(c);
            }
            Hotkey::ToggleMouseMode => {
                s.relative = !s.relative;
                let msg = if s.relative {
                    "Game mouse mode (locked, raw movement)"
                } else {
                    "Desktop mouse mode (absolute pointer)"
                };
                s.toast(msg);
                let c = s.captured;
                self.set_captured(c);
            }
            Hotkey::ToggleFullscreen => {
                if window.fullscreen().is_some() {
                    window.set_fullscreen(None);
                } else {
                    window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                }
            }
            Hotkey::ToggleStats => {
                s.show_stats = !s.show_stats;
                window.request_redraw();
            }
        }
    }

    /// Input while streaming with capture on. Returns true if the event was consumed.
    fn stream_input(&mut self, event: &WindowEvent) -> bool {
        let Some(s) = &mut self.stream else { return false };
        if !s.captured {
            return false;
        }
        match event {
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if *is_synthetic || s.hook_active {
                    return true;
                }
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(hid) = crate::keymap::winit_to_hid(code)
                {
                    let down = event.state == ElementState::Pressed;
                    if let Some(hk) = s.filter.on_key(hid, down) {
                        self.handle_hotkey(hk);
                    } else {
                        s.session.input.send(InputEvent::Key { hid, down });
                    }
                }
                true
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => mouse_button::LEFT,
                    MouseButton::Middle => mouse_button::MIDDLE,
                    MouseButton::Right => mouse_button::RIGHT,
                    MouseButton::Back => mouse_button::BACK,
                    MouseButton::Forward => mouse_button::FORWARD,
                    MouseButton::Other(_) => return true,
                };
                s.session.input.send(InputEvent::MouseButton { button: b, down: *state == ElementState::Pressed });
                true
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (*x as f64 * 120.0, *y as f64 * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x * 2.4, p.y * 2.4),
                };
                s.scroll_acc.0 -= x; // winit: positive x scrolls left; the wire uses positive = right
                s.scroll_acc.1 += y;
                let (h, v) = (s.scroll_acc.0.trunc(), s.scroll_acc.1.trunc());
                if h != 0.0 || v != 0.0 {
                    s.scroll_acc.0 -= h;
                    s.scroll_acc.1 -= v;
                    s.session.input.send(InputEvent::Scroll {
                        vertical: v.clamp(-32768.0, 32767.0) as i16,
                        horizontal: h.clamp(-32768.0, 32767.0) as i16,
                    });
                }
                true
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !s.relative
                    && let Some(g) = &self.gfx
                    && let Some(vs) = g.video.video_size()
                {
                    let r = gfx::letterbox(g.size(), vs);
                    let nx = ((position.x as f32 - r[0]) / r[2]).clamp(0.0, 1.0);
                    let ny = ((position.y as f32 - r[1]) / r[3]).clamp(0.0, 1.0);
                    s.session.input.send(InputEvent::MouseAbs { x: (nx * 65535.0) as u16, y: (ny * 65535.0) as u16 });
                }
                true
            }
            WindowEvent::Touch(_) | WindowEvent::ModifiersChanged(_) => true,
            _ => false,
        }
    }

    fn run_egui(&mut self, build: impl FnMut(&mut egui::Ui)) -> Option<EguiFrame> {
        run_egui(self.window.as_ref()?, self.egui_state.as_mut()?, &self.egui_ctx, &mut self.next_refresh, build)
    }

    fn redraw(&mut self) {
        if self.gfx.is_none() {
            return;
        }
        if self.stream.is_some() {
            self.redraw_stream();
        } else {
            self.redraw_launcher();
        }
    }

    fn redraw_stream(&mut self) {
        let frame = self.stream.as_ref().unwrap().session.frames.take();
        let mut decoded_at = None;
        if let Some(f) = frame {
            self.gfx.as_mut().unwrap().upload_frame(&f.sample);
            decoded_at = Some(f.decoded_at);
        }
        let s = self.stream.as_mut().unwrap();
        if decoded_at.is_some() {
            s.has_video = true;
        }
        let overlay = s.overlay_visible();
        let egui = if overlay {
            s.last_overlay = Instant::now();
            let stats = s.session.stats();
            let stream_cfg = s.session.stream.clone();
            let host_name = s.session.host_name.clone();
            let info_owned = (
                s.show_stats,
                s.captured,
                s.relative,
                s.toast_text().map(str::to_string),
                s.render_fps,
                s.present_ms,
                !s.has_video && s.started.elapsed() > Duration::from_millis(300),
            );
            let mut actions = Vec::new();
            let frame = self.run_egui(|ui| {
                let info = OverlayInfo {
                    host_name: &host_name,
                    stream: &stream_cfg,
                    stats: &stats,
                    show_stats: info_owned.0,
                    captured: info_owned.1,
                    relative: info_owned.2,
                    toast: info_owned.3.as_deref(),
                    render_fps: info_owned.4,
                    present_ms: info_owned.5,
                    waiting_for_video: info_owned.6,
                };
                actions = screens::overlay(ui.ctx(), &info);
            });
            for a in actions {
                match a {
                    OverlayAction::Resume => self.set_captured(true),
                    OverlayAction::Disconnect => {
                        self.end_stream(None);
                        return;
                    }
                    OverlayAction::ToggleStats => self.handle_hotkey(Hotkey::ToggleStats),
                    OverlayAction::ToggleFullscreen => self.handle_hotkey(Hotkey::ToggleFullscreen),
                    OverlayAction::ToggleMouseMode => self.handle_hotkey(Hotkey::ToggleMouseMode),
                }
            }
            frame
        } else {
            None
        };
        let Some(g) = self.gfx.as_mut() else { return };
        g.render(true, egui);
        if let (Some(t0), Some(s)) = (decoded_at, self.stream.as_mut()) {
            let ms = t0.elapsed().as_secs_f32() * 1000.0;
            s.present_ms = if s.present_ms == 0.0 { ms } else { s.present_ms * 0.9 + ms * 0.1 };
            s.shown += 1;
            let el = s.shown_window.elapsed().as_secs_f32();
            if el >= 1.0 {
                s.render_fps = s.shown as f32 / el;
                s.shown = 0;
                s.shown_window = Instant::now();
            }
        }
    }

    fn redraw_launcher(&mut self) {
        if let Some(g) = &self.gfx
            && self.launcher.adapter.is_empty()
        {
            self.launcher.adapter = g.adapter_name.clone();
        }
        let host_status = self.host.as_ref().map(|h| h.status());
        let mut actions = Vec::new();
        let frame = {
            // The UI edits the shared config in place (host threads only hold the lock briefly).
            let cfg_arc = self.cfg.clone();
            let mut cfg = cfg_arc.lock().unwrap();
            let (Some(window), Some(state)) = (self.window.as_ref(), self.egui_state.as_mut()) else { return };
            let launcher = &mut self.launcher;
            run_egui(window, state, &self.egui_ctx, &mut self.next_refresh, |ui| {
                actions = screens::launcher(ui, launcher, &mut cfg, host_status.as_ref());
            })
        };
        if let Some(g) = self.gfx.as_mut() {
            g.render(false, frame);
        }
        for a in actions {
            match a {
                Action::Connect { address, label, pin } => self.connect(address, label, pin),
                Action::CancelConnect => {
                    self.connect_gen += 1; // ignore the pending result
                    self.launcher.connecting = None;
                }
                Action::StartHost => self.start_host(),
                Action::StopHost => {
                    if let Some(h) = self.host.take() {
                        std::thread::spawn(move || h.stop());
                    }
                }
                Action::KickClient => {
                    if let Some(h) = &self.host {
                        h.kick();
                    }
                }
                Action::SaveConfig => self.cfg.lock().unwrap().save(),
            }
            self.window().request_redraw();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("PC Connector")
            .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 760.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(560.0, 420.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.fatal = Some(format!("Could not create a window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let gfx = match Gfx::new(window.clone()) {
            Ok(g) => g,
            Err(e) => {
                self.fatal = Some(format!("Graphics initialisation failed: {e:#}"));
                event_loop.exit();
                return;
            }
        };
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(gfx.max_texture_side()),
        ));
        self.gfx = Some(gfx);
        self.window = Some(window.clone());
        if let Some((addr, pin)) = self.autoconnect.take() {
            self.connect(addr.clone(), addr, pin);
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested => {
                self.end_stream(None);
                if let Some(h) = self.host.take() {
                    h.stop();
                }
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gfx {
                    g.resize(size.width, size.height);
                }
                self.window().request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Focused(false) if self.stream.as_ref().is_some_and(|s| s.captured) => {
                self.set_captured(false);
            }
            _ => {}
        }
        if self.stream_input(&event) {
            return;
        }
        let window = self.window().clone();
        let Some(state) = self.egui_state.as_mut() else { return };
        let response = state.on_window_event(&window, &event);
        // Clicking the picture while input is released takes control again.
        if let WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } = event
            && self.stream.as_ref().is_some_and(|s| !s.captured)
            && !response.consumed
            && !self.egui_ctx.is_pointer_over_egui()
        {
            self.set_captured(true);
            return;
        }
        if response.repaint {
            window.request_redraw();
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && let Some(s) = &mut self.stream
            && s.captured
            && s.relative
        {
            s.acc.0 += delta.0;
            s.acc.1 += delta.1;
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => {
                self.frame_pending.store(false, Ordering::Release);
                // Draw right away rather than waiting for a redraw request: lowest latency.
                if self.stream.is_some() {
                    self.flush_mouse();
                    self.redraw();
                }
            }
            UserEvent::Hotkey(hk) => self.handle_hotkey(hk),
            UserEvent::ConnectDone(generation, address, label, result) => {
                if generation == self.connect_gen {
                    self.on_connect_done(address, label, result);
                }
                // A stale result (cancelled attempt) is dropped here, closing that session.
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_mouse();
        let mut ended = None;
        if let Some(s) = &self.stream {
            ended = s.session.ended();
        }
        if let Some(reason) = ended {
            self.end_stream(Some(reason));
        }
        let now = Instant::now();
        if let Some(s) = &self.stream {
            // Keep the overlay (stats/toasts) fresh even if no frames arrive.
            if s.overlay_visible() && s.last_overlay.elapsed() > Duration::from_millis(250) {
                self.window().request_redraw();
            } else if s.toast.is_some()
                && s.toast_text().is_none()
                && s.last_overlay.elapsed() > Duration::from_millis(250)
            {
                // toast just expired: one more draw to remove it
                self.window().request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(100)));
        } else {
            if now >= self.next_refresh {
                self.next_refresh = now + Duration::from_millis(500);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh));
        }
    }
}

fn run_egui(
    window: &Arc<Window>,
    state: &mut egui_winit::State,
    ctx: &egui::Context,
    next_refresh: &mut Instant,
    build: impl FnMut(&mut egui::Ui),
) -> Option<EguiFrame> {
    let raw = state.take_egui_input(window);
    let out = ctx.run_ui(raw, build);
    state.handle_platform_output(window, out.platform_output);
    if let Some(vp) = out.viewport_output.get(&egui::ViewportId::ROOT) {
        if vp.repaint_delay.is_zero() {
            window.request_redraw();
        } else if vp.repaint_delay < Duration::from_secs(1) {
            *next_refresh = (*next_refresh).min(Instant::now() + vp.repaint_delay);
        }
    }
    let primitives = ctx.tessellate(out.shapes, out.pixels_per_point);
    Some(EguiFrame { primitives, textures: out.textures_delta, pixels_per_point: out.pixels_per_point })
}

impl App {
    fn flush_mouse(&mut self) {
        if let Some(s) = &mut self.stream {
            let (dx, dy) = (s.acc.0.trunc(), s.acc.1.trunc());
            if dx != 0.0 || dy != 0.0 {
                s.acc.0 -= dx;
                s.acc.1 -= dy;
                s.session.input.send(InputEvent::MouseMove { dx: dx as i32, dy: dy as i32 });
            }
        }
    }
}
