//! The session window: winit + wgpu with egui on top. A toolbar along the top (status,
//! monitor, picture quality, special keys, files, terminal, full screen, disconnect), the
//! remote screen in the space left, and a side panel on the right with the files, the
//! terminals and facts about the session. Sessions without a screen (files or terminal
//! only) give the window to that panel. Raw winit key events are forwarded as physical
//! keys.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::containers::menu::{MenuButton, MenuConfig};
use egui::{
    Align, Align2, Color32, ColorImage, CornerRadius, Layout, Margin, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Vec2, ViewportId,
};
use egui_wgpu::winit::Painter;
use egui_winit::winit;
use rdesk_common::i18n::access_label;
use rdesk_common::{t, tf};
use rdesk_proto::{DirEntry, Feature, HostInfo, InputEvent, Key, MonitorInfo, MouseButton, NavAction};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowId};

use crate::conn::{self, Cmd, ConnectOpts, Handle, PasswordAnswer, PasswordReply, Raise, Status, UiEvent, Update};
use crate::keys;
use crate::terminal::TermView;
use crate::ui::{self, Tone, icons, size, space};

/// A drop shorter than this is not worth a dialog (the picture just pauses).
const OFFLINE_GRACE: Duration = Duration::from_millis(1500);
/// After this long, say the computer may be off.
const OFFLINE_LONG: Duration = Duration::from_secs(20);
/// How often the lock keys here are looked at while the window has the keyboard.
const LOCKS_EVERY: Duration = Duration::from_millis(150);
/// The side panel's width: what it starts with and how narrow it can be dragged. It can be
/// dragged wide until the picture next to it is [`PICTURE_MIN`] wide (a terminal wants room).
const PANEL_DEFAULT: f32 = 380.0;
const PANEL_MIN: f32 = 300.0;
const PICTURE_MIN: f32 = 200.0;
/// A file in the list.
const ROW_H: f32 = 44.0;
/// Controls in the toolbar.
const CONTROL_H: f32 = 32.0;
/// In full screen, how close to the top edge the mouse brings the toolbar back.
const REVEAL_EDGE: f32 = 4.0;
/// The host name keeps at least this much room in the toolbar.
const NAME_MIN: f32 = 110.0;

pub struct ViewerOpts {
    pub connect: ConnectOpts,
    /// Start with the side panel open on the files.
    pub open_files: bool,
    /// Open a terminal as soon as it connects.
    pub open_terminal: bool,
    pub swap_cmd_ctrl: bool,
    pub download_dir: PathBuf,
    /// Started by the app: read `show`, `files` or `terminal` lines on standard input (the
    /// app asks an open session window to come forward).
    pub control_stdin: bool,
    /// The account the computer was opened with ("server · e-mail"), in the title: it stays
    /// when the app changes to another server.
    pub account: Option<String>,
}

pub fn run(opts: ViewerOpts) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    if opts.control_stdin {
        let proxy = event_loop.create_proxy();
        std::thread::Builder::new().name("rdesk-control".into()).spawn(move || {
            use std::io::BufRead as _;
            // Ends when the app goes away (the pipe closes).
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Some(r) = Raise::parse(&line) {
                    let _ = proxy.send_event(UiEvent::Raise(r));
                }
            }
        })?;
    }
    let name = opts.connect.name.clone();
    let video = opts.connect.video;
    let mut connect = opts.connect;
    // The window plays the computer's sound.
    let player = Arc::new(crate::audio::Player::default());
    connect.audio = Some(player.sink());
    let handle = conn::spawn(connect, move |ev| {
        let _ = proxy.send_event(ev);
    });
    let mut app = App::new(handle, name, opts.swap_cmd_ctrl, opts.download_dir);
    app.player = player;
    app.video = video;
    app.terminal_on_connect = opts.open_terminal;
    app.account = opts.account;
    if opts.open_files {
        app.open_panel(Tab::Files);
        app.list(app.remote_path.clone());
    }
    event_loop.run_app(&mut app)?;
    let _ = app.handle.cmd.send(Cmd::Quit);
    Ok(())
}

struct Gfx {
    window: Arc<Window>,
    state: egui_winit::State,
    painter: Painter,
}

struct PasswordPrompt {
    host: String,
    error: Option<String>,
    can_remember: bool,
    reply: PasswordReply,
    text: zeroize::Zeroizing<String>,
    remember: bool,
    focused: bool,
}

struct Transfer {
    name: String,
    done: u64,
    total: u64,
    finished: Option<Result<String, String>>,
}

/// The connection dropped and is being retried.
struct Offline {
    since: Instant,
    attempt: u32,
    reason: String,
}

/// A question before something that cannot be undone.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Confirm {
    /// Close a terminal tab whose shell is still running.
    CloseShell(u64),
    /// Leave the session with a terminal or transfers going.
    Disconnect,
    /// Restart the remote computer (in safe mode with networking, or not).
    Restart { safe_mode: bool },
}

/// The side panel's tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Files,
    Terminal,
    Info,
}

/// The toolbar's buttons that open a menu or the side panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Monitor,
    Quality,
    Keys,
    /// Sound, the screen over there, restarting it…
    Session,
    Files,
    Terminal,
    /// "Show screen", in a window that opened without the picture.
    Screen,
}

impl Tool {
    fn icon(self) -> &'static str {
        match self {
            Tool::Screen => icons::MONITOR,
            Tool::Monitor => icons::MONITOR,
            Tool::Quality => icons::SLIDERS_HORIZONTAL,
            Tool::Keys => icons::KEYBOARD,
            Tool::Session => icons::DOTS_THREE,
            Tool::Files => icons::FOLDER,
            Tool::Terminal => icons::TERMINAL_WINDOW,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tool::Monitor => t!("Monitor", "Monitor"),
            Tool::Quality => t!("Quality", "Qualidade"),
            Tool::Keys => t!("Keys", "Teclas"),
            Tool::Session => t!("Session", "Sessão"),
            Tool::Files => t!("Files", "Arquivos"),
            Tool::Terminal => t!("Terminal", "Terminal"),
            Tool::Screen => t!("Show screen", "Mostrar tela"),
        }
    }
}

/// Picture settings in one click.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Preset {
    Auto,
    Speed,
    Quality,
}

impl Preset {
    const ALL: [Preset; 3] = [Preset::Auto, Preset::Speed, Preset::Quality];

    /// Frames per second and JPEG quality.
    fn values(self) -> (u8, u8) {
        match self {
            // The ceiling: the host lowers quality and size when the link cannot keep up.
            Preset::Auto => (60, 85),
            Preset::Speed => (60, 50),
            Preset::Quality => (30, 90),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Preset::Auto => t!("Auto", "Auto"),
            Preset::Speed => t!("Speed", "Velocidade"),
            Preset::Quality => t!("Quality", "Qualidade"),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Preset::Auto => t!(
                "Adapts to the connection: as sharp and smooth as it allows (up to 60 fps).",
                "Se adapta à conexão: tão nítida e fluida quanto ela permitir (até 60 qps)."
            ),
            Preset::Speed => t!(
                "Smoother movement, a softer picture: for slow networks and video (60 fps).",
                "Movimento mais fluido, imagem mais suave: para redes lentas e vídeo (60 qps)."
            ),
            Preset::Quality => t!(
                "The sharpest picture, for reading small text (30 fps).",
                "A imagem mais nítida, para ler letras pequenas (30 qps)."
            ),
        }
    }
}

/// One terminal tab.
struct Shell {
    /// Its number in this session ("Terminal 2").
    id: u64,
    view: TermView,
    admin: bool,
}

impl Shell {
    fn name(&self) -> String {
        let n = self.id;
        if self.admin {
            tf!("Terminal {n} (admin)", "Terminal {n} (admin)")
        } else {
            tf!("Terminal {n}", "Terminal {n}")
        }
    }

    fn live(&self) -> bool {
        !self.view.ended
    }
}

struct App {
    handle: Handle,
    name: String,
    /// "Server · e-mail" of the account it was opened with.
    account: Option<String>,
    egui: egui::Context,
    gfx: Option<Gfx>,
    tex: Option<TextureHandle>,
    status: Status,
    password: Option<PasswordPrompt>,
    fatal: Option<String>,
    host: Option<HostInfo>,
    monitors: Vec<MonitorInfo>,
    active: u32,
    stats: (u32, f32, f32, &'static str),
    /// What this session may do (from the host's answer).
    access: rdesk_proto::Access,
    /// The remote screen is wanted (a terminal opens without it until "Show screen").
    video: bool,
    /// Where the remote image is drawn, in egui points.
    image_rect: Option<egui::Rect>,
    pending_move: Option<(u16, u16)>,
    pointer_inside: bool,
    /// Mouse buttons pressed on the picture. A button's release goes where its press went,
    /// wherever the pointer is by then: a drag of the panel's edge that ended over the
    /// picture gave its release to the remote computer, and the edge stayed stuck to the
    /// mouse.
    remote_buttons: Vec<MouseButton>,
    swap_cmd_ctrl: bool,
    fps: u8,
    quality: u8,
    /// Automatic quality is on (the Auto preset), and what the host chose:
    /// (frames per second, JPEG quality, scale %).
    auto: bool,
    auto_choice: Option<(u8, u8, u8)>,
    /// The host's pointer images by id, the one in use (0: none) and the one set on the
    /// window, and the cursor egui asked for last (egui sets its own when that changes).
    cursors: HashMap<u64, winit::window::CustomCursor>,
    /// What the host's extensions stream offers.
    features: Vec<Feature>,
    /// Modifier keys held here (a phone gets typed characters as text, shortcuts as keys).
    mods: winit::keyboard::ModifiersState,
    /// The computer's sound is wanted, and where it plays.
    sound: bool,
    /// The black screen and the blocked keyboard over there (the host undoes both when the
    /// session ends, so a reconnect starts without them).
    privacy: bool,
    block_input: bool,
    /// Port tunnels open in this session, and the one being typed.
    tunnels: Vec<rdesk_core::tunnel::TunnelSpec>,
    tunnel_text: String,
    player: Arc<crate::audio::Player>,
    cursor_id: u64,
    cursor_shown: Option<u64>,
    egui_icon: egui::CursorIcon,
    remote_path: String,
    /// The path being typed (the path bar in edit mode).
    path_edit: Option<String>,
    listing: Option<(String, Result<Vec<DirEntry>, String>)>,
    /// A listing was asked for and has not come yet.
    listing_pending: bool,
    transfers: BTreeMap<u64, Transfer>,
    download_dir: PathBuf,
    next_repaint: Option<Instant>,
    /// The terminal tabs, the one shown, and how many are still opening.
    shells: Vec<Shell>,
    shell_active: u64,
    next_shell: u64,
    shells_opening: usize,
    terminal_on_connect: bool,
    /// Signed in to the host: keyboard and mouse go through. Off while connecting or
    /// reconnecting, so nothing typed meanwhile is replayed once it is back.
    connected: bool,
    /// The window has the keyboard.
    window_focused: bool,
    /// The lock keys here when last looked at ([`keys::locks`]), and when that was.
    locks_here: Option<keys::Locks>,
    locks_checked: Instant,
    offline: Option<Offline>,
    confirm: Option<Confirm>,
    /// Close the window on the next turn of the event loop.
    exit_requested: bool,
    /// The side panel is open (with a screen; without one it is the whole window).
    panel: bool,
    tab: Tab,
    fullscreen: bool,
    /// In full screen: the toolbar is showing (the mouse went to the top edge).
    bar_revealed: bool,
    toasts: ui::Toasts,
    /// The ui gallery: a menu to show open, and a sample picture to draw.
    #[cfg(test)]
    sample: Sample,
}

#[cfg(test)]
#[derive(Default)]
struct Sample {
    menu: Option<Tool>,
    picture: bool,
    scrollback: usize,
}

impl App {
    fn new(handle: Handle, name: String, swap_cmd_ctrl: bool, download_dir: PathBuf) -> Self {
        let egui = egui::Context::default();
        ui::install(&egui);
        let (fps, quality) = Preset::Auto.values();
        Self {
            handle,
            name,
            account: None,
            egui,
            gfx: None,
            tex: None,
            status: Status::Connecting,
            password: None,
            fatal: None,
            host: None,
            monitors: Vec::new(),
            active: 0,
            stats: (0, 0.0, 0.0, ""),
            access: rdesk_proto::Access::FULL,
            video: true,
            image_rect: None,
            pending_move: None,
            pointer_inside: false,
            remote_buttons: Vec::new(),
            swap_cmd_ctrl,
            fps,
            quality,
            auto: true,
            auto_choice: None,
            cursors: HashMap::new(),
            features: Vec::new(),
            mods: Default::default(),
            sound: true,
            privacy: false,
            block_input: false,
            tunnels: Vec::new(),
            tunnel_text: String::new(),
            player: Arc::new(crate::audio::Player::default()),
            cursor_id: 0,
            cursor_shown: None,
            egui_icon: egui::CursorIcon::Default,
            remote_path: "~".into(),
            path_edit: None,
            listing: None,
            listing_pending: false,
            transfers: BTreeMap::new(),
            download_dir,
            next_repaint: None,
            shells: Vec::new(),
            shell_active: 0,
            next_shell: 1,
            shells_opening: 0,
            terminal_on_connect: false,
            connected: false,
            window_focused: true,
            locks_here: None,
            locks_checked: Instant::now(),
            offline: None,
            confirm: None,
            exit_requested: false,
            panel: false,
            tab: Tab::Files,
            fullscreen: false,
            bar_revealed: false,
            toasts: ui::Toasts::default(),
            #[cfg(test)]
            sample: Sample::default(),
        }
    }

    fn send(&self, c: Cmd) {
        let _ = self.handle.cmd.send(c);
    }

    fn input(&self, ev: InputEvent) {
        if self.access.input && self.connected {
            self.send(Cmd::Input(ev));
        }
    }

    /// Press `keys` in order, then let go in reverse (Ctrl+Alt+Del, Alt+Tab…).
    fn combo(&self, keys: &[rdesk_proto::Key]) {
        for &k in keys {
            self.input(InputEvent::Key { key: k, down: true });
        }
        for &k in keys.iter().rev() {
            self.input(InputEvent::Key { key: k, down: false });
        }
    }

    /// Caps Lock and Num Lock there follow the ones here. A host that knows its own gets the
    /// state ([`ExtClient::Locks`]): at the start (`force`), when the window gets the keyboard
    /// back (`force`) and when it changes. An older one gets a Caps Lock press when it changes
    /// here, from a Mac (whose Caps Lock never reaches the window as a key; elsewhere the keys
    /// go as they are).
    fn sync_locks(&mut self, force: bool) {
        self.locks_checked = Instant::now();
        let Some(now) = keys::locks() else { return };
        let before = self.locks_here.replace(now);
        if !self.access.input || !self.connected || !self.screen() {
            return;
        }
        if self.features.contains(&Feature::LockKeys) {
            if force || before.is_some_and(|b| b != now) {
                self.send(Cmd::Ext(rdesk_proto::ExtClient::Locks { caps: now.caps, num: now.num }));
            }
        } else if cfg!(target_os = "macos") && !force && before.is_some_and(|b| b.caps != now.caps) {
            self.combo(&[rdesk_proto::Key::CapsLock]);
        }
    }

    /// This lock key goes as its state ([`Self::sync_locks`]), not as a key.
    fn lock_by_state(&self, key: Key) -> bool {
        let Some(here) = keys::locks() else { return false };
        self.features.contains(&Feature::LockKeys)
            && match key {
                Key::CapsLock => true,
                Key::NumLock => here.num.is_some(),
                _ => false,
            }
    }

    fn host_name(&self) -> String {
        self.host.as_ref().map(|h| h.hostname.clone()).unwrap_or_else(|| self.name.clone())
    }

    /// The window shows the remote screen: the session may see it and it was wanted.
    /// Otherwise the side panel is the whole window.
    fn screen(&self) -> bool {
        self.access.screen && self.video
    }

    /// Start the picture in a window that opened without it (it stays on from then on).
    fn show_screen(&mut self) {
        if self.access.screen && !self.video {
            self.video = true;
            self.send(Cmd::ShowScreen);
        }
    }

    fn host_is_windows(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.os == "windows")
    }

    /// A terminal with administrator rights exists on Windows hosts, with full control.
    fn admin_terminal_possible(&self) -> bool {
        self.access == rdesk_proto::Access::FULL && self.host_is_windows()
    }

    fn live_shells(&self) -> usize {
        self.shells.iter().filter(|s| s.live()).count()
    }

    fn transfers_running(&self) -> usize {
        self.transfers.values().filter(|t| t.finished.is_none()).count()
    }

    /// The offline card is up (a drop that lasted past the grace period).
    fn offline_shown(&self) -> bool {
        self.offline.as_ref().is_some_and(|o| o.since.elapsed() >= OFFLINE_GRACE)
    }

    /// Path separator on the host.
    fn sep(&self) -> char {
        if self.host_is_windows() { '\\' } else { '/' }
    }

    /// Try again now (while waiting between attempts, or after the session ended).
    fn reconnect_now(&mut self) {
        if self.fatal.take().is_some() {
            self.status = Status::Connecting;
            self.offline = None;
            self.update_title();
        }
        self.send(Cmd::Reconnect);
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn update_title(&self) {
        if let Some(g) = &self.gfx {
            let host = self.host.as_ref().map(|h| h.hostname.as_str()).unwrap_or(&self.name);
            match &self.account {
                Some(a) => g.window.set_title(&format!("rdesk — {host} ({a}) — {}", self.status)),
                None => g.window.set_title(&format!("rdesk — {host} — {}", self.status)),
            }
        }
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.bar_revealed = false;
        if let Some(g) = &self.gfx {
            g.window.set_fullscreen(if self.fullscreen { Some(Fullscreen::Borderless(None)) } else { None });
        }
    }

    // -----------------------------------------------------------------------------------
    // Side panel, files and terminals
    // -----------------------------------------------------------------------------------

    /// The tabs this session has.
    fn tabs(&self) -> Vec<(Tab, &'static str)> {
        let mut v = Vec::new();
        if self.access.files {
            v.push((Tab::Files, t!("Files", "Arquivos")));
        }
        if self.access.terminal {
            v.push((Tab::Terminal, t!("Terminal", "Terminal")));
        }
        v.push((Tab::Info, t!("Info", "Info")));
        v
    }

    fn open_panel(&mut self, tab: Tab) {
        self.panel = true;
        self.tab = tab;
        if tab == Tab::Files && self.listing.is_none() && !self.listing_pending && self.connected {
            self.list(self.remote_path.clone());
        }
    }

    /// The toolbar's Files and Terminal buttons: open the panel there, or close it.
    fn toggle_panel(&mut self, tab: Tab) {
        if self.screen() && self.panel && self.tab == tab {
            self.panel = false;
        } else {
            self.open_panel(tab);
        }
    }

    fn panel_shows(&self, tab: Tab) -> bool {
        (self.panel || !self.screen()) && self.tab == tab
    }

    fn list(&mut self, path: String) {
        self.listing_pending = true;
        self.path_edit = None;
        self.send(Cmd::ListDir(path));
    }

    /// Open a new shell in a new tab (`admin`: with administrator rights).
    fn start_terminal(&mut self, admin: bool) {
        self.shells_opening += 1;
        self.send(Cmd::OpenTerminal { cols: 100, rows: 30, admin });
        self.open_panel(Tab::Terminal);
    }

    /// The app's "Terminal" for a session already open: show a running shell, or start one.
    fn show_terminal(&mut self) {
        self.open_panel(Tab::Terminal);
        if let Some(s) =
            self.shells.iter().find(|s| s.live() && !s.admin).or_else(|| self.shells.iter().find(|s| s.live()))
        {
            self.shell_active = s.id;
        } else if self.shells_opening == 0 {
            self.start_terminal(false);
        }
    }

    fn close_shell(&mut self, id: u64) {
        if self.shells.iter().any(|s| s.id == id && s.live()) {
            self.confirm = Some(Confirm::CloseShell(id));
        } else {
            self.remove_shell(id);
        }
    }

    /// Drop a tab: its stream closes, which ends the shell on the host.
    fn remove_shell(&mut self, id: u64) {
        let Some(i) = self.shells.iter().position(|s| s.id == id) else { return };
        self.shells.remove(i);
        if self.shell_active == id {
            self.shell_active = self.shells.get(i.min(self.shells.len().saturating_sub(1))).map_or(0, |s| s.id);
        }
    }

    /// Where the keyboard goes, when a terminal could take it: `Some(true)` the terminal,
    /// `Some(false)` the remote screen.
    fn keyboard_target(&self) -> Option<bool> {
        if !(self.screen() && self.access.input && self.panel && self.tab == Tab::Terminal) {
            return None;
        }
        self.shells.iter().find(|s| s.id == self.shell_active && s.live()).map(|s| s.view.focused)
    }

    /// The window's close button and Disconnect: ask first when leaving would stop
    /// something.
    fn close_requested(&mut self) {
        if self.connected && (self.live_shells() > 0 || self.transfers_running() > 0) {
            self.confirm = Some(Confirm::Disconnect);
            if let Some(g) = &self.gfx {
                g.window.focus_window();
            }
            self.request_redraw();
        } else {
            self.exit_requested = true;
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
        let icon = out.platform_output.cursor_icon;
        gfx.state.handle_platform_output(&gfx.window, out.platform_output);
        self.show_remote_cursor(&gfx.window, icon);
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

    /// Over the picture, the pointer looks like the host's (an I-beam over text, a hand on
    /// links), drawn by this computer so it moves without delay. `icon` is what egui asked for
    /// this frame: it sets that itself whenever it changes, replacing ours.
    fn show_remote_cursor(&mut self, window: &Window, icon: egui::CursorIcon) {
        if icon != self.egui_icon {
            self.egui_icon = icon;
            self.cursor_shown = None;
        }
        let over_picture = self.pointer_inside && icon == egui::CursorIcon::Default && self.screen();
        let want = over_picture.then(|| self.cursors.get(&self.cursor_id).map(|c| (self.cursor_id, c))).flatten();
        match want {
            Some((id, c)) if self.cursor_shown != Some(id) => {
                window.set_cursor(c.clone());
                self.cursor_shown = Some(id);
            }
            None if self.cursor_shown.is_some() => {
                window.set_cursor(winit::window::CursorIcon::Default);
                self.cursor_shown = None;
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------------------

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let p = ui::pal(ui);
        let window = ui.max_rect();
        if !self.tabs().iter().any(|(t, _)| *t == self.tab) {
            self.tab = self.tabs()[0].0;
        }

        // The toolbar: a strip at the top; in full screen it hides until the mouse
        // touches the top edge, and then lies over the picture.
        let bar_frame = egui::Frame::new()
            .fill(p.bg_alt)
            .inner_margin(Margin::symmetric(space::S as i8, 6))
            .stroke(Stroke::new(1.0, p.border));
        if self.fullscreen {
            if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                if pos.y <= window.top() + REVEAL_EDGE {
                    self.bar_revealed = true;
                } else if pos.y > window.top() + CONTROL_H + 24.0 && !egui::Popup::is_any_open(&ctx) {
                    self.bar_revealed = false;
                }
            }
            if self.bar_revealed {
                egui::Area::new(egui::Id::unique("session-toolbar-overlay"))
                    .fixed_pos(window.min)
                    .order(egui::Order::Middle)
                    .show(&ctx, |ui| {
                        bar_frame.show(ui, |ui| {
                            ui.set_width(window.width() - 2.0 * space::S);
                            self.toolbar(ui);
                        });
                    });
            }
        } else {
            egui::Panel::top("session-toolbar")
                .resizable(false)
                .show_separator_line(false)
                .frame(bar_frame)
                .show(ui, |ui| self.toolbar(ui));
        }

        let panel_frame = egui::Frame::new().fill(p.bg_alt).inner_margin(Margin::same(space::M as i8));
        if self.screen() {
            if self.panel {
                egui::Panel::right("session-panel")
                    .resizable(true)
                    .default_size(PANEL_DEFAULT)
                    .size_range(PANEL_MIN..=(window.width() - PICTURE_MIN).max(PANEL_MIN))
                    .frame(panel_frame)
                    .show(ui, |ui| self.side_panel(ui, true));
            }
            // What is left is the picture's (and egui leaves pointer events there alone,
            // so they go to the remote computer).
            let area = ui.available_rect_before_wrap();
            ui.painter().rect_filled(area, 0.0, p.bg);
            self.draw_picture(ui, area);
            self.connection_ui(&ctx, area);
        } else {
            self.image_rect = None;
            let area = ui.available_rect_before_wrap();
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::same(space::M as i8)))
                .show(ui, |ui| self.side_panel(ui, false));
            self.connection_ui(&ctx, area);
        }

        self.drop_hint(&ctx);
        self.password_dialog(&ctx);
        self.confirm_dialog(&ctx);
        self.toasts.show(&ctx);
    }

    /// The remote screen, aspect-fit in `area` (dimmed while the connection is down).
    fn draw_picture(&mut self, ui: &egui::Ui, area: egui::Rect) {
        self.image_rect = None;
        let Some(t) = &self.tex else { return };
        let [tw, th] = t.size();
        let scale = (area.width() / tw as f32).min(area.height() / th as f32);
        if scale <= 0.0 {
            return;
        }
        let rect = egui::Rect::from_center_size(area.center(), egui::vec2(tw as f32 * scale, th as f32 * scale));
        let painter = ui.painter();
        painter.image(
            t.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        // The picture is stale while the connection is down: dim it.
        if self.offline_shown() || self.fatal.is_some() {
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
        }
        self.image_rect = Some(rect);
    }

    /// The word next to the host name: latency when connected, else where it stands.
    fn status_badge(&self) -> (String, Tone, String) {
        if self.fatal.is_some() {
            (t!("Disconnected", "Desconectado").into(), Tone::Danger, self.status.to_string())
        } else if self.offline.is_some() {
            (t!("Reconnecting…", "Reconectando…").into(), Tone::Warning, self.status.to_string())
        } else if !self.connected {
            (t!("Connecting…", "Conectando…").into(), Tone::Warning, self.status.to_string())
        } else if self.stats.0 > 0 {
            let rtt = self.stats.0;
            let tone = match rtt {
                0..80 => Tone::Success,
                80..200 => Tone::Warning,
                _ => Tone::Danger,
            };
            let route = route_label(self.stats.3);
            let hover = if route.is_empty() {
                tf!("Round trip: {rtt} ms", "Ida e volta: {rtt} ms")
            } else {
                tf!("Round trip: {rtt} ms · {route} route", "Ida e volta: {rtt} ms · rota {route}")
            };
            (format!("{rtt} ms"), tone, hover)
        } else {
            (t!("Connected", "Conectado").into(), Tone::Success, self.status.to_string())
        }
    }

    /// The toolbar buttons this session has.
    fn tools(&self) -> Vec<Tool> {
        let mut v = Vec::new();
        if self.screen() {
            v.push(Tool::Monitor);
            v.push(Tool::Quality);
            if self.access.input {
                v.push(Tool::Keys);
            }
            v.push(Tool::Session);
        } else if self.access.screen {
            v.push(Tool::Screen);
        }
        // Without a screen the panel is the window, and its tabs do this.
        if self.screen() && self.access.files {
            v.push(Tool::Files);
        }
        if self.screen() && self.access.terminal {
            v.push(Tool::Terminal);
        }
        v
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let pad = ui.spacing().button_padding.x;
        let gap = ui.spacing().item_spacing.x;
        let tools = self.tools();
        let kb = self.keyboard_target();
        let (badge, tone, badge_hover) = self.status_badge();
        let disconnect = t!("Disconnect", "Desconectar");

        // Measure first: labels collapse to icons (named on hover) when they do not fit,
        // the tools first, then the keyboard chip and Disconnect.
        let w = |text: &str, size: f32| text_width(ui, text, size) + 2.0 * pad;
        let icon_w = (w(icons::GEAR, size::ICON + 1.0)).max(CONTROL_H) + gap;
        let labeled = |t: &Tool| w(&ui::with_icon(t.icon(), t.label()), size::BODY) + gap;
        let toggles = |t: &&Tool| matches!(t, Tool::Files | Tool::Terminal);
        let menus_full: f32 = tools.iter().filter(|t| !toggles(t)).map(labeled).sum();
        let toggles_full: f32 = tools.iter().filter(toggles).map(labeled).sum();
        let menus_compact = icon_w * tools.iter().filter(|t| !toggles(t)).count() as f32;
        let toggles_compact = icon_w * tools.iter().filter(toggles).count() as f32;
        let chip_full =
            kb.map_or(0.0, |k| w(&ui::with_icon(icons::KEYBOARD, keyboard_label(k)), size::SMALL + 0.5) + gap);
        let chip_compact = kb.map_or(0.0, |_| icon_w);
        // Bold text is a little wider.
        let disc_full = w(&ui::with_icon(icons::SIGN_OUT, disconnect), size::BODY) * 1.06 + gap;
        let nav = self.features.contains(&Feature::Nav);
        let nav_w = if nav { 3.0 * icon_w } else { 0.0 };
        let right_full = chip_full + icon_w + disc_full + nav_w;
        let right_compact = chip_compact + icon_w + icon_w + nav_w;
        let left_fixed = 10.0 + gap + text_width(ui, &badge, size::SMALL) + 16.0 + gap + space::S;
        let avail = ui.available_width();
        // From roomiest to tightest: (menus as icons, toggles as icons, right side as icons).
        let fits = |menus: f32, toggles: f32, right: f32| left_fixed + NAME_MIN + menus + toggles + right <= avail;
        let (compact_menus, compact_toggles, compact_right) = if fits(menus_full, toggles_full, right_full) {
            (false, false, false)
        } else if fits(menus_compact, toggles_full, right_full) {
            (true, false, false)
        } else if fits(menus_compact, toggles_compact, right_full) {
            (true, true, false)
        } else {
            (true, true, true)
        };
        let tools_w = if compact_menus { menus_compact } else { menus_full }
            + if compact_toggles { toggles_compact } else { toggles_full };
        let right_w = if compact_right { right_compact } else { right_full };
        let name_w = (avail - left_fixed - tools_w - right_w - space::S).max(40.0);

        ui.horizontal(|ui| {
            ui::dot(ui, tone.color(&p)).on_hover_text(&badge_hover);
            let host = self.host_name();
            ui.allocate_ui(Vec2::new(name_w, CONTROL_H), |ui| {
                ui::truncate(ui, RichText::new(host).strong());
            });
            pill(ui, &badge, tone).on_hover_text(&badge_hover);
            ui.add_space(space::S);
            for tool in &tools {
                let compact = if matches!(tool, Tool::Files | Tool::Terminal) { compact_toggles } else { compact_menus };
                self.tool_button(ui, *tool, compact);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let r = if compact_right {
                    ui::danger(ui, icons::SIGN_OUT).on_hover_text(disconnect)
                } else {
                    ui::danger(ui, ui::with_icon(icons::SIGN_OUT, disconnect))
                };
                if r.clicked() {
                    self.close_requested();
                }
                let (icon, hover) = if self.fullscreen {
                    (icons::CORNERS_IN, t!("Leave full screen", "Sair da tela cheia"))
                } else {
                    (icons::CORNERS_OUT, t!("Full screen", "Tela cheia"))
                };
                if ui::icon_button(ui, icon, hover).clicked() {
                    self.toggle_fullscreen();
                }
                if nav {
                    // Right to left: Back, Home, Recents read left to right.
                    for (icon, hover, action) in [
                        (icons::SQUARES_FOUR, t!("Recent apps", "Apps recentes"), NavAction::Recents),
                        (icons::HOUSE, t!("Home", "Início"), NavAction::Home),
                        (icons::CARET_LEFT, t!("Back", "Voltar"), NavAction::Back),
                    ] {
                        if ui::icon_button(ui, icon, hover).clicked() {
                            self.send(Cmd::Ext(rdesk_proto::ExtClient::Nav(action)));
                        }
                    }
                }
                if let Some(to_terminal) = kb {
                    let label = keyboard_label(to_terminal);
                    let hover = if to_terminal {
                        t!(
                            "Keys go to the terminal. Click to send them to the remote screen (or click the screen).",
                            "As teclas vão para o terminal. Clique para mandá-las à tela remota (ou clique na tela)."
                        )
                    } else {
                        t!(
                            "Keys go to the remote screen. Click to send them to the terminal (or click the terminal).",
                            "As teclas vão para a tela remota. Clique para mandá-las ao terminal (ou clique no terminal)."
                        )
                    };
                    let text = if compact_right { icons::KEYBOARD.to_string() } else { ui::with_icon(icons::KEYBOARD, label) };
                    let r = ui::chip(ui, to_terminal, &text);
                    let r = if compact_right { r.on_hover_text(format!("{label}\n\n{hover}")) } else { r.on_hover_text(hover) };
                    if r.clicked() {
                        if to_terminal {
                            ui.memory_mut(|m| {
                                if let Some(id) = m.focused() {
                                    m.surrender_focus(id);
                                }
                            });
                        } else if let Some(s) = self.shells.iter_mut().find(|s| s.id == self.shell_active) {
                            s.view.request_focus();
                        }
                    }
                }
            });
        });
    }

    fn tool_button(&mut self, ui: &mut egui::Ui, tool: Tool, compact: bool) {
        let text = if compact {
            RichText::new(tool.icon()).size(size::ICON + 1.0)
        } else {
            RichText::new(ui::with_icon(tool.icon(), tool.label()))
        };
        let min = Vec2::new(CONTROL_H, CONTROL_H);
        if tool == Tool::Screen {
            let hover = t!(
                "See and control that computer's screen. Until then it is not even captured.",
                "Ver e controlar a tela desse computador. Até lá ela nem é capturada."
            );
            let r = ui.add(egui::Button::new(text).min_size(min));
            let r =
                if compact { r.on_hover_text(format!("{}\n{hover}", tool.label())) } else { r.on_hover_text(hover) };
            if r.clicked() {
                self.show_screen();
            }
            return;
        }
        let tab = match tool {
            Tool::Files => Some(Tab::Files),
            Tool::Terminal => Some(Tab::Terminal),
            _ => None,
        };
        if let Some(tab) = tab {
            let on = self.panel_shows(tab);
            let r = ui.add(egui::Button::selectable(on, text).min_size(min));
            let hover = match tab {
                Tab::Files => t!("Browse, send and download files", "Navegar, enviar e baixar arquivos"),
                _ => t!(
                    "A shell on that computer, as whoever is signed in there",
                    "Um shell nesse computador, como quem estiver conectado nele"
                ),
            };
            let r =
                if compact { r.on_hover_text(format!("{}\n{hover}", tool.label())) } else { r.on_hover_text(hover) };
            if r.clicked() {
                self.toggle_panel(tab);
            }
            return;
        }
        let button = egui::Button::new(text).frame_when_inactive(false).min_size(min);
        let config = MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        let (resp, _) = MenuButton::from_button(button).config(config).ui(ui, |ui| match tool {
            Tool::Monitor => self.monitor_menu(ui),
            Tool::Quality => self.quality_menu(ui),
            Tool::Session => self.session_menu(ui),
            _ => self.keys_menu(ui),
        });
        if compact {
            resp.clone().on_hover_text(tool.label());
        }
        #[cfg(test)]
        if self.sample.menu == Some(tool) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&resp));
        }
    }

    fn monitor_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(280.0);
        if self.monitors.is_empty() {
            ui::caption(ui, t!("No monitor information yet.", "Ainda sem informações dos monitores."));
        }
        for m in self.monitors.clone() {
            let mut text = format!("{}  ·  {}×{}", m.name, m.width, m.height);
            if m.primary {
                text.push_str(t!("  ·  main", "  ·  principal"));
            }
            let on = m.id == self.active;
            let r = ui.add(
                egui::Button::selectable(on, ui::with_icon(icons::MONITOR, &text)).min_size(Vec2::new(0.0, CONTROL_H)),
            );
            if r.clicked() {
                if !on {
                    self.send(Cmd::SelectMonitor(m.id));
                }
                ui.close();
            }
        }
        ui.separator();
        let refresh =
            egui::Button::new(ui::with_icon(icons::ARROW_CLOCKWISE, t!("Refresh picture", "Atualizar imagem")))
                .frame_when_inactive(false)
                .min_size(Vec2::new(0.0, CONTROL_H));
        if ui.add(refresh).on_hover_text(t!("Ask for the whole screen again", "Pede a tela inteira de novo")).clicked()
        {
            self.send(Cmd::FullFrame);
            ui.close();
        }
    }

    fn quality_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(300.0);
        ui.set_max_width(340.0);
        let current = if self.auto {
            Some(Preset::Auto)
        } else {
            [Preset::Speed, Preset::Quality].into_iter().find(|p| p.values() == (self.fps, self.quality))
        };
        ui.spacing_mut().slider_width = 200.0;
        ui.horizontal_wrapped(|ui| {
            for preset in Preset::ALL {
                let r = ui::chip(ui, current == Some(preset), preset.label());
                if r.on_hover_text(preset.hint()).clicked() {
                    (self.fps, self.quality) = preset.values();
                    self.set_auto(preset == Preset::Auto);
                    self.apply_quality();
                }
            }
        });
        match current {
            Some(p) => ui::caption(ui, p.hint()),
            None => ui::caption(ui, t!("Your own settings.", "Ajuste próprio.")),
        };
        if self.auto
            && let Some((_, q, scale)) = self.auto_choice
        {
            ui::caption(
                ui,
                &if scale < 100 {
                    tf!("Now: quality {q}, picture at {scale}%.", "Agora: qualidade {q}, imagem a {scale}%.")
                } else {
                    tf!("Now: quality {q}, full size.", "Agora: qualidade {q}, tamanho inteiro.")
                },
            );
        }
        ui.separator();
        ui.label(t!("Frames per second", "Quadros por segundo"));
        let r = ui.add(egui::Slider::new(&mut self.fps, 5..=60));
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            self.set_auto(false);
            self.apply_quality();
        }
        ui.label(t!("Picture quality", "Qualidade da imagem"));
        let r = ui.add(egui::Slider::new(&mut self.quality, 30..=95));
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            self.set_auto(false);
            self.apply_quality();
        }
        ui::caption(ui, t!("Changes apply right away.", "As mudanças valem na hora."));
    }

    fn apply_quality(&self) {
        self.send(Cmd::SetQuality { max_fps: self.fps, jpeg_quality: self.quality });
    }

    /// What the session can do besides the picture: sound, and what the host offers.
    fn session_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(300.0);
        ui.set_max_width(360.0);
        let has = |f: Feature| self.features.contains(&f);
        if has(Feature::Audio) {
            let mut on = self.sound;
            let label = ui::with_icon(
                if on { icons::SPEAKER_HIGH } else { icons::SPEAKER_SLASH },
                t!("Hear the computer", "Ouvir o computador"),
            );
            if ui.checkbox(&mut on, label).changed() {
                self.sound = on;
                self.send(Cmd::Audio(on));
            }
        } else {
            ui::caption(
                ui,
                t!(
                    "This computer does not send its sound (only Windows computers with rdesk 0.7 or newer do).",
                    "Este computador não envia o som (só computadores Windows com rdesk 0.7 ou mais novo)."
                ),
            );
        }
        self.session_actions(ui);
    }

    /// What the host offers with full control: the black screen at the computer, blocking its
    /// keyboard and mouse, restarting it.
    fn session_actions(&mut self, ui: &mut egui::Ui) {
        let features = self.features.clone();
        let has = |f: Feature| features.contains(&f);
        if has(Feature::Nav) {
            ui.separator();
            for (icon, label, action) in [
                (icons::BELL, t!("Notifications", "Notificações"), NavAction::Notifications),
                (icons::LIST, t!("Quick settings", "Ajustes rápidos"), NavAction::QuickSettings),
                (icons::IMAGE, t!("Screenshot on the phone", "Captura de tela no celular"), NavAction::Screenshot),
                (icons::LOCK, t!("Lock the phone", "Bloquear o celular"), NavAction::LockScreen),
            ] {
                if ui.button(ui::with_icon(icon, label)).clicked() {
                    self.send(Cmd::Ext(rdesk_proto::ExtClient::Nav(action)));
                    ui.close();
                }
            }
        }
        if !(has(Feature::Privacy) || has(Feature::BlockInput) || has(Feature::Power)) {
            return;
        }
        ui.separator();
        if has(Feature::Privacy) {
            let mut on = self.privacy;
            let label = ui::with_icon(icons::EYE_SLASH, t!("Black screen over there", "Tela preta lá"));
            let r = ui.checkbox(&mut on, label).on_hover_text(t!(
                "Whoever is at that computer sees a black screen; you keep seeing it as it is.",
                "Quem estiver nesse computador vê uma tela preta; você continua vendo a tela como ela é."
            ));
            if r.changed() {
                self.privacy = on;
                self.send(Cmd::Ext(rdesk_proto::ExtClient::Privacy { on }));
            }
        }
        if has(Feature::BlockInput) {
            let mut on = self.block_input;
            let label = ui::with_icon(
                icons::PROHIBIT,
                t!("Block its keyboard and mouse", "Bloquear o teclado e o mouse de lá"),
            );
            let r = ui.checkbox(&mut on, label).on_hover_text(t!(
                "Only you control it until the session ends (Ctrl+Alt+Del at that computer gives them back).",
                "Só você controla até a sessão terminar (Ctrl+Alt+Del nesse computador os devolve)."
            ));
            if r.changed() {
                self.block_input = on;
                self.send(Cmd::Ext(rdesk_proto::ExtClient::BlockInput { on }));
            }
        }
        if has(Feature::Tunnel) {
            ui.separator();
            ui.label(
                RichText::new(ui::with_icon(icons::ARROWS_LEFT_RIGHT, t!("Port tunnel", "Túnel de porta"))).strong(),
            );
            ui::caption(
                ui,
                t!(
                    "A port on this computer that leads to one reachable from there (its remote desktop, SSH, a \
                     router's page): LOCAL:HOST:PORT, like 13389:localhost:3389.",
                    "Uma porta neste computador que leva a uma alcançável de lá (a área de trabalho remota dele, \
                     SSH, a página do roteador): LOCAL:HOST:PORTA, como 13389:localhost:3389."
                ),
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.tunnel_text)
                        .hint_text("13389:localhost:3389")
                        .desired_width(200.0),
                );
                if ui.button(t!("Open", "Abrir")).clicked() {
                    match rdesk_core::tunnel::TunnelSpec::parse(self.tunnel_text.trim()) {
                        Ok(spec) => {
                            self.toasts.push(
                                tf!(
                                    "Tunnel: localhost:{} leads to {}:{} there",
                                    "Túnel: localhost:{} leva a {}:{} de lá",
                                    spec.local,
                                    spec.host,
                                    spec.port
                                ),
                                false,
                            );
                            self.tunnels.push(spec.clone());
                            self.send(Cmd::Tunnel(spec));
                            self.tunnel_text.clear();
                        }
                        Err(e) => self.toasts.push(tf!("Tunnel: {e:#}", "Túnel: {e:#}"), true),
                    }
                }
            });
            for t in &self.tunnels {
                ui::caption(ui, &format!("localhost:{} → {}:{}", t.local, t.host, t.port));
            }
        }
        if has(Feature::Power) {
            ui.separator();
            if ui
                .button(ui::with_icon(icons::ARROW_CLOCKWISE, t!("Restart the computer…", "Reiniciar o computador…")))
                .clicked()
            {
                self.confirm = Some(Confirm::Restart { safe_mode: false });
                ui.close();
            }
            if ui
                .button(ui::with_icon(icons::WRENCH, t!("Restart in safe mode…", "Reiniciar em modo de segurança…")))
                .clicked()
            {
                self.confirm = Some(Confirm::Restart { safe_mode: true });
                ui.close();
            }
        }
    }

    fn set_auto(&mut self, on: bool) {
        if self.auto != on {
            self.auto = on;
            self.auto_choice = None;
            self.send(Cmd::AutoQuality(on));
        }
    }

    fn keys_menu(&mut self, ui: &mut egui::Ui) {
        use rdesk_proto::Key::*;
        ui.set_min_width(300.0);
        ui.set_max_width(340.0);
        let item = |ui: &mut egui::Ui, text: &str| {
            ui.add(egui::Button::new(text).frame_when_inactive(false).min_size(Vec2::new(0.0, CONTROL_H)))
        };
        if self.host_is_windows()
            && item(ui, "Ctrl+Alt+Del")
                .on_hover_text(t!(
                    "Works when the computer runs rdesk as a service (installed)",
                    "Funciona quando o computador roda o rdesk como serviço (instalado)"
                ))
                .clicked()
        {
            self.combo(&[ControlLeft, AltLeft, Delete]);
            ui.close();
        }
        if item(ui, "Alt+Tab").clicked() {
            self.combo(&[AltLeft, Tab]);
            ui.close();
        }
        if self.host_is_windows() && item(ui, t!("Windows key", "Tecla Windows")).clicked() {
            self.combo(&[MetaLeft]);
            ui.close();
        }
        // A Mac's Caps Lock never reaches the window as a key; this turns it on or off there.
        if item(ui, "Caps Lock")
            .on_hover_text(t!(
                "Turns Caps Lock on or off on that computer",
                "Liga ou desliga o Caps Lock nesse computador"
            ))
            .clicked()
        {
            self.combo(&[CapsLock]);
            ui.close();
        }
        // A Mac laptop has no Num Lock (on a Mac keypad, Clear is it).
        if item(ui, "Num Lock")
            .on_hover_text(t!(
                "Turns Num Lock on or off on that computer (on: the keypad types digits)",
                "Liga ou desliga o Num Lock nesse computador (ligado: o teclado numérico digita números)"
            ))
            .clicked()
        {
            self.combo(&[NumLock]);
            ui.close();
        }
        ui.separator();
        ui::row(
            ui,
            |ui| {
                ui.label(t!("Mac keys", "Teclas de Mac"));
            },
            |ui| {
                ui::toggle(ui, &mut self.swap_cmd_ctrl);
            },
        );
        let a = icons::ARROW_RIGHT;
        ui::caption(
            ui,
            &tf!(
                "Cmd {a} Ctrl, left Option {a} Windows key, Control {a} Alt",
                "Cmd {a} Ctrl, Option esquerdo {a} tecla Windows, Control {a} Alt"
            ),
        );
    }

    /// The tabs and the open one. `closable`: a side panel (with a picture next to it).
    fn side_panel(&mut self, ui: &mut egui::Ui, closable: bool) {
        let tabs = self.tabs();
        let mut tab = self.tab;
        ui::row(
            ui,
            |ui| {
                ui.vertical(|ui| ui::tabs(ui, &mut tab, &tabs));
            },
            |ui| {
                if closable && ui::icon_button(ui, icons::X, t!("Close the panel", "Fechar o painel")).clicked() {
                    self.panel = false;
                }
            },
        );
        if tab != self.tab {
            self.open_panel(tab);
        }
        ui.add_space(space::S);
        match self.tab {
            Tab::Files => self.files_tab(ui),
            Tab::Terminal => self.terminal_tab(ui),
            Tab::Info => self.info_tab(ui),
        }
    }

    // -- Files ---------------------------------------------------------------------------

    fn files_tab(&mut self, ui: &mut egui::Ui) {
        self.path_bar(ui);
        ui.add_space(space::XS);
        egui::Panel::bottom("session-files-bottom")
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(Margin { top: space::S as i8, ..Margin::ZERO }))
            .show(ui, |ui| {
                let max = (ui.ctx().content_rect().height() * 0.18).clamp(64.0, 160.0);
                self.files_bottom(ui, max)
            });
        self.listing_ui(ui);
    }

    /// Up, refresh, and the path as clickable folders (click the bar to type a path).
    fn path_bar(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let sep = self.sep();
        if let Some(edit) = &mut self.path_edit {
            let (mut go_button, mut cancel_button) = (false, false);
            let (enter, escape) = ui::row(
                ui,
                |ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(edit)
                            .desired_width(ui.available_width())
                            .margin(Margin::symmetric(8, 7))
                            .hint_text(t!("A folder on that computer", "Uma pasta nesse computador")),
                    );
                    if !r.has_focus() && !r.lost_focus() {
                        r.request_focus();
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    (enter, ui.input(|i| i.key_pressed(egui::Key::Escape)))
                },
                |ui| {
                    cancel_button = ui::icon_button(ui, icons::X, t!("Cancel", "Cancelar")).clicked();
                    go_button =
                        ui::icon_button(ui, icons::ARROW_RIGHT, t!("Open this folder", "Abrir esta pasta")).clicked();
                },
            );
            let cancel = cancel_button || escape;
            if enter || go_button {
                let path = edit.trim().to_string();
                if !path.is_empty() {
                    self.list(path);
                }
                self.path_edit = None;
            } else if cancel {
                self.path_edit = None;
            }
            return;
        }

        let path = self.listing.as_ref().map(|(p, _)| p.clone()).unwrap_or_else(|| self.remote_path.clone());
        let mut nav = None;
        let mut edit = false;
        ui.horizontal(|ui| {
            if ui::icon_button(ui, icons::ARROW_UP, t!("Up one folder", "Uma pasta acima")).clicked() {
                nav = Some(parent_of(&path, sep));
            }
            if ui::icon_button(ui, icons::ARROW_CLOCKWISE, t!("Refresh", "Atualizar")).clicked() {
                nav = Some(path.clone());
            }
            // The path: a field-looking strip; a click on it (not on a folder) types a path.
            let h = CONTROL_H;
            let width = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, h), Sense::click());
            let resp = resp.on_hover_text(t!("Click to type a path", "Clique para digitar um caminho"));
            let stroke =
                if resp.hovered() { Stroke::new(1.0, ui::tint(p.accent, 160)) } else { Stroke::new(1.0, p.border) };
            ui.painter().rect(
                rect,
                CornerRadius::same(ui::RADIUS),
                ui.visuals().extreme_bg_color,
                stroke,
                egui::StrokeKind::Inside,
            );
            if resp.clicked() {
                edit = true;
            }
            let inner = rect.shrink2(Vec2::new(4.0, 0.0));
            let mut child =
                ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
            child.spacing_mut().item_spacing.x = 2.0;
            if let Some(to) = breadcrumbs(&mut child, &path, sep, &mut edit) {
                nav = Some(to);
            }
        });
        if edit {
            self.path_edit = Some(path);
        } else if let Some(to) = nav {
            self.list(to);
        }
    }

    fn listing_ui(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let sep = self.sep();
        let mut nav = None;
        let mut pull = None;
        egui::ScrollArea::vertical().id_salt("session-files").auto_shrink([false, false]).show(ui, |ui| {
            if self.listing_pending {
                ui::busy(ui, t!("Reading the folder…", "Lendo a pasta…"));
            }
            match &self.listing {
                None if !self.listing_pending => {
                    ui::caption(ui, t!("Nothing listed yet.", "Nada listado ainda."));
                }
                None => {}
                Some((_, Err(e))) => {
                    ui::banner(ui, Tone::Danger, e, None);
                }
                Some((_, Ok(entries))) if entries.is_empty() => {
                    ui::empty_state(
                        ui,
                        icons::FOLDER_OPEN,
                        t!("This folder is empty", "Esta pasta está vazia"),
                        t!("Send files to put something here.", "Envie arquivos para colocar algo aqui."),
                    );
                }
                Some((path, Ok(entries))) => {
                    for e in entries {
                        match file_row(ui, &p, e) {
                            RowAction::None => {}
                            RowAction::Open => nav = Some(join(path, &e.name, sep)),
                            RowAction::Download => pull = Some(join(path, &e.name, sep)),
                        }
                    }
                }
            }
        });
        if let Some(to) = nav {
            self.list(to);
        }
        if let Some(remote) = pull {
            self.send(Cmd::Pull { remote, local_dir: self.download_dir.clone() });
        }
    }

    /// Send, the transfers, and where downloads go.
    fn files_bottom(&mut self, ui: &mut egui::Ui, transfers_height: f32) {
        let p = ui::pal(ui);
        ui.horizontal_wrapped(|ui| {
            if ui::secondary(ui, ui::with_icon(icons::UPLOAD_SIMPLE, t!("Send files…", "Enviar arquivos…"))).clicked()
                && let Some(paths) = rfd::FileDialog::new().pick_files()
            {
                for path in paths {
                    self.send(Cmd::Push(path));
                }
            }
            ui::caption(ui, t!("or drop files here", "ou solte arquivos aqui"));
        });
        if !self.transfers.is_empty() {
            ui.add_space(space::S);
            let n = self.transfers.len();
            let finished = n - self.transfers_running();
            let mut clear = false;
            ui::row(
                ui,
                |ui| {
                    ui.label(RichText::new(tf!("Transfers ({n})", "Transferências ({n})")).strong());
                },
                |ui| {
                    if finished > 0 && ui::link(ui, t!("Clear finished", "Limpar concluídos")).clicked() {
                        clear = true;
                    }
                },
            );
            egui::ScrollArea::vertical()
                .id_salt("session-transfers")
                .max_height(transfers_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for t in self.transfers.values().rev() {
                        transfer_row(ui, &p, t);
                    }
                });
            if clear {
                self.transfers.retain(|_, t| t.finished.is_none());
            }
        }
        ui.add_space(space::S);
        ui::row(
            ui,
            |ui| {
                ui.label(RichText::new(icons::DOWNLOAD_SIMPLE).color(p.muted));
                let text = tf!("Downloads go to {}", "Os downloads vão para {}", self.download_dir.display());
                ui::truncate(ui, RichText::new(text).size(size::SMALL).color(p.muted));
            },
            |ui| {
                if ui::ghost(ui, t!("Open", "Abrir"))
                    .on_hover_text(t!("Open the downloads folder", "Abrir a pasta de downloads"))
                    .clicked()
                {
                    open_folder(&self.download_dir);
                }
            },
        );
    }

    // -- Terminal ------------------------------------------------------------------------

    fn terminal_tab(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let admin_ok = self.admin_terminal_possible();
        let mut select = None;
        let mut close = None;
        let mut open = None;
        if !self.shells.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for s in &self.shells {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let on = s.id == self.shell_active;
                        let color = if s.live() { p.text } else { p.muted };
                        let icon = if s.admin {
                            RichText::new(icons::SHIELD_CHECK).color(p.warning)
                        } else {
                            RichText::new(icons::TERMINAL_WINDOW).color(color)
                        };
                        let n = s.id;
                        let label = RichText::new(tf!(" Terminal {n}", " Terminal {n}")).color(color);
                        let button = if s.admin {
                            egui::Button::selectable(
                                on,
                                (icon, label, RichText::new(" admin").size(size::SMALL).color(p.warning)),
                            )
                        } else {
                            egui::Button::selectable(on, (icon, label))
                        };
                        let r = ui.add(button.min_size(Vec2::new(0.0, CONTROL_H))).on_hover_text(&s.view.status);
                        if r.clicked() {
                            select = Some(s.id);
                        }
                        let hover = if s.live() {
                            tf!("End {}", "Encerrar o {}", s.name())
                        } else {
                            tf!("Close {}", "Fechar o {}", s.name())
                        };
                        let x = egui::Button::new(RichText::new(icons::X).size(size::ICON - 2.0).color(p.muted))
                            .frame_when_inactive(false)
                            .min_size(Vec2::new(26.0, CONTROL_H));
                        if ui.add(x).on_hover_text(hover).clicked() {
                            close = Some(s.id);
                        }
                    });
                }
                ui.add_space(space::XS);
                if admin_ok {
                    let button = egui::Button::new(RichText::new(icons::PLUS).size(size::ICON + 1.0))
                        .frame_when_inactive(false)
                        .min_size(Vec2::splat(CONTROL_H));
                    let (r, _) = MenuButton::from_button(button).ui(ui, |ui| {
                        ui.set_min_width(220.0);
                        if let Some(admin) = new_terminal_items(ui) {
                            open = Some(admin);
                        }
                    });
                    r.on_hover_text(t!("New terminal", "Novo terminal"));
                } else if ui::icon_button(ui, icons::PLUS, t!("New terminal", "Novo terminal")).clicked() {
                    open = Some(false);
                }
            });
            ui.add_space(space::XS);
        }
        if let Some(id) = select {
            self.shell_active = id;
            if let Some(s) = self.shells.iter_mut().find(|s| s.id == id) {
                s.view.request_focus();
            }
        }
        if let Some(id) = close {
            self.close_shell(id);
        }
        if let Some(admin) = open {
            self.start_terminal(admin);
        }

        let connected = self.connected;
        let mut again = None;
        if let Some(s) = self.shells.iter_mut().find(|s| s.id == self.shell_active) {
            ui.horizontal_wrapped(|ui| {
                if s.admin {
                    pill(ui, t!("administrator", "administrador"), Tone::Warning);
                }
                ui::caption(ui, &s.view.status);
            });
            if s.view.ended {
                ui.add_space(space::XS);
                let action = connected.then_some(t!("Open a new terminal", "Abrir um novo terminal"));
                if ui::banner(ui, Tone::Neutral, t!("This shell has closed.", "Este shell foi fechado."), action) {
                    again = Some((s.id, s.admin));
                }
            }
            ui.add_space(space::XS);
            ui.push_id(("session-shell", s.id), |ui| s.view.ui(ui));
        } else if self.shells_opening > 0 {
            ui.add_space(space::L);
            ui::busy(ui, t!("Opening a terminal…", "Abrindo um terminal…"));
        } else {
            ui::empty_state(
                ui,
                icons::TERMINAL_WINDOW,
                t!("No terminal open", "Nenhum terminal aberto"),
                t!(
                    "A shell on that computer, as whoever is signed in there. Open several in tabs.",
                    "Um shell nesse computador, como quem estiver conectado nele. Abra vários, em abas."
                ),
            );
            ui.vertical_centered(|ui| {
                if ui::primary(ui, ui::with_icon(icons::TERMINAL_WINDOW, t!("Open a terminal", "Abrir um terminal")))
                    .clicked()
                {
                    open = Some(false);
                }
                if admin_ok
                    && ui::secondary(
                        ui,
                        ui::with_icon(icons::SHIELD_CHECK, t!("As administrator", "Como administrador")),
                    )
                    .on_hover_text(t!(
                        "A shell with the administrator rights of whoever is signed in there",
                        "Um shell com os direitos de administrador de quem estiver conectado nele"
                    ))
                    .clicked()
                {
                    open = Some(true);
                }
            });
            if let Some(admin) = open.take() {
                self.start_terminal(admin);
            }
        }
        if let Some((id, admin)) = again {
            self.remove_shell(id);
            self.start_terminal(admin);
        }
    }

    // -- Info ----------------------------------------------------------------------------

    fn info_tab(&mut self, ui: &mut egui::Ui) {
        let dash = || "—".to_string();
        let mut items: Vec<(&str, String)> = vec![(t!("Computer", "Computador"), self.host_name())];
        items.push((t!("System", "Sistema"), self.host.as_ref().map_or_else(dash, |h| os_name(&h.os))));
        items.push((t!("Access", "Acesso"), access_label(self.access)));
        items.push((t!("Status", "Situação"), self.status.to_string()));
        let connected = self.connected && self.stats.0 > 0;
        let route = route_label(self.stats.3);
        items.push((t!("Route", "Rota"), if route.is_empty() { dash() } else { route.to_string() }));
        items.push((t!("Latency", "Latência"), if connected { format!("{} ms", self.stats.0) } else { dash() }));
        if self.screen() {
            items.push((
                t!("Frames per second", "Quadros por segundo"),
                if connected { format!("{:.0}", self.stats.1) } else { dash() },
            ));
            items.push((t!("Data", "Dados"), if connected { format!("{:.0} kbit/s", self.stats.2) } else { dash() }));
            let monitors = self
                .monitors
                .iter()
                .map(|m| {
                    let mut s = format!("{} · {}×{}", m.name, m.width, m.height);
                    if m.primary {
                        s.push_str(t!(" · main", " · principal"));
                    }
                    if m.id == self.active {
                        s.push_str(t!(" (shown)", " (exibido)"));
                    }
                    s
                })
                .collect::<Vec<_>>();
            items.push((t!("Monitors", "Monitores"), if monitors.is_empty() { dash() } else { monitors.join("\n") }));
        }
        egui::ScrollArea::vertical().id_salt("session-info").auto_shrink([false, false]).show(ui, |ui| {
            ui::facts(ui, "session-facts", &items);
        });
    }

    // -- Cards and dialogs ---------------------------------------------------------------

    /// Connecting, reconnecting or disconnected for good: say so, with what can be done.
    fn connection_ui(&mut self, ctx: &egui::Context, area: egui::Rect) {
        enum Card {
            Connecting,
            Lost {
                since: Duration,
                attempt: u32,
                reason: String,
            },
            Ended(String),
            /// Connected, waiting for the first picture.
            Waiting,
        }
        let card = if let Some(msg) = &self.fatal {
            Card::Ended(msg.clone())
        } else if let Some(o) = &self.offline {
            if o.since.elapsed() < OFFLINE_GRACE {
                // A short drop: the picture just pauses.
                ctx.request_repaint_after(OFFLINE_GRACE.saturating_sub(o.since.elapsed()));
                return;
            }
            ctx.request_repaint_after(Duration::from_secs(1));
            Card::Lost { since: o.since.elapsed(), attempt: o.attempt, reason: o.reason.clone() }
        } else if !self.connected {
            Card::Connecting
        } else if self.tex.is_none() && self.screen() {
            Card::Waiting
        } else {
            return;
        };
        let host = self.host_name();
        let width = (area.width() - 2.0 * space::L).clamp(240.0, 440.0);
        let mut action = None;
        egui::Area::new(egui::Id::unique("session-connection"))
            .pivot(Align2::CENTER_CENTER)
            .fixed_pos(area.center())
            .show(ctx, |ui| {
                let p = ui::pal(ui);
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(ui::RADIUS_CARD))
                    .inner_margin(Margin::same(20))
                    .shadow(ui.visuals().popup_shadow)
                    .show(ui, |ui| {
                        ui.set_width(width);
                        let title = |ui: &mut egui::Ui, spinner: bool, icon: Option<(&str, Color32)>, text: String| {
                            ui.horizontal(|ui| {
                                if spinner {
                                    ui.spinner();
                                }
                                if let Some((icon, color)) = icon {
                                    ui.label(RichText::new(icon).size(size::SECTION + 2.0).color(color));
                                }
                                ui.add(egui::Label::new(RichText::new(text).size(size::SECTION).strong()).wrap());
                            });
                        };
                        match &card {
                            Card::Waiting => {
                                title(
                                    ui,
                                    true,
                                    None,
                                    tf!("Waiting for the picture from {host}…", "Aguardando a imagem de {host}…"),
                                );
                            }
                            Card::Connecting => {
                                title(ui, true, None, tf!("Connecting to {host}…", "Conectando a {host}…"));
                                if self.status != Status::Connecting {
                                    ui::caption(ui, &self.status.to_string());
                                }
                                ui.add_space(space::M);
                                if ui::secondary(ui, t!("Cancel", "Cancelar")).clicked() {
                                    action = Some(false);
                                }
                            }
                            Card::Lost { since, attempt, reason } => {
                                title(ui, true, None, tf!("Reconnecting to {host}…", "Reconectando a {host}…"));
                                ui.add_space(space::XS);
                                let since_text = seconds(*since);
                                ui::paragraph(
                                    ui,
                                    &tf!(
                                        "The connection was lost {since_text} ago ({reason}).",
                                        "A conexão caiu há {since_text} ({reason})."
                                    ),
                                );
                                ui::caption(
                                    ui,
                                    t!(
                                        "Keyboard and mouse are paused until it is back.",
                                        "Teclado e mouse ficam pausados até ela voltar."
                                    ),
                                );
                                if *since >= OFFLINE_LONG {
                                    ui.add_space(space::XS);
                                    ui::banner(
                                        ui,
                                        Tone::Warning,
                                        &tf!(
                                            "Still trying: {host} may be off, asleep or without network. rdesk keeps \
                                             trying until you disconnect.",
                                            "Ainda tentando: {host} pode estar desligado, em suspensão ou sem rede. O \
                                             rdesk continua tentando até você desconectar."
                                        ),
                                        None,
                                    );
                                }
                                ui::caption(ui, &tf!("Attempt {attempt}", "Tentativa {attempt}"));
                                ui.add_space(space::M);
                                ui.horizontal_wrapped(|ui| {
                                    if ui::primary(ui, t!("Try now", "Tentar agora")).clicked() {
                                        action = Some(true);
                                    }
                                    if ui::secondary(ui, t!("Disconnect", "Desconectar")).clicked() {
                                        action = Some(false);
                                    }
                                });
                            }
                            Card::Ended(msg) => {
                                title(
                                    ui,
                                    false,
                                    Some((icons::LINK_BREAK, p.danger)),
                                    tf!("Disconnected from {host}", "Desconectado de {host}"),
                                );
                                ui.add_space(space::XS);
                                ui::paragraph(ui, msg);
                                ui.add_space(space::M);
                                ui.horizontal_wrapped(|ui| {
                                    if ui::primary(ui, t!("Reconnect", "Reconectar")).clicked() {
                                        action = Some(true);
                                    }
                                    if ui::secondary(ui, t!("Close", "Fechar")).clicked() {
                                        action = Some(false);
                                    }
                                });
                            }
                        }
                    });
            });
        match action {
            Some(true) => self.reconnect_now(),
            Some(false) => self.exit_requested = true,
            None => {}
        }
    }

    /// Files dragged over the window: say what dropping does.
    fn drop_hint(&self, ctx: &egui::Context) {
        if !self.access.files || ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let rect = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("session-drop")));
        let p = ui::palette(ctx.global_style().visuals.dark_mode);
        painter.rect(
            rect.shrink(space::S),
            CornerRadius::same(ui::RADIUS_CARD),
            ui::tint(p.bg, 220),
            Stroke::new(2.0, p.accent),
            egui::StrokeKind::Inside,
        );
        let host = self.host_name();
        let text = tf!("Drop to send to {host}", "Solte para enviar a {host}");
        let galley = painter.layout(
            format!("{}\n{text}", icons::UPLOAD_SIMPLE),
            egui::FontId::proportional(size::SECTION + 2.0),
            p.text,
            (rect.width() - 4.0 * space::XL).max(120.0),
        );
        painter.galley(rect.center() - galley.size() / 2.0, galley, p.text);
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.confirm else { return };
        let (title, body, ok) = match c {
            Confirm::CloseShell(id) => {
                let Some(s) = self.shells.iter().find(|s| s.id == id) else {
                    self.confirm = None;
                    return;
                };
                let (name, shell) = (s.name(), s.view.status.clone());
                (
                    tf!("End {name}?", "Encerrar o {name}?"),
                    tf!(
                        "The shell ({shell}) ends, and anything running in it stops. To keep it running, close \
                         just the panel.",
                        "O shell ({shell}) é encerrado, e o que estiver rodando nele para. Para mantê-lo rodando, \
                         feche só o painel."
                    ),
                    t!("End the shell", "Encerrar o shell"),
                )
            }
            Confirm::Restart { safe_mode } => {
                let host = self.host_name();
                (
                    tf!("Restart {host}?", "Reiniciar {host}?"),
                    if safe_mode {
                        t!(
                            "It restarts in safe mode with networking (for removing what does not let Windows start \
                             well); this window reconnects once it is back. The next restart is a normal one.",
                            "Ele reinicia em modo de segurança com rede (para remover o que impede o Windows de \
                             iniciar bem); esta janela reconecta quando ele voltar. O próximo reinício é normal."
                        )
                        .to_string()
                    } else {
                        t!(
                            "Unsaved work over there is lost. This window reconnects once it is back.",
                            "O que não foi salvo lá se perde. Esta janela reconecta quando ele voltar."
                        )
                        .to_string()
                    },
                    t!("Restart", "Reiniciar"),
                )
            }
            Confirm::Disconnect => {
                let mut lines = Vec::new();
                match self.shells.iter().filter(|s| s.live()).collect::<Vec<_>>().as_slice() {
                    [] => {}
                    [s] => {
                        let shell = &s.view.status;
                        lines.push(tf!(
                            "The terminal ({shell}) ends, and anything running in it stops.",
                            "O terminal ({shell}) é encerrado, e o que estiver rodando nele para."
                        ));
                    }
                    many => {
                        let n = many.len();
                        lines.push(tf!(
                            "{n} terminals end, and anything running in them stops.",
                            "{n} terminais são encerrados, e o que estiver rodando neles para."
                        ));
                    }
                }
                match self.transfers_running() {
                    0 => {}
                    1 => lines.push(
                        t!(
                            "1 file transfer stops before it finishes.",
                            "1 transferência de arquivo para antes de terminar."
                        )
                        .to_string(),
                    ),
                    n => lines.push(tf!(
                        "{n} file transfers stop before they finish.",
                        "{n} transferências de arquivos param antes de terminar."
                    )),
                }
                let host = self.host_name();
                (
                    tf!("Disconnect from {host}?", "Desconectar de {host}?"),
                    lines.join("\n"),
                    t!("Disconnect", "Desconectar"),
                )
            }
        };
        let Some(yes) = ui::confirm(ctx, "session-confirm", &title, &body, ok, true) else { return };
        self.confirm = None;
        if !yes {
            return;
        }
        match c {
            Confirm::CloseShell(id) => self.remove_shell(id),
            Confirm::Disconnect => self.exit_requested = true,
            Confirm::Restart { safe_mode } => {
                let action = if safe_mode {
                    rdesk_proto::PowerAction::RestartSafeMode
                } else {
                    rdesk_proto::PowerAction::Restart
                };
                self.send(Cmd::Ext(rdesk_proto::ExtClient::Power(action)));
            }
        }
    }

    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.password else { return };
        let mut answer: Option<Option<PasswordAnswer>> = None;
        let title = tf!("Password for {}", "Senha de {}", p.host);
        let body = |ui: &mut egui::Ui| {
            if let Some(e) = &p.error {
                ui::banner(ui, Tone::Danger, e, None);
                ui.add_space(space::XS);
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut *p.text)
                    .password(true)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 7))
                    .hint_text(t!("Password", "Senha")),
            );
            if !p.focused {
                resp.request_focus();
                p.focused = true;
            }
            if p.can_remember {
                ui.checkbox(&mut p.remember, tf!("Remember in {}", "Guardar em {}", crate::secrets::store_name()));
            }
            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        };
        let footer =
            |ui: &mut egui::Ui| ui::dialog_buttons(ui, t!("Log in", "Entrar"), false, &[t!("Cancel", "Cancelar")]);
        let d = ui::dialog_with_footer(ctx, "session-password", &title, 420.0, body, footer);
        match d.inner {
            (true, _) | (_, Some(ui::DialogButton::Primary)) if !p.text.is_empty() => {
                answer = Some(Some(PasswordAnswer {
                    password: zeroize::Zeroizing::new(std::mem::take(&mut *p.text)),
                    remember: p.remember,
                }));
            }
            (_, Some(ui::DialogButton::Other(_))) => answer = Some(None),
            _ if d.closed => answer = Some(None),
            _ => {}
        }
        if let Some(a) = answer {
            p.reply.send(a);
            self.password = None;
        }
    }

    fn handle_key(&self, event: &winit::event::KeyEvent) -> bool {
        let PhysicalKey::Code(code) = event.physical_key else { return false };
        let Some(mut key) = keys::map(code) else { return false };
        if self.lock_by_state(key) {
            // Looked at again in a moment (`about_to_wait`), with the toggle already applied.
            return true;
        }
        if self.swap_cmd_ctrl {
            key = keys::swap_cmd_ctrl(key);
        }
        // A phone has no keyboard layout: send what the key typed here (accents, dead keys,
        // AltGr), not where it is. Shortcuts (Ctrl, Cmd/Win) still go as keys.
        let shortcut = (self.mods.control_key() && !self.mods.alt_key()) || self.mods.super_key();
        if self.features.contains(&Feature::TextKeys) && types_text(key) && !shortcut {
            if event.state == ElementState::Pressed
                && let Some(text) = event.text.as_ref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
            {
                self.input(InputEvent::Text { text: text.to_string() });
            }
            return true;
        }
        self.input(InputEvent::Key { key, down: event.state == ElementState::Pressed });
        true
    }
}

/// Keys that type a character (for hosts that want text, [`Feature::TextKeys`]).
fn types_text(key: Key) -> bool {
    use Key::*;
    matches!(
        key,
        Backquote
            | Backslash
            | BracketLeft
            | BracketRight
            | Comma
            | Equal
            | IntlBackslash
            | IntlRo
            | IntlYen
            | Minus
            | Period
            | Quote
            | Semicolon
            | Slash
            | Space
            | Digit0
            | Digit1
            | Digit2
            | Digit3
            | Digit4
            | Digit5
            | Digit6
            | Digit7
            | Digit8
            | Digit9
            | KeyA
            | KeyB
            | KeyC
            | KeyD
            | KeyE
            | KeyF
            | KeyG
            | KeyH
            | KeyI
            | KeyJ
            | KeyK
            | KeyL
            | KeyM
            | KeyN
            | KeyO
            | KeyP
            | KeyQ
            | KeyR
            | KeyS
            | KeyT
            | KeyU
            | KeyV
            | KeyW
            | KeyX
            | KeyY
            | KeyZ
            | Numpad0
            | Numpad1
            | Numpad2
            | Numpad3
            | Numpad4
            | Numpad5
            | Numpad6
            | Numpad7
            | Numpad8
            | Numpad9
            | NumpadAdd
            | NumpadComma
            | NumpadDecimal
            | NumpadDivide
            | NumpadEqual
            | NumpadMultiply
            | NumpadSubtract
    )
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(match &self.account {
                Some(a) => format!("rdesk — {} ({a})", self.name),
                None => format!("rdesk — {}", self.name),
            })
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 420.0));
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
                self.close_requested();
                if self.exit_requested {
                    self.send(Cmd::Quit);
                    el.exit();
                }
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gfx.as_mut() {
                    self.fullscreen = g.window.fullscreen().is_some();
                    if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                        g.painter.on_window_resized(ViewportId::ROOT, w, h);
                    }
                }
            }
            WindowEvent::Focused(false) => {
                self.window_focused = false;
                self.remote_buttons.clear();
                self.input(InputEvent::ReleaseAll);
            }
            // Back from minimized or covered: frames kept arriving, but nothing was drawn
            // (macOS skips redraws then) and the wake-up for new frames was spent. The
            // picture is current in memory: just draw it.
            WindowEvent::Focused(true) => {
                self.window_focused = true;
                self.sync_locks(true);
                self.request_redraw();
            }
            WindowEvent::Occluded(false) => self.request_redraw(),
            WindowEvent::DroppedFile(p) => {
                self.send(Cmd::Push(p.clone()));
                if self.access.files {
                    self.open_panel(Tab::Files);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event: kev, is_synthetic, .. } => {
                // Synthetic events are winit replaying held keys on focus changes. A
                // focused terminal or field, or a dialog, keeps the keys.
                if !wants_kb
                    && self.password.is_none()
                    && self.confirm.is_none()
                    && !is_synthetic
                    && self.handle_key(kev)
                {
                    return;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let remote = self.to_remote(position.x, position.y);
                // Dragging the panel's edge (or a slider) over the picture is egui's.
                self.pointer_inside = remote.is_some() && !over_egui && !self.egui.egui_is_using_pointer();
                if let Some(p) = remote
                    && self.pointer_inside
                {
                    self.pending_move = Some(p);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let down = *state == ElementState::Pressed;
                let on_picture = self.pointer_inside && !over_egui;
                if let Some(b) = remote_button(*button)
                    && route_button(&mut self.remote_buttons, b, down, on_picture)
                {
                    if down {
                        // A click on the picture: the keyboard goes back to the remote screen,
                        // and open menus close.
                        self.egui.memory_mut(|m| {
                            if let Some(id) = m.focused() {
                                m.surrender_focus(id);
                            }
                        });
                        egui::Popup::close_all(&self.egui);
                        self.request_redraw();
                    }
                    if let Some(p) = self.pending_move.take() {
                        self.input(InputEvent::MouseMove { x: p.0, y: p.1 });
                    }
                    self.input(InputEvent::MouseButton { button: b, down });
                    return;
                }
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
            UiEvent::Extensions(f) => {
                self.features = f;
                self.sync_locks(true);
            }
            UiEvent::FrameUpdated | UiEvent::Wake | UiEvent::Cursor { .. } | UiEvent::Clipboard(_) => {}
            UiEvent::Quality { fps, jpeg_quality, scale_pct } => {
                self.auto_choice = Some((fps, jpeg_quality, scale_pct));
            }
            UiEvent::CursorShape { id, image } => {
                if let Some(img) = image {
                    let made = winit::window::CustomCursor::from_rgba(
                        img.rgba,
                        img.width,
                        img.height,
                        img.hot_x.min(img.width.saturating_sub(1)),
                        img.hot_y.min(img.height.saturating_sub(1)),
                    );
                    match made {
                        Ok(src) => {
                            if self.cursors.len() > 64 {
                                self.cursors.clear();
                            }
                            self.cursors.insert(id, el.create_custom_cursor(src));
                        }
                        Err(e) => tracing::debug!("pointer image: {e}"),
                    }
                }
                self.cursor_id = id;
                self.cursor_shown = None;
            }
            UiEvent::Terminal { admin, result: Ok(slot) } => {
                self.shells_opening = self.shells_opening.saturating_sub(1);
                if let Some(link) = slot.0.lock().unwrap().take() {
                    let id = self.next_shell;
                    self.next_shell += 1;
                    self.shells.push(Shell { id, view: TermView::new(link, 100, 30), admin });
                    self.shell_active = id;
                    self.open_panel(Tab::Terminal);
                }
            }
            UiEvent::Terminal { result: Err(e), .. } => {
                self.shells_opening = self.shells_opening.saturating_sub(1);
                self.toasts.push(format!("Terminal: {e}"), true);
            }
            UiEvent::Status(s) => {
                self.status = s;
                self.update_title();
            }
            UiEvent::Reconnecting { reason, attempt } => {
                self.connected = false;
                self.listing_pending = false;
                // Counted from the first drop, not from each attempt.
                let since = self.offline.as_ref().map(|o| o.since).unwrap_or_else(Instant::now);
                self.offline = Some(Offline { since, attempt, reason });
            }
            UiEvent::Raise(what) => {
                if let Some(g) = &self.gfx {
                    g.window.set_visible(true);
                    g.window.set_minimized(false);
                    g.window.focus_window();
                }
                match what {
                    // The app's "Connect" for a window that opened on a terminal.
                    Raise::Window => self.show_screen(),
                    Raise::Files => {
                        self.open_panel(Tab::Files);
                        if !self.listing_pending {
                            self.list(self.remote_path.clone());
                        }
                    }
                    Raise::Terminal => self.show_terminal(),
                }
            }
            UiEvent::PasswordNeeded { host, error, can_remember, reply } => {
                // A newer request replaces an unanswered one (that login attempt is gone).
                if let Some(old) = self.password.take() {
                    old.reply.send(None);
                }
                self.password = Some(PasswordPrompt {
                    host,
                    error,
                    can_remember,
                    reply,
                    text: zeroize::Zeroizing::new(String::new()),
                    remember: false,
                    focused: false,
                });
                if let Some(g) = &self.gfx {
                    g.window.focus_window();
                }
            }
            UiEvent::Notice(text) => {
                tracing::warn!("host: {text}");
                self.toasts.push_tone(text, Tone::Warning);
            }
            UiEvent::Access(a) => {
                self.access = a;
                // Already on a terminal it opened with (or a reconnect): stay there.
                let settled = self.panel && self.tab != Tab::Info && self.tabs().iter().any(|(t, _)| *t == self.tab);
                if !self.screen() && !settled {
                    // No picture: the window is the panel, on what the session may do.
                    if a.files {
                        self.open_panel(Tab::Files);
                        if !self.listing_pending {
                            self.list(self.remote_path.clone());
                        }
                    } else if a.terminal {
                        self.open_panel(Tab::Terminal);
                    }
                }
                if a != rdesk_proto::Access::FULL {
                    self.toasts.push_tone(tf!("This session: {}", "Esta sessão: {}", access_label(a)), Tone::Neutral);
                }
            }
            UiEvent::Connected(h) => {
                self.connected = true;
                (self.privacy, self.block_input) = (false, false);
                if std::mem::take(&mut self.terminal_on_connect) {
                    self.start_terminal(false);
                }
                if let Some(o) = self.offline.take() {
                    if o.since.elapsed() >= OFFLINE_GRACE {
                        let after = seconds(o.since.elapsed());
                        self.toasts.push(tf!("Reconnected after {after}", "Reconectado após {after}"), false);
                    }
                    // The file list showed the old connection's listing: read it again.
                    if self.listing.is_some() {
                        self.list(self.remote_path.clone());
                    }
                }
                self.fatal = None;
                self.monitors = h.monitors.clone();
                self.active = h.active_monitor;
                self.host = Some(h);
            }
            UiEvent::Monitors { monitors, active } => {
                self.monitors = monitors;
                self.active = active;
            }
            UiEvent::DirListing { path, mut result } => {
                self.listing_pending = false;
                if let Ok(entries) = &mut result {
                    self.remote_path = path.clone();
                    // Folders first, then by name.
                    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
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
            UiEvent::Stats { rtt_ms, fps, kbps, route } => self.stats = (rtt_ms, fps, kbps, route),
            UiEvent::Fatal(msg) => {
                self.status = Status::Disconnected;
                self.connected = false;
                self.listing_pending = false;
                self.offline = None;
                self.confirm = None;
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
        if self.exit_requested {
            self.send(Cmd::Quit);
            el.exit();
            return;
        }
        if let Some(p) = self.pending_move.take() {
            self.input(InputEvent::MouseMove { x: p.0, y: p.1 });
        }
        // No event says Caps Lock changed on a Mac: look while the window has the keyboard.
        let locks_watch = self.window_focused && self.connected && self.screen() && keys::locks().is_some();
        if locks_watch && self.locks_checked.elapsed() >= LOCKS_EVERY {
            self.sync_locks(false);
        }
        let locks_next = locks_watch.then(|| self.locks_checked + LOCKS_EVERY);
        match self.next_repaint {
            Some(t) if t <= Instant::now() => {
                self.next_repaint = None;
                self.request_redraw();
                el.set_control_flow(locks_next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(locks_next.map_or(t, |c| c.min(t)))),
            None => el.set_control_flow(locks_next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------------------

/// A badge as tall as its text (`ui::badge` grows to the height of the line it sits in).
fn pill(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    let p = ui::pal(ui);
    let c = tone.color(&p);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(size::SMALL), c);
    let size = galley.size() + Vec2::new(16.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), ui::tint(c, if p.dark { 46 } else { 30 }));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, c);
    resp
}

/// The width of `text` at `size` (to decide what fits in the toolbar).
fn text_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(size), Color32::WHITE).size().x
}

fn keyboard_label(to_terminal: bool) -> &'static str {
    if to_terminal {
        t!("Keyboard: terminal", "Teclado: terminal")
    } else {
        t!("Keyboard: remote screen", "Teclado: tela remota")
    }
}

/// The buttons the remote computer knows.
fn remote_button(b: winit::event::MouseButton) -> Option<MouseButton> {
    match b {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        winit::event::MouseButton::Back => Some(MouseButton::Back),
        winit::event::MouseButton::Forward => Some(MouseButton::Forward),
        winit::event::MouseButton::Other(_) => None,
    }
}

/// Whether a mouse button goes to the remote computer (else to egui). A press goes there
/// when made on the picture, and a release goes where its press went (`held`: the buttons
/// pressed on the picture).
fn route_button(held: &mut Vec<MouseButton>, b: MouseButton, down: bool, on_picture: bool) -> bool {
    if down {
        if on_picture && !held.contains(&b) {
            held.push(b);
        }
        on_picture
    } else if let Some(i) = held.iter().position(|&h| h == b) {
        held.swap_remove(i);
        true
    } else {
        false
    }
}

fn route_label(route: &str) -> String {
    match route {
        "" => String::new(),
        "direct" => t!("direct", "direta").into(),
        "relay" => t!("relay", "relay").into(),
        other => other.into(),
    }
}

fn os_name(os: &str) -> String {
    match os {
        "windows" => "Windows".into(),
        "macos" => "macOS".into(),
        "linux" => "Linux".into(),
        other => other.into(),
    }
}

/// The "+" menu's items: `Some(admin)` when one was picked.
fn new_terminal_items(ui: &mut egui::Ui) -> Option<bool> {
    let item = |ui: &mut egui::Ui, icon: &str, text: &str| {
        ui.add(
            egui::Button::new(ui::with_icon(icon, text)).frame_when_inactive(false).min_size(Vec2::new(0.0, CONTROL_H)),
        )
    };
    let mut out = None;
    if item(ui, icons::TERMINAL_WINDOW, t!("Terminal", "Terminal")).clicked() {
        out = Some(false);
    }
    if item(ui, icons::SHIELD_CHECK, t!("Terminal (admin)", "Terminal (admin)"))
        .on_hover_text(t!(
            "A shell with the administrator rights of whoever is signed in there",
            "Um shell com os direitos de administrador de quem estiver conectado nele"
        ))
        .clicked()
    {
        out = Some(true);
    }
    if out.is_some() {
        ui.close();
    }
    out
}

/// The path as clickable folders, the last ones that fit (a "…" for the rest, which
/// opens the path for typing). Returns the folder clicked.
fn breadcrumbs(ui: &mut egui::Ui, path: &str, sep: char, edit: &mut bool) -> Option<String> {
    let p = ui::pal(ui);
    let crumbs = crumbs(path, sep);
    let pad = 6.0;
    let gap = ui.spacing().item_spacing.x;
    let caret_w = text_width(ui, icons::CARET_RIGHT, size::SMALL) + gap;
    let widths: Vec<f32> =
        crumbs.iter().map(|(label, _)| text_width(ui, label, size::BODY) + 2.0 * pad + gap).collect();
    let ellipsis_w = text_width(ui, "…", size::BODY) + 2.0 * pad + gap + caret_w;
    let avail = ui.available_width();
    // From the end: the current folder always shows (cut if it must be).
    let mut first = crumbs.len().saturating_sub(1);
    let mut used = widths.last().copied().unwrap_or(0.0);
    while first > 0 {
        let more = widths[first - 1] + caret_w;
        let reserve = if first - 1 > 0 { ellipsis_w } else { 0.0 };
        if used + more + reserve > avail {
            break;
        }
        used += more;
        first -= 1;
    }
    let mut nav = None;
    let small = |ui: &mut egui::Ui, text: &str| {
        ui.add(
            egui::Button::new(RichText::new(text).color(p.muted))
                .frame_when_inactive(false)
                .min_size(Vec2::new(0.0, 26.0)),
        )
    };
    let caret = |ui: &mut egui::Ui| {
        ui.label(RichText::new(icons::CARET_RIGHT).size(size::SMALL).color(p.muted));
    };
    ui.spacing_mut().button_padding.x = pad;
    if first > 0 {
        if small(ui, "…").on_hover_text(path).clicked() {
            *edit = true;
        }
        caret(ui);
    }
    for (i, (label, full)) in crumbs.iter().enumerate().skip(first) {
        let last = i + 1 == crumbs.len();
        if last {
            ui.add_space(pad);
            ui::truncate(ui, RichText::new(label).strong());
        } else {
            if small(ui, label).on_hover_text(full).clicked() {
                nav = Some(full.clone());
            }
            caret(ui);
        }
    }
    nav
}

/// The folders of a path, each with its full path: `C:\Users\ana` → `C:\`, `Users`, `ana`;
/// `/home/ana` → `/`, `home`, `ana`.
fn crumbs(path: &str, sep: char) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = path;
    let mut acc = String::new();
    if let Some(r) = rest.strip_prefix(['/', '\\']) {
        acc.push(sep);
        out.push((sep.to_string(), acc.clone()));
        rest = r;
    }
    for part in rest.split(['/', '\\']).filter(|s| !s.is_empty()) {
        if acc.is_empty() {
            acc.push_str(part);
            // A drive: "C:" is shown as "C:\".
            if part.ends_with(':') {
                acc.push(sep);
            }
        } else {
            acc = join(&acc, part, sep);
        }
        let label = if part.ends_with(':') && out.is_empty() { format!("{part}{sep}") } else { part.to_string() };
        out.push((label, acc.clone()));
    }
    if out.is_empty() {
        out.push((path.to_string(), path.to_string()));
    }
    out
}

enum RowAction {
    None,
    Open,
    Download,
}

/// A file or folder: a click opens a folder; files download with their button (or a
/// double click).
fn file_row(ui: &mut egui::Ui, p: &ui::Palette, e: &DirEntry) -> RowAction {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(ui::RADIUS), p.hover);
    }
    let mut action = RowAction::None;
    if (e.is_dir && resp.clicked()) || (!e.is_dir && resp.double_clicked()) {
        action = if e.is_dir { RowAction::Open } else { RowAction::Download };
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(space::S, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui::row(
        &mut child,
        |ui| {
            let (icon, color) = if e.is_dir { (icons::FOLDER, p.accent) } else { (icons::FILE, p.muted) };
            ui.label(RichText::new(icon).size(size::ICON + 1.0).color(color));
            ui::truncate(ui, e.name.as_str());
        },
        |ui| {
            if e.is_dir {
                ui.label(RichText::new(icons::CARET_RIGHT).color(p.muted));
            } else {
                if ui::icon_button(ui, icons::DOWNLOAD_SIMPLE, &tf!("Download {}", "Baixar {}", e.name)).clicked() {
                    action = RowAction::Download;
                }
                ui.label(RichText::new(human(e.size)).size(size::SMALL).color(p.muted));
            }
        },
    );
    action
}

fn transfer_row(ui: &mut egui::Ui, p: &ui::Palette, t: &Transfer) {
    let (icon, color) = match &t.finished {
        None => (icons::ARROWS_CLOCKWISE, p.accent),
        Some(Ok(_)) => (icons::CHECK_CIRCLE, p.success),
        Some(Err(_)) => (icons::WARNING_CIRCLE, p.danger),
    };
    ui::row(
        ui,
        |ui| {
            ui.label(RichText::new(icon).color(color));
            ui::truncate(ui, t.name.as_str());
        },
        |ui| {
            if t.finished.is_none() && t.total > 0 {
                let pct = (t.done as f64 / t.total as f64 * 100.0).round();
                ui.label(RichText::new(format!("{pct:.0}%")).size(size::SMALL).color(p.muted));
            }
        },
    );
    match &t.finished {
        None => {
            let frac = if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).desired_height(6.0).fill(p.accent));
            let (done, total) = (human(t.done), human(t.total));
            ui::caption(ui, &tf!("{done} of {total}", "{done} de {total}"));
        }
        Some(Ok(where_)) => {
            ui::truncate(
                ui,
                RichText::new(format!("{}  {where_}", icons::ARROW_RIGHT)).size(size::SMALL).color(p.muted),
            );
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).size(size::SMALL).color(p.danger)).wrap());
        }
    }
    ui.add_space(space::XS);
}

fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::process::Command::new(program).arg(dir).spawn();
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

/// "8 s", "3 min", "1 h 5 min".
fn seconds(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
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

// ---------------------------------------------------------------------------------------
// The ui gallery
// ---------------------------------------------------------------------------------------

#[cfg(test)]
impl App {
    /// A session to `DESKTOP-…` with sample data, connected, no panel open.
    fn sample() -> Self {
        let mut app = App::new(
            conn::test_handle(),
            "escritorio-financeiro".into(),
            false,
            PathBuf::from(r"C:\Users\jeferson.santos.silva\Downloads\rdesk\Sessões remotas"),
        );
        let monitors = vec![
            MonitorInfo {
                id: 1,
                name: "DELL U2720Q".into(),
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
                scale: 1.5,
                primary: true,
            },
            MonitorInfo {
                id: 2,
                name: r"\\.\DISPLAY2 Samsung Odyssey G5".into(),
                x: 3840,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.0,
                primary: false,
            },
        ];
        let host = "DESKTOP-FINANCEIRO-ANA-BEATRIZ-02";
        app.host = Some(HostInfo {
            hostname: host.into(),
            os: "windows".into(),
            monitors: monitors.clone(),
            active_monitor: 1,
        });
        app.monitors = monitors;
        app.active = 1;
        app.connected = true;
        app.status = Status::Connected { host: host.into() };
        app.stats = (38, 29.6, 2450.0, "direct");
        app.sample.picture = true;
        app
    }

    fn with_files(mut self) -> Self {
        let path = r"C:\Users\ana.beatriz\Documents\Projetos de 2026\Relatórios financeiros trimestrais";
        let e = |name: &str, is_dir: bool, size: u64| DirEntry { name: name.into(), is_dir, size, mtime: 0 };
        self.remote_path = path.into();
        self.listing = Some((
            path.into(),
            Ok(vec![
                e("Contratos assinados e digitalizados do segundo semestre de 2026", true, 0),
                e("Fotos", true, 0),
                e(
                    "Planilha de controle de despesas da família Santos - versão final revisada (3).xlsx",
                    false,
                    2_411_724,
                ),
                e("leia-me.txt", false, 812),
                e("Apresentação para a reunião do conselho.pptx", false, 48_234_112),
                e("backup.zip", false, 1_288_490_188),
            ]),
        ));
        let t = |name: &str, done, total, finished| Transfer { name: name.into(), done, total, finished };
        self.transfers.insert(
            1,
            t(
                "contrato-de-prestacao-de-servicos.pdf",
                0,
                0,
                Some(
                    Ok(r"C:\Users\jeferson.santos.silva\Downloads\rdesk\contrato-de-prestacao-de-servicos.pdf".into()),
                ),
            ),
        );
        self.transfers.insert(
            2,
            t(
                "fotos-das-férias.zip",
                0,
                0,
                Some(Err("o computador remoto ficou sem espaço em disco (são necessários 4,2 GB)".into())),
            ),
        );
        self.transfers
            .insert(3, t("backup-completo-do-servidor-de-arquivos-2026-11-20.zip", 356_515_840, 1_288_490_188, None));
        self.open_panel(Tab::Files);
        self
    }

    fn with_shells(mut self) -> Self {
        let mut out = String::new();
        out.push_str(
            "Windows PowerShell\r\nCopyright (C) Microsoft Corporation. Todos os direitos reservados.\r\n\r\n",
        );
        out.push_str("PS C:\\Users\\ana> Get-ChildItem\r\n\r\n    Diretório: C:\\Users\\ana\r\n\r\n");
        out.push_str("Mode                 LastWriteTime         Length Name\r\n");
        out.push_str("----                 -------------         ------ ----\r\n");
        for (d, name) in [("d-----", "Documentos"), ("d-----", "Downloads"), ("d-----", "Imagens")] {
            out.push_str(&format!("{d}        20/11/2026     09:14                \x1b[1;34m{name}\x1b[0m\r\n"));
        }
        out.push_str("-a----        19/11/2026     18:02          18342 relatório.docx\r\n\r\n");
        out.push_str("PS C:\\Users\\ana> \x1b[33mWrite-Host\x1b[0m \"pronto\"\r\npronto\r\nPS C:\\Users\\ana> ");
        let mut one = TermView::sample("PowerShell como ana", out.as_bytes(), 100, 30);
        one.select((3, 0), (5, 30));
        let mut lines = String::new();
        for i in 1..=80 {
            lines.push_str(&format!("C:\\Windows\\System32> linha de log número {i}: serviço iniciado\r\n"));
        }
        let two = TermView::sample("PowerShell como ana (administrador)", lines.as_bytes(), 100, 30);
        self.shells.push(Shell { id: 1, view: one, admin: false });
        self.shells.push(Shell { id: 2, view: two, admin: true });
        self.next_shell = 3;
        self.shell_active = 1;
        self.open_panel(Tab::Terminal);
        self
    }

    /// Draw it as the window does, with a made-up remote screen.
    fn gallery_ui(&mut self, ui: &mut egui::Ui) {
        if self.sample.picture && self.tex.is_none() {
            self.tex = Some(ui.ctx().load_texture("remote", sample_picture(), TextureOptions::LINEAR));
        }
        if self.sample.scrollback > 0
            && let Some(s) = self.shells.iter_mut().find(|s| s.id == self.shell_active)
        {
            s.view.scroll_back(self.sample.scrollback);
        }
        self.ui(ui);
    }
}

/// A remote desktop: a gradient with a few windows on it.
#[cfg(test)]
fn sample_picture() -> ColorImage {
    let (w, h) = (1600usize, 900usize);
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let mut c = Color32::from_rgb(
                (20.0 + 60.0 * fx) as u8,
                (60.0 + 90.0 * fy) as u8,
                (120.0 + 100.0 * (1.0 - fx)) as u8,
            );
            let win = |x0, y0, x1, y1| x >= x0 && x < x1 && y >= y0 && y < y1;
            if win(180, 120, 980, 640) || win(1040, 260, 1480, 760) {
                c = if win(180, 120, 980, 156) || win(1040, 260, 1480, 296) {
                    Color32::from_rgb(40, 44, 52)
                } else {
                    Color32::from_rgb(236, 238, 242)
                };
            }
            if y >= h - 48 {
                c = Color32::from_rgb(28, 30, 36);
            }
            px.push(c);
        }
    }
    ColorImage::new([w, h], px)
}

/// Screens for the ui gallery (see `app/gallery.rs`): sample data, long texts.
#[cfg(test)]
pub(crate) fn gallery() -> Vec<crate::app::gallery::Screen> {
    use crate::app::gallery::screen;
    let s = |name: &'static str, make: fn() -> App| {
        screen(name, move || {
            let mut a = make();
            move |ui: &mut egui::Ui| a.gallery_ui(ui)
        })
    };
    vec![
        s("session-connected", App::sample),
        s("session-menu-monitor", || {
            let mut a = App::sample();
            a.sample.menu = Some(Tool::Monitor);
            a
        }),
        s("session-menu-quality", || {
            let mut a = App::sample();
            a.sample.menu = Some(Tool::Quality);
            a
        }),
        s("session-menu-keys", || {
            let mut a = App::sample();
            a.sample.menu = Some(Tool::Keys);
            a
        }),
        s("session-files", || App::sample().with_files()),
        s("session-files-loading", || {
            let mut a = App::sample().with_files();
            a.listing = Some((a.remote_path.clone(), Err("Acesso negado: C:\\Users\\ana.beatriz\\Documents\\Projetos de 2026\\Relatórios financeiros trimestrais (os error 5)".into())));
            a.transfers.clear();
            a.listing_pending = true;
            a.path_edit = Some(r"C:\Users\ana.beatriz\Documents\Projetos de 2026".into());
            a
        }),
        s("session-terminal", || {
            let mut a = App::sample().with_shells();
            a.shells[0].view.request_focus();
            a
        }),
        s("session-terminal-history", || {
            let mut a = App::sample().with_shells();
            a.shell_active = 2;
            a.sample.scrollback = 12;
            a
        }),
        s("session-terminal-ended", || {
            let mut a = App::sample().with_shells();
            a.shells[1].view.end("Encerrado (código 0)");
            a.shell_active = 2;
            a
        }),
        s("session-terminal-empty", || {
            let mut a = App::sample();
            a.open_panel(Tab::Terminal);
            a
        }),
        s("session-info", || {
            let mut a = App::sample();
            a.open_panel(Tab::Info);
            a
        }),
        s("session-connecting", || {
            let mut a = App::sample();
            a.sample.picture = false;
            a.connected = false;
            a.host = None;
            a.monitors.clear();
            a.status = Status::Authenticating;
            a
        }),
        s("session-waiting", || {
            let mut a = App::sample();
            a.sample.picture = false;
            a
        }),
        s("session-reconnecting", || {
            let mut a = App::sample().with_files();
            a.connected = false;
            a.offline = Some(Offline {
                since: Instant::now() - Duration::from_secs(25),
                attempt: 4,
                reason: "o computador remoto parou de responder".into(),
            });
            a
        }),
        s("session-ended", || {
            let mut a = App::sample();
            a.connected = false;
            a.status = Status::Disconnected;
            a.fatal = Some(
                "Outra pessoa se conectou a este computador com controle total e esta sessão foi encerrada.".into(),
            );
            a
        }),
        s("session-password", || {
            let mut a = App::sample();
            a.sample.picture = false;
            a.connected = false;
            a.status = Status::Authenticating;
            a.password = Some(PasswordPrompt {
                host: "DESKTOP-FINANCEIRO-ANA-BEATRIZ-02".into(),
                error: Some("Senha incorreta. Confira maiúsculas e minúsculas e tente de novo.".into()),
                can_remember: true,
                reply: PasswordReply::test(),
                text: zeroize::Zeroizing::new(String::new()),
                remember: false,
                focused: false,
            });
            a
        }),
        s("session-confirm-disconnect", || {
            let mut a = App::sample().with_files().with_shells();
            a.confirm = Some(Confirm::Disconnect);
            a
        }),
        s("session-confirm-close-shell", || {
            let mut a = App::sample().with_shells();
            a.confirm = Some(Confirm::CloseShell(2));
            a
        }),
        s("session-view-only", || {
            let mut a = App::sample();
            a.access = rdesk_proto::Access::VIEW;
            a.open_panel(Tab::Info);
            a
        }),
        s("session-fullscreen-bar", || {
            let mut a = App::sample();
            a.fullscreen = true;
            a.bar_revealed = true;
            a
        }),
        s("session-terminal-only", || {
            let mut a = App::sample().with_shells();
            a.sample.picture = false;
            a.access = rdesk_proto::Access::TERMINAL;
            a
        }),
        s("session-files-only", || {
            let mut a = App::sample().with_files();
            a.sample.picture = false;
            a.access = rdesk_proto::Access::FILES;
            a
        }),
        // Opened from the app's Terminal: full control, no picture until "Show screen".
        s("session-terminal-start", || {
            let mut a = App::sample().with_shells();
            a.sample.picture = false;
            a.video = false;
            a
        }),
    ]
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

    #[test]
    fn a_release_goes_where_its_press_went() {
        let mut held = Vec::new();
        // The panel's edge dragged out over the picture: the release is egui's, or the
        // edge stays stuck to the mouse.
        assert!(!route_button(&mut held, MouseButton::Left, true, false));
        assert!(!route_button(&mut held, MouseButton::Left, false, true));
        // A drag on the picture that ends over the panel: the remote gets its release.
        assert!(route_button(&mut held, MouseButton::Left, true, true));
        assert!(route_button(&mut held, MouseButton::Left, false, false));
        assert!(held.is_empty());
        // Two buttons, released in any order.
        assert!(route_button(&mut held, MouseButton::Left, true, true));
        assert!(!route_button(&mut held, MouseButton::Right, true, false));
        assert!(!route_button(&mut held, MouseButton::Right, false, true));
        assert!(route_button(&mut held, MouseButton::Left, false, true));
        assert!(held.is_empty());
    }

    #[test]
    fn path_crumbs() {
        let c = |path, sep| crumbs(path, sep).into_iter().map(|(l, f)| format!("{l}={f}")).collect::<Vec<_>>();
        assert_eq!(c("/home/ana", '/'), ["/=/", "home=/home", "ana=/home/ana"]);
        assert_eq!(c("/", '/'), ["/=/"]);
        assert_eq!(c("C:\\Users\\ana", '\\'), ["C:\\=C:\\", "Users=C:\\Users", "ana=C:\\Users\\ana"]);
        assert_eq!(c("C:\\", '\\'), ["C:\\=C:\\"]);
        assert_eq!(c("~", '/'), ["~=~"]);
        assert_eq!(c("~/x", '/'), ["~=~", "x=~/x"]);
    }

    #[test]
    fn terminal_window_shows_the_screen_on_request() {
        let mut a = App::sample().with_shells();
        a.video = false;
        assert!(!a.screen(), "the terminal is the whole window");
        assert_eq!(a.tools(), [Tool::Screen]);
        assert!(a.panel_shows(Tab::Terminal));
        a.show_screen();
        assert!(a.screen(), "Show screen brings the picture");
        assert!(a.tools().contains(&Tool::Monitor) && !a.tools().contains(&Tool::Screen));
        assert!(a.panel_shows(Tab::Terminal), "the terminal stays open beside it");

        // Without the right to see the screen there is nothing to ask for.
        let mut v = App::sample().with_shells();
        v.video = false;
        v.access = rdesk_proto::Access::TERMINAL;
        v.show_screen();
        assert!(!v.screen() && v.tools().is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(seconds(Duration::from_secs(8)), "8 s");
        assert_eq!(seconds(Duration::from_secs(185)), "3 min");
        assert_eq!(seconds(Duration::from_secs(3900)), "1 h 5 min");
    }
}
