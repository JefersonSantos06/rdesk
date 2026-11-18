//! The viewer window: winit + wgpu, with a small egui overlay (menu, monitors,
//! files, stats). Raw winit key events are forwarded as physical keys.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::{Align2, Color32, ColorImage, TextureHandle, TextureOptions, ViewportId};
use egui_wgpu::winit::Painter;
use egui_winit::winit;
use rdesk_proto::{DirEntry, HostInfo, InputEvent, MonitorInfo, MouseButton};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowId};

use crate::conn::{self, Cmd, ConnectOpts, Handle, UiEvent, Update};
use crate::keys;

pub struct ViewerOpts {
    pub connect: ConnectOpts,
    pub swap_cmd_ctrl: bool,
    pub download_dir: PathBuf,
}

pub fn run(opts: ViewerOpts) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let name = opts.connect.name.clone();
    let handle = conn::spawn(opts.connect, move |ev| {
        let _ = proxy.send_event(ev);
    });
    let mut app = App::new(handle, name, opts.swap_cmd_ctrl, opts.download_dir);
    event_loop.run_app(&mut app)?;
    let _ = app.handle.cmd.send(Cmd::Quit);
    Ok(())
}

struct Gfx {
    window: Arc<Window>,
    state: egui_winit::State,
    painter: Painter,
}

struct Transfer {
    name: String,
    done: u64,
    total: u64,
    finished: Option<Result<String, String>>,
}

struct App {
    handle: Handle,
    name: String,
    egui: egui::Context,
    gfx: Option<Gfx>,
    tex: Option<TextureHandle>,
    status: String,
    fatal: Option<String>,
    host: Option<HostInfo>,
    monitors: Vec<MonitorInfo>,
    active: u32,
    stats: (u32, f32, f32),
    menu_open: bool,
    /// Where the remote image is drawn, in egui points.
    image_rect: Option<egui::Rect>,
    pending_move: Option<(u16, u16)>,
    pointer_inside: bool,
    swap_cmd_ctrl: bool,
    fps: u8,
    quality: u8,
    remote_path: String,
    listing: Option<(String, Result<Vec<DirEntry>, String>)>,
    transfers: BTreeMap<u64, Transfer>,
    download_dir: PathBuf,
    next_repaint: Option<Instant>,
}

impl App {
    fn new(handle: Handle, name: String, swap_cmd_ctrl: bool, download_dir: PathBuf) -> Self {
        Self {
            handle,
            name,
            egui: egui::Context::default(),
            gfx: None,
            tex: None,
            status: "Connecting…".into(),
            fatal: None,
            host: None,
            monitors: Vec::new(),
            active: 0,
            stats: (0, 0.0, 0.0),
            menu_open: false,
            image_rect: None,
            pending_move: None,
            pointer_inside: false,
            swap_cmd_ctrl,
            fps: 30,
            quality: 70,
            remote_path: "~".into(),
            listing: None,
            transfers: BTreeMap::new(),
            download_dir,
            next_repaint: None,
        }
    }

    fn send(&self, c: Cmd) {
        let _ = self.handle.cmd.send(c);
    }

    fn input(&self, ev: InputEvent) {
        self.send(Cmd::Input(ev));
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn update_title(&self) {
        if let Some(g) = &self.gfx {
            let host = self.host.as_ref().map(|h| h.hostname.as_str()).unwrap_or(&self.name);
            g.window.set_title(&format!("rdesk — {host} — {}", self.status));
        }
    }

    /// Map a window position (physical pixels) to normalized remote coordinates.
    fn to_remote(&self, px: f64, py: f64) -> Option<(u16, u16)> {
        let g = self.gfx.as_ref()?;
        let r = self.image_rect?;
        let ppp = g.window.scale_factor() as f32;
        let (x, y) = (px as f32 / ppp, py as f32 / ppp);
        if !r.contains(egui::pos2(x, y)) {
            return None;
        }
        let nx = ((x - r.min.x) / r.width()).clamp(0.0, 1.0);
        let ny = ((y - r.min.y) / r.height()).clamp(0.0, 1.0);
        Some(((nx * 65535.0).round() as u16, (ny * 65535.0).round() as u16))
    }

    fn upload_texture(&mut self) {
        let opts = TextureOptions::LINEAR;
        match conn::take_updates(&self.handle.view) {
            Update::None => {}
            Update::Full { width, height, rgba } => {
                let img = ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
                match &mut self.tex {
                    Some(t) if t.size() == [width as usize, height as usize] => t.set(img, opts),
                    _ => {
                        tracing::debug!("remote picture {width}x{height}");
                        self.tex = Some(self.egui.load_texture("remote", img, opts));
                    }
                }
            }
            Update::Partial { width, height, tiles } => {
                let Some(t) = &mut self.tex else { return };
                if t.size() != [width as usize, height as usize] {
                    return;
                }
                for (r, px) in tiles {
                    let img = ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &px);
                    t.set_partial([r.x as usize, r.y as usize], img, opts);
                }
            }
        }
    }

    fn redraw(&mut self) {
        self.upload_texture();
        let Some(mut gfx) = self.gfx.take() else { return };
        let raw = gfx.state.take_egui_input(&gfx.window);
        let ctx = self.egui.clone();
        let mut out = ctx.run_ui(raw, |ui| self.ui(ui));
        gfx.state.handle_platform_output(&gfx.window, out.platform_output);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        gfx.painter.paint_and_update_textures(
            ViewportId::ROOT,
            out.pixels_per_point,
            [0.05, 0.05, 0.05, 1.0],
            &prims,
            &mut out.textures_delta,
            Vec::new(),
            &gfx.window,
        );
        let delay = out.viewport_output.get(&ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
        self.next_repaint = if delay.is_zero() {
            gfx.window.request_redraw();
            None
        } else if delay < Duration::from_secs(3600) {
            Some(Instant::now() + delay)
        } else {
            None
        };
        self.gfx = Some(gfx);
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let screen = ui.max_rect();

        // Remote screen, aspect-fit, on the background layer (so the pointer over it
        // is not "over egui").
        self.image_rect = None;
        if let Some(t) = &self.tex {
            let [tw, th] = t.size();
            let scale = (screen.width() / tw as f32).min(screen.height() / th as f32);
            let size = egui::vec2(tw as f32 * scale, th as f32 * scale);
            let rect = egui::Rect::from_center_size(screen.center(), size);
            ctx.layer_painter(egui::LayerId::background()).image(
                t.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            self.image_rect = Some(rect);
        }

        // Status banner while not showing a picture or when something went wrong.
        if self.tex.is_none() || self.fatal.is_some() || !self.status.starts_with("Connected") {
            let text = self.fatal.clone().unwrap_or_else(|| self.status.clone());
            egui::Area::new(egui::Id::unique("status"))
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .interactable(false)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(egui::RichText::new(text).size(18.0));
                    });
                });
        }

        // Menu button.
        egui::Area::new(egui::Id::unique("menu_button")).anchor(Align2::CENTER_TOP, [0.0, 2.0]).show(&ctx, |ui| {
            let label = if self.menu_open { "▲ rdesk" } else { "▼ rdesk" };
            if ui.add(egui::Button::new(label).small()).clicked() {
                self.menu_open = !self.menu_open;
            }
        });

        if self.menu_open {
            let mut open = true;
            egui::Window::new("rdesk").open(&mut open).resizable(true).default_width(380.0).show(&ctx, |ui| {
                self.menu(ui);
            });
            self.menu_open = open;
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        if let Some(h) = &self.host {
            ui.label(format!("{} ({})", h.hostname, h.os));
        }
        ui.label(format!(
            "{} · RTT {} ms · {:.0} fps · {:.0} kbit/s",
            self.status, self.stats.0, self.stats.1, self.stats.2
        ));
        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.label("Monitor:");
            for m in self.monitors.clone() {
                let text = format!("{}{} {}x{}", if m.primary { "★ " } else { "" }, m.name, m.width, m.height);
                if ui.selectable_label(m.id == self.active, text).clicked() && m.id != self.active {
                    self.send(Cmd::SelectMonitor(m.id));
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("FPS");
            ui.add(egui::Slider::new(&mut self.fps, 5..=60));
            ui.label("Quality");
            ui.add(egui::Slider::new(&mut self.quality, 30..=95));
            if ui.button("Apply").clicked() {
                self.send(Cmd::SetQuality { max_fps: self.fps, jpeg_quality: self.quality });
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Full screen").clicked()
                && let Some(g) = &self.gfx
            {
                let fs = g.window.fullscreen().is_some();
                g.window.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
            }
            if ui.button("Refresh picture").clicked() {
                self.send(Cmd::FullFrame);
            }
            ui.checkbox(&mut self.swap_cmd_ctrl, "Cmd ⇄ Ctrl");
            if ui.button("Ctrl+Alt+Del").on_hover_text("Only works if the host runs elevated").clicked() {
                use rdesk_proto::Key::*;
                for k in [ControlLeft, AltLeft, Delete] {
                    self.input(InputEvent::Key { key: k, down: true });
                }
                for k in [Delete, AltLeft, ControlLeft] {
                    self.input(InputEvent::Key { key: k, down: false });
                }
            }
        });
        ui.separator();

        ui.heading("Files");
        ui.horizontal(|ui| {
            if ui.button("Send files…").clicked()
                && let Some(paths) = rfd::FileDialog::new().pick_files()
            {
                for p in paths {
                    self.send(Cmd::Push(p));
                }
            }
            ui.label("(or drop files on the window)");
        });
        ui.horizontal(|ui| {
            ui.label("Host path:");
            let resp = ui.text_edit_singleline(&mut self.remote_path);
            if ui.button("List").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                self.send(Cmd::ListDir(self.remote_path.clone()));
            }
        });
        let sep = if self.host.as_ref().is_some_and(|h| h.os == "windows") { '\\' } else { '/' };
        let mut nav: Option<String> = None;
        if let Some((path, result)) = &self.listing {
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| match result {
                Err(e) => {
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
                Ok(entries) => {
                    let parent = parent_of(path, sep);
                    if ui.link("⬆ ..").clicked() {
                        nav = Some(parent);
                    }
                    for e in entries {
                        let full = join(path, &e.name, sep);
                        ui.horizontal(|ui| {
                            if e.is_dir {
                                if ui.link(format!("📁 {}", e.name)).clicked() {
                                    nav = Some(full);
                                }
                            } else {
                                ui.label(format!("📄 {} ({})", e.name, human(e.size)));
                                if ui.small_button("Download").clicked() {
                                    let _ = self
                                        .handle
                                        .cmd
                                        .send(Cmd::Pull { remote: full, local_dir: self.download_dir.clone() });
                                }
                            }
                        });
                    }
                }
            });
        }
        if let Some(p) = nav {
            self.remote_path = p.clone();
            self.send(Cmd::ListDir(p));
        }
        ui.label(format!("Downloads go to {}", self.download_dir.display()));

        if !self.transfers.is_empty() {
            ui.separator();
            for t in self.transfers.values() {
                match &t.finished {
                    None => {
                        let frac = if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(frac).text(format!(
                            "{} {}/{}",
                            t.name,
                            human(t.done),
                            human(t.total)
                        )));
                    }
                    Some(Ok(where_)) => {
                        ui.label(format!("✔ {} → {where_}", t.name));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::LIGHT_RED, format!("✘ {}: {e}", t.name));
                    }
                }
            }
            if ui.small_button("Clear finished").clicked() {
                self.transfers.retain(|_, t| t.finished.is_none());
            }
        }
    }

    fn handle_key(&self, event: &winit::event::KeyEvent) -> bool {
        let PhysicalKey::Code(code) = event.physical_key else { return false };
        let Some(mut key) = keys::map(code) else { return false };
        if self.swap_cmd_ctrl {
            key = keys::swap_cmd_ctrl(key);
        }
        self.input(InputEvent::Key { key, down: event.state == ElementState::Pressed });
        true
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("rdesk — {}", self.name))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let mut painter = pollster::block_on(Painter::new(
            self.egui.clone(),
            egui_wgpu::WgpuConfiguration::default(),
            false,
            egui_wgpu::RendererOptions::default(),
        ));
        pollster::block_on(painter.set_window(ViewportId::ROOT, Some(window.clone()))).expect("init GPU surface");
        let state = egui_winit::State::new(
            self.egui.clone(),
            ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            painter.max_texture_side(),
        );
        self.gfx = Some(Gfx { window, state, painter });
        self.request_redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let over_egui = self.egui.is_pointer_over_egui();
        let wants_kb = self.egui.egui_wants_keyboard_input();
        match &event {
            WindowEvent::CloseRequested => {
                self.send(Cmd::Quit);
                el.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Resized(size) => {
                if let (Some(g), Some(w), Some(h)) =
                    (self.gfx.as_mut(), NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    g.painter.on_window_resized(ViewportId::ROOT, w, h);
                }
            }
            WindowEvent::Focused(false) => self.input(InputEvent::ReleaseAll),
            WindowEvent::DroppedFile(p) => {
                self.send(Cmd::Push(p.clone()));
                self.menu_open = true;
            }
            WindowEvent::KeyboardInput { event: kev, is_synthetic, .. } => {
                // Synthetic events are winit replaying held keys on focus changes.
                if !wants_kb && !is_synthetic && self.handle_key(kev) {
                    return;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let remote = self.to_remote(position.x, position.y);
                self.pointer_inside = remote.is_some() && !over_egui;
                if let Some(p) = remote
                    && !over_egui
                {
                    self.pending_move = Some(p);
                }
            }
            WindowEvent::MouseInput { state, button, .. } if self.pointer_inside && !over_egui => {
                let b = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                if let Some(p) = self.pending_move.take() {
                    self.input(InputEvent::MouseMove { x: p.0, y: p.1 });
                }
                self.input(InputEvent::MouseButton { button: b, down: *state == ElementState::Pressed });
                return;
            }
            WindowEvent::MouseWheel { delta, .. } if self.pointer_inside && !over_egui => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (*x, *y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 40.0, p.y as f32 / 40.0),
                };
                self.input(InputEvent::Scroll { dx, dy });
                return;
            }
            _ => {}
        }
        if let Some(g) = self.gfx.as_mut() {
            let resp = g.state.on_window_event(&g.window, &event);
            if resp.repaint {
                g.window.request_redraw();
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::FrameUpdated => {}
            UiEvent::Status(s) => {
                self.status = s;
                self.update_title();
            }
            UiEvent::Connected(h) => {
                self.fatal = None;
                self.monitors = h.monitors.clone();
                self.active = h.active_monitor;
                self.host = Some(h);
            }
            UiEvent::Monitors { monitors, active } => {
                self.monitors = monitors;
                self.active = active;
            }
            UiEvent::DirListing { path, result } => {
                if result.is_ok() {
                    self.remote_path = path.clone();
                }
                self.listing = Some((path, result));
            }
            UiEvent::Transfer { id, name, done, total, finished } => {
                let t = self.transfers.entry(id).or_insert(Transfer { name, done: 0, total: 0, finished: None });
                if finished.is_some() {
                    t.finished = finished;
                } else {
                    t.done = done;
                    t.total = total;
                }
            }
            UiEvent::Stats { rtt_ms, fps, kbps } => self.stats = (rtt_ms, fps, kbps),
            UiEvent::Fatal(msg) => {
                self.status = "Disconnected".into();
                self.fatal = Some(msg);
                self.update_title();
            }
            UiEvent::Quit => {
                el.exit();
                return;
            }
        }
        self.request_redraw();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(p) = self.pending_move.take() {
            self.input(InputEvent::MouseMove { x: p.0, y: p.1 });
        }
        match self.next_repaint {
            Some(t) if t <= Instant::now() => {
                self.next_repaint = None;
                self.request_redraw();
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

fn join(dir: &str, name: &str, sep: char) -> String {
    if dir.ends_with(['/', '\\']) { format!("{dir}{name}") } else { format!("{dir}{sep}{name}") }
}

fn parent_of(path: &str, sep: char) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(0) => sep.to_string(),
        Some(i) => {
            let p = &trimmed[..i];
            // Keep "C:\" rather than "C:".
            if p.ends_with(':') { format!("{p}{sep}") } else { p.to_string() }
        }
        None => path.to_string(),
    }
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(join("/home/j", "a.txt", '/'), "/home/j/a.txt");
        assert_eq!(join("C:\\", "x", '\\'), "C:\\x");
        assert_eq!(parent_of("/home/j", '/'), "/home");
        assert_eq!(parent_of("/home", '/'), "/");
        assert_eq!(parent_of("C:\\Users", '\\'), "C:\\");
        assert_eq!(human(1536), "1.5 KB");
    }
}
