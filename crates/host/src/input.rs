//! Input injection on a dedicated thread.
//!
//! * Windows: `SendInput` with scan codes (layout-independent) and absolute
//!   coordinates over the whole virtual desktop.
//! * macOS / Linux X11: `enigo` with raw key codes from [`crate::keymap`].
//! * One the program around the host brings ([`set_input_backend`]: the phone app's
//!   accessibility service on Android).
//! * `RDESK_FAKE_INPUT=1`: log only (tests, headless machines).
//!
//! The thread remembers which keys and buttons are held so they can all be
//! released when a session ends: a lost key-up must never leave Ctrl stuck.

use std::collections::BTreeSet;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use rdesk_common::tf;
use rdesk_proto::{InputEvent, Key, MouseButton, NavAction, TouchPoint};
use tokio::sync::broadcast;

use crate::capture::InputRect;
use crate::elevation;

/// At most one "your input is being blocked" notice per this interval.
const NOTICE_EVERY: Duration = Duration::from_secs(10);

pub enum InputCmd {
    /// Map normalized coordinates onto this rectangle from now on.
    Monitor(InputRect),
    Event(InputEvent),
    /// Where the pointer is on the current monitor (see [`Pointer`]).
    Pointer(tokio::sync::oneshot::Sender<Pointer>),
    /// Block the computer's own keyboard and mouse (Windows, with administrator rights), or
    /// let them through again. Injected input still works; Ctrl+Alt+Del at the computer
    /// unblocks.
    BlockLocal(bool, tokio::sync::oneshot::Sender<Result<()>>),
    /// A phone's system button ([`Backend::nav`]).
    Nav(NavAction),
    /// The fingers on a phone's screen now ([`Backend::touch`]).
    Touch(Vec<TouchPoint>),
    /// Caps Lock as it is on the client's keyboard: pressed here if this computer's differs
    /// ([`rdesk_proto::ExtClient::Locks`]).
    Locks {
        caps: bool,
    },
}

/// The pointer on the current monitor, normalized like input (0..=65535 per axis);
/// `visible` is false when it is outside that monitor or cannot be known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

impl Pointer {
    pub const UNKNOWN: Pointer = Pointer { x: 0, y: 0, visible: false };
}

#[derive(Clone)]
pub struct Injector {
    tx: mpsc::Sender<InputCmd>,
    problems: broadcast::Sender<String>,
}

impl Injector {
    pub fn spawn(fake: bool) -> Self {
        // Only a non-elevated Windows host can be locked out by an elevated window (UIPI).
        let watch_uipi = cfg!(windows) && !fake && !elevation::is_elevated();
        Self::spawn_with(move || make_backend(fake), watch_uipi)
    }

    fn spawn_with(make: impl FnOnce() -> Box<dyn Backend> + Send + 'static, watch_uipi: bool) -> Self {
        let (tx, rx) = mpsc::channel::<InputCmd>();
        let (problems, _) = broadcast::channel(8);
        let mut reporter = Reporter { tx: problems.clone(), last: None };
        std::thread::Builder::new()
            .name("rdesk-input".into())
            .spawn(move || {
                let mut backend = make();
                let mut state = State::default();
                while let Ok(cmd) = rx.recv() {
                    // Started by the service: type into the sign-in, lock and UAC screens too.
                    #[cfg(windows)]
                    crate::win::desktop::follow_input_desktop();
                    let blocked_by =
                        if watch_uipi && is_press(&cmd) { elevation::foreground_blocks_input() } else { None };
                    match state.handle(backend.as_mut(), cmd) {
                        // SendInput may "succeed" while UIPI silently drops the event.
                        Ok(()) => {
                            if let Some(app) = blocked_by {
                                reporter.report(elevation::blocked_message(Some(&app)), None);
                            }
                        }
                        Err(e) => {
                            let msg = match (&blocked_by, watch_uipi) {
                                (Some(app), _) => elevation::blocked_message(Some(app)),
                                (None, true) => elevation::blocked_message(None),
                                (None, false) => tf!(
                                    "Remote input failed on the host: {e:#}",
                                    "A entrada remota falhou no host: {e:#}"
                                ),
                            };
                            reporter.report(msg, Some(&e));
                        }
                    }
                }
                state.release_all(backend.as_mut());
            })
            .expect("spawn input thread");
        Self { tx, problems }
    }

    pub fn send(&self, cmd: InputCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn event(&self, ev: InputEvent) {
        self.send(InputCmd::Event(ev));
    }

    /// Where the pointer is now (see [`Pointer`]).
    pub async fn pointer(&self) -> Pointer {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send(InputCmd::Pointer(tx));
        rx.await.unwrap_or(Pointer::UNKNOWN)
    }

    /// Human-readable problems worth telling the remote user (rate-limited).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.problems.subscribe()
    }
}

/// A click, key press or typed text: the moments the user expects something to happen.
fn is_press(cmd: &InputCmd) -> bool {
    matches!(
        cmd,
        InputCmd::Event(
            InputEvent::MouseButton { down: true, .. } | InputEvent::Key { down: true, .. } | InputEvent::Text { .. }
        )
    )
}

struct Reporter {
    tx: broadcast::Sender<String>,
    last: Option<Instant>,
}

impl Reporter {
    fn report(&mut self, msg: String, err: Option<&anyhow::Error>) {
        if self.last.is_some_and(|t| t.elapsed() < NOTICE_EVERY) {
            if let Some(e) = err {
                tracing::debug!("input injection failed: {e:#}");
            }
            return;
        }
        self.last = Some(Instant::now());
        match err {
            Some(e) => tracing::warn!("input injection failed: {e:#}"),
            None => tracing::warn!("{msg}"),
        }
        // Nobody listening (no session) is fine.
        let _ = self.tx.send(msg);
    }
}

/// Makes the input backend (on the input thread).
type MakeBackend = fn() -> Box<dyn Backend>;

static BACKEND: OnceLock<(MakeBackend, Caps)> = OnceLock::new();

/// What a backend brought by [`set_input_backend`] can do beyond a computer's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caps {
    /// A phone's system buttons ([`Backend::nav`]).
    pub nav: bool,
    /// It wants typed characters as text ([`rdesk_proto::Feature::TextKeys`]).
    pub text_keys: bool,
    /// Several fingers at once ([`Backend::touch`]).
    pub multitouch: bool,
}

/// Inject input through the program the host runs in (the phone app) instead of this system's
/// own way. Set once, before the host starts; `make` runs on the input thread.
pub fn set_input_backend(make: MakeBackend, caps: Caps) {
    let _ = BACKEND.set((make, caps));
}

/// What the input in use can do beyond a computer's.
pub fn caps() -> Caps {
    BACKEND.get().map(|b| b.1).unwrap_or_default()
}

fn make_backend(fake: bool) -> Box<dyn Backend> {
    if fake {
        return Box::new(LogBackend);
    }
    if let Some((make, _)) = BACKEND.get() {
        return make();
    }
    match platform_backend() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("input injection unavailable ({e:#}); remote input will be ignored");
            Box::new(LogBackend)
        }
    }
}

pub trait Backend {
    fn key(&mut self, key: Key, down: bool) -> Result<()>;
    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, b: MouseButton, down: bool) -> Result<()>;
    /// Whole notches; positive = up / right.
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn text(&mut self, s: &str) -> Result<()>;
    /// Ctrl+Alt+Del, which no injected key press makes. True if the backend sent the
    /// real one; false to inject the keys as they come.
    fn secure_attention(&mut self) -> Result<bool> {
        Ok(false)
    }
    /// The pointer in the same absolute coordinates as [`Backend::mouse_abs`], if known.
    fn pointer(&mut self) -> Option<(i32, i32)> {
        None
    }
    /// Block (or unblock) the local keyboard and mouse.
    fn block_local(&mut self, _on: bool) -> Result<()> {
        Err(anyhow::anyhow!("blocking the keyboard and mouse works on Windows computers only"))
    }
    /// A phone's system button or action.
    fn nav(&mut self, _action: NavAction) -> Result<()> {
        Err(anyhow::anyhow!("only phones have system buttons"))
    }
    /// The fingers on the screen now, by id, in the same coordinates as [`Backend::mouse_abs`]
    /// (empty: all lifted).
    fn touch(&mut self, _fingers: &[(u8, i32, i32)]) -> Result<()> {
        Err(anyhow::anyhow!("only phones take several fingers"))
    }
    /// Whether Caps Lock is on here, if this backend can tell.
    fn caps_lock(&mut self) -> Option<bool> {
        None
    }
}

/// Whether this computer's input can tell (and so match) its Caps Lock
/// ([`rdesk_proto::Feature::LockKeys`]): Windows and X11. Not a phone's, Wayland's (the portal
/// does not say) or a Mac's (a pressed Caps Lock does not turn the light on there).
pub fn locks_supported() -> bool {
    if BACKEND.get().is_some() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        !crate::wayland::active()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(windows)
    }
}

#[derive(Default)]
struct State {
    rect: Option<InputRect>,
    keys: BTreeSet<Key>,
    buttons: Vec<MouseButton>,
    scroll_acc: (f32, f32),
}

impl State {
    fn handle(&mut self, b: &mut dyn Backend, cmd: InputCmd) -> Result<()> {
        match cmd {
            InputCmd::Monitor(r) => self.rect = Some(r),
            InputCmd::BlockLocal(on, reply) => {
                let _ = reply.send(b.block_local(on));
            }
            InputCmd::Nav(a) => b.nav(a)?,
            // Not while the client holds Caps Lock down itself (the menu's key, an older client).
            InputCmd::Locks { caps } => {
                if !self.keys.contains(&Key::CapsLock)
                    && let Some(now) = b.caps_lock()
                    && now != caps
                {
                    b.key(Key::CapsLock, true)?;
                    b.key(Key::CapsLock, false)?;
                }
            }
            InputCmd::Touch(points) => {
                if let Some(r) = self.rect {
                    let fingers: Vec<(u8, i32, i32)> = points
                        .iter()
                        .take(TouchPoint::MAX)
                        .map(|p| {
                            let (x, y) = map_point(r, p.x, p.y);
                            (p.id, x, y)
                        })
                        .collect();
                    b.touch(&fingers)?;
                }
            }
            InputCmd::Pointer(reply) => {
                let p = match (self.rect, b.pointer()) {
                    (Some(r), Some((x, y))) => unmap_point(r, x, y),
                    _ => Pointer::UNKNOWN,
                };
                let _ = reply.send(p);
            }
            InputCmd::Event(ev) => match ev {
                InputEvent::MouseMove { x, y } => {
                    if let Some(r) = self.rect {
                        let (px, py) = map_point(r, x, y);
                        b.mouse_abs(px, py)?;
                    }
                }
                InputEvent::MouseButton { button, down } => {
                    if down {
                        if !self.buttons.contains(&button) {
                            self.buttons.push(button);
                        }
                    } else {
                        self.buttons.retain(|&x| x != button);
                    }
                    b.button(button, down)?;
                }
                InputEvent::Scroll { dx, dy } => {
                    self.scroll_acc.0 += dx.clamp(-50.0, 50.0);
                    self.scroll_acc.1 += dy.clamp(-50.0, 50.0);
                    let (ix, iy) = (self.scroll_acc.0.trunc(), self.scroll_acc.1.trunc());
                    if ix != 0.0 || iy != 0.0 {
                        self.scroll_acc.0 -= ix;
                        self.scroll_acc.1 -= iy;
                        b.scroll(ix as i32, iy as i32)?;
                    }
                }
                InputEvent::Key { key, down } => {
                    if down && key == Key::Delete && self.ctrl_alt() && b.secure_attention()? {
                        return Ok(());
                    }
                    if down {
                        self.keys.insert(key);
                    } else if !self.keys.remove(&key) {
                        // Releasing a key we never pressed is harmless but pointless.
                        return Ok(());
                    }
                    b.key(key, down)?;
                }
                InputEvent::Text { text } => b.text(&text)?,
                InputEvent::ReleaseAll => self.release_all(b),
            },
        }
        Ok(())
    }

    fn ctrl_alt(&self) -> bool {
        let held = |a, b| self.keys.contains(&a) || self.keys.contains(&b);
        held(Key::ControlLeft, Key::ControlRight) && held(Key::AltLeft, Key::AltRight)
    }

    fn release_all(&mut self, b: &mut dyn Backend) {
        // Release non-modifiers first so modifiers don't trigger shortcuts on the way up.
        let mut keys: Vec<Key> = std::mem::take(&mut self.keys).into_iter().collect();
        keys.sort_by_key(|k| k.is_modifier());
        for k in keys {
            let _ = b.key(k, false);
        }
        for btn in std::mem::take(&mut self.buttons) {
            let _ = b.button(btn, false);
        }
        self.scroll_acc = (0.0, 0.0);
    }
}

/// Normalized 0..=65535 → absolute coordinates inside `r`.
pub fn map_point(r: InputRect, x: u16, y: u16) -> (i32, i32) {
    let fx = x as f64 / 65535.0;
    let fy = y as f64 / 65535.0;
    let px = r.x + (fx * (r.w.saturating_sub(1)) as f64).round() as i32;
    let py = r.y + (fy * (r.h.saturating_sub(1)) as f64).round() as i32;
    (px, py)
}

/// Absolute coordinates → normalized inside `r` (the inverse of [`map_point`]).
pub fn unmap_point(r: InputRect, x: i32, y: i32) -> Pointer {
    let inside = x >= r.x && y >= r.y && x < r.x + r.w as i32 && y < r.y + r.h as i32;
    if !inside {
        return Pointer::UNKNOWN;
    }
    let n = |v: i32, len: u32| {
        ((v as f64 / len.saturating_sub(1).max(1) as f64) * 65535.0).round().clamp(0.0, 65535.0) as u16
    };
    Pointer { x: n(x - r.x, r.w), y: n(y - r.y, r.h), visible: true }
}

struct LogBackend;

impl Backend for LogBackend {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        tracing::debug!("fake input: key {key:?} down={down}");
        Ok(())
    }
    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        tracing::trace!("fake input: mouse {x},{y}");
        Ok(())
    }
    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        tracing::debug!("fake input: button {b:?} down={down}");
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        tracing::debug!("fake input: scroll {dx},{dy}");
        Ok(())
    }
    fn text(&mut self, s: &str) -> Result<()> {
        tracing::debug!("fake input: text {} chars", s.chars().count());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn platform_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(win::WinBackend))
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::keymap::{self, WinKey};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    pub struct WinBackend;

    fn send(inputs: &[INPUT]) -> Result<()> {
        let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        if n as usize != inputs.len() {
            // Typically UIPI: the foreground window is elevated and we are not.
            anyhow::bail!("SendInput injected {n}/{} events (is an elevated window focused?)", inputs.len());
        }
        Ok(())
    }

    fn kbd(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    impl Backend for WinBackend {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            let up = if down { 0 } else { KEYEVENTF_KEYUP };
            match keymap::windows(key) {
                Some(WinKey::Scan { code, ext }) => {
                    let flags = KEYEVENTF_SCANCODE | up | if ext { KEYEVENTF_EXTENDEDKEY } else { 0 };
                    send(&[kbd(0, code, flags)])
                }
                Some(WinKey::Vk(vk)) => send(&[kbd(vk, 0, up)]),
                None => Ok(()),
            }
        }

        fn secure_attention(&mut self) -> Result<bool> {
            // Only the service's host (SYSTEM) may ask Windows for it.
            if !crate::win::proc::is_system() {
                return Ok(false);
            }
            crate::win::sas::send()?;
            Ok(true)
        }

        fn caps_lock(&mut self) -> Option<bool> {
            // The toggle bit; this thread has no window, so it is the keyboard's own state.
            Some(unsafe { GetKeyState(VK_CAPITAL as i32) } & 1 != 0)
        }

        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            let (vx, vy, vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            let nx = (((x - vx) as i64 * 65535) / (vw.max(2) - 1) as i64) as i32;
            let ny = (((y - vy) as i64 * 65535) / (vh.max(2) - 1) as i64) as i32;
            send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
        }

        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            let (flags, data) = match (b, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, 1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
            };
            send(&[mouse(0, 0, data, flags)])
        }

        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            const WHEEL_DELTA: i32 = 120;
            let mut v = Vec::new();
            if dy != 0 {
                v.push(mouse(0, 0, dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
            }
            if dx != 0 {
                v.push(mouse(0, 0, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
            }
            send(&v)
        }

        fn text(&mut self, s: &str) -> Result<()> {
            let mut v = Vec::new();
            for unit in s.encode_utf16() {
                v.push(kbd(0, unit, KEYEVENTF_UNICODE));
                v.push(kbd(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            send(&v)
        }

        fn pointer(&mut self) -> Option<(i32, i32)> {
            let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
            (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
        }

        fn block_local(&mut self, on: bool) -> Result<()> {
            // Only this thread may unblock what it blocked; injected input goes on working.
            if unsafe { BlockInput(on as i32) } == 0 {
                anyhow::bail!(
                    "Windows refused ({}): it needs rdesk installed as a service",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// macOS / Linux (enigo)
// ---------------------------------------------------------------------------

/// Phones: no input backend yet (Android's accessibility service comes next).
#[cfg(any(target_os = "android", target_os = "ios"))]
fn platform_backend() -> Result<Box<dyn Backend>> {
    Err(anyhow::anyhow!("remote control of a phone is not available yet"))
}

#[cfg(not(any(windows, target_os = "android", target_os = "ios")))]
fn platform_backend() -> Result<Box<dyn Backend>> {
    #[cfg(target_os = "linux")]
    if crate::wayland::active() {
        return Ok(Box::new(crate::wayland::Input::new()?));
    }
    let enigo = enigo::Enigo::new(&enigo::Settings::default())
        .map_err(|e| anyhow::anyhow!("{e:?} (macOS: grant Accessibility; Linux: needs an X11 session)"))?;
    Ok(Box::new(EnigoBackend {
        enigo,
        #[cfg(target_os = "linux")]
        x11: x11_locks::Locks::open(),
    }))
}

#[cfg(not(any(windows, target_os = "android", target_os = "ios")))]
struct EnigoBackend {
    enigo: enigo::Enigo,
    /// The X server, to ask it for Caps Lock.
    #[cfg(target_os = "linux")]
    x11: Option<x11_locks::Locks>,
}

/// Caps Lock on X11: the Lock modifier the server reports with the pointer.
#[cfg(target_os = "linux")]
mod x11_locks {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask, Window};
    use x11rb::rust_connection::RustConnection;

    pub struct Locks {
        conn: RustConnection,
        root: Window,
    }

    impl Locks {
        pub fn open() -> Option<Self> {
            let (conn, screen) =
                x11rb::connect(None).inspect_err(|e| tracing::debug!("no X server for Caps Lock: {e}")).ok()?;
            let root = conn.setup().roots.get(screen)?.root;
            Some(Self { conn, root })
        }

        pub fn caps(&self) -> Option<bool> {
            let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
            Some(reply.mask.contains(KeyButMask::LOCK))
        }
    }
}

#[cfg(not(any(windows, target_os = "android", target_os = "ios")))]
impl Backend for EnigoBackend {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        use enigo::{Direction, Keyboard};
        #[cfg(target_os = "macos")]
        let code = crate::keymap::macos(key);
        #[cfg(not(target_os = "macos"))]
        let code = crate::keymap::evdev(key).map(|c| c + 8);
        let Some(code) = code else { return Ok(()) };
        let dir = if down { Direction::Press } else { Direction::Release };
        self.enigo.raw(code, dir).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        use enigo::{Coordinate, Mouse};
        self.enigo.move_mouse(x, y, Coordinate::Abs).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        use enigo::{Button, Direction, Mouse};
        let btn = match b {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
            MouseButton::Back => Button::Back,
            MouseButton::Forward => Button::Forward,
        };
        let dir = if down { Direction::Press } else { Direction::Release };
        self.enigo.button(btn, dir).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        use enigo::{Axis, Mouse};
        // enigo: positive = down / right.
        if dy != 0 {
            self.enigo.scroll(-dy, Axis::Vertical).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        if dx != 0 {
            self.enigo.scroll(dx, Axis::Horizontal).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        Ok(())
    }

    fn text(&mut self, s: &str) -> Result<()> {
        use enigo::Keyboard;
        self.enigo.text(s).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    fn pointer(&mut self) -> Option<(i32, i32)> {
        use enigo::Mouse;
        self.enigo.location().ok()
    }

    #[cfg(target_os = "linux")]
    fn caps_lock(&mut self) -> Option<bool> {
        self.x11.as_ref()?.caps()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pointer_maps_back_to_input_coordinates() {
        let r = InputRect { x: 1920, y: -200, w: 2560, h: 1440 };
        for (x, y) in [(0u16, 0u16), (65535, 65535), (32768, 1000), (12345, 54321)] {
            let (px, py) = map_point(r, x, y);
            let p = unmap_point(r, px, py);
            assert!(p.visible);
            assert!(p.x.abs_diff(x) <= 26 && p.y.abs_diff(y) <= 46, "{x},{y} → {px},{py} → {p:?}");
        }
        assert!(!unmap_point(r, 100, 100).visible, "on another monitor");
        assert!(!unmap_point(r, 1920 + 2560, 0).visible, "just past the right edge");
    }

    #[derive(Default, Clone)]
    struct Rec(Arc<Mutex<Vec<String>>>);

    impl Backend for Rec {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("key {key:?} {down}"));
            Ok(())
        }
        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            self.0.lock().unwrap().push(format!("move {x} {y}"));
            Ok(())
        }
        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("btn {b:?} {down}"));
            Ok(())
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.0.lock().unwrap().push(format!("scroll {dx} {dy}"));
            Ok(())
        }
        fn text(&mut self, s: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("text {s}"));
            Ok(())
        }
    }

    /// Sends Ctrl+Alt+Del itself, as the service's host does.
    struct Sas(Rec);

    impl Backend for Sas {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            self.0.key(key, down)
        }
        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            self.0.mouse_abs(x, y)
        }
        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            self.0.button(b, down)
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.0.scroll(dx, dy)
        }
        fn text(&mut self, s: &str) -> Result<()> {
            self.0.text(s)
        }
        fn secure_attention(&mut self) -> Result<bool> {
            self.0.0.lock().unwrap().push("SAS".into());
            Ok(true)
        }
    }

    #[test]
    fn ctrl_alt_del_becomes_the_real_one_where_the_backend_can() {
        let keys = |s: &mut State, b: &mut dyn Backend| {
            for (key, down) in [
                (Key::ControlRight, true),
                (Key::AltLeft, true),
                (Key::Delete, true),
                (Key::Delete, false),
                (Key::AltLeft, false),
                (Key::ControlRight, false),
            ] {
                s.handle(b, InputCmd::Event(InputEvent::Key { key, down })).unwrap();
            }
        };
        let rec = Rec::default();
        keys(&mut State::default(), &mut Sas(rec.clone()));
        let log = rec.0.lock().unwrap().clone();
        assert_eq!(
            log,
            ["key ControlRight true", "key AltLeft true", "SAS", "key AltLeft false", "key ControlRight false"]
        );
        // A backend that cannot just injects the keys.
        let rec = Rec::default();
        keys(&mut State::default(), &mut rec.clone());
        assert_eq!(rec.0.lock().unwrap().len(), 6);
    }

    struct Failing(Arc<Mutex<u32>>);

    impl Backend for Failing {
        fn key(&mut self, _: Key, _: bool) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            anyhow::bail!("SendInput injected 0/1 events")
        }
        fn mouse_abs(&mut self, _: i32, _: i32) -> Result<()> {
            Ok(())
        }
        fn button(&mut self, _: MouseButton, _: bool) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            anyhow::bail!("SendInput injected 0/1 events")
        }
        fn scroll(&mut self, _: i32, _: i32) -> Result<()> {
            Ok(())
        }
        fn text(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failures_are_reported_once_per_interval_and_input_keeps_flowing() {
        let calls = Arc::new(Mutex::new(0));
        let c2 = calls.clone();
        let inj = Injector::spawn_with(move || Box::new(Failing(c2)), false);
        let mut rx = inj.subscribe();
        for _ in 0..3 {
            inj.event(InputEvent::MouseButton { button: MouseButton::Left, down: true });
            inj.event(InputEvent::MouseButton { button: MouseButton::Left, down: false });
        }
        inj.event(InputEvent::Key { key: Key::KeyA, down: true });
        let deadline = Instant::now() + Duration::from_secs(5);
        while *calls.lock().unwrap() < 7 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // The thread survived every failure and handled all seven events…
        assert_eq!(*calls.lock().unwrap(), 7);
        // …but told the user only once.
        let first = rx.try_recv().unwrap();
        assert!(first.contains("Remote input failed on the host"), "{first}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn presses_are_what_we_check() {
        assert!(is_press(&InputCmd::Event(InputEvent::MouseButton { button: MouseButton::Left, down: true })));
        assert!(!is_press(&InputCmd::Event(InputEvent::MouseButton { button: MouseButton::Left, down: false })));
        assert!(is_press(&InputCmd::Event(InputEvent::Key { key: Key::KeyA, down: true })));
        assert!(!is_press(&InputCmd::Event(InputEvent::MouseMove { x: 1, y: 1 })));
    }

    #[test]
    fn maps_corners_onto_monitor_rect() {
        let r = InputRect { x: -1920, y: 0, w: 1920, h: 1080 };
        assert_eq!(map_point(r, 0, 0), (-1920, 0));
        assert_eq!(map_point(r, 65535, 65535), (-1, 1079));
        assert_eq!(map_point(r, 32768, 32768), (-960, 540));
    }

    #[test]
    fn release_all_lifts_everything_modifiers_last() {
        let rec = Rec::default();
        let mut b = rec.clone();
        let mut s = State::default();
        for ev in [
            InputEvent::Key { key: Key::ControlLeft, down: true },
            InputEvent::Key { key: Key::KeyC, down: true },
            InputEvent::MouseButton { button: MouseButton::Left, down: true },
            InputEvent::ReleaseAll,
        ] {
            s.handle(&mut b, InputCmd::Event(ev)).unwrap();
        }
        let log = rec.0.lock().unwrap().clone();
        assert_eq!(log[3..], ["key KeyC false", "key ControlLeft false", "btn Left false"].map(String::from));
        assert!(s.keys.is_empty() && s.buttons.is_empty());
    }

    #[test]
    fn stray_key_up_is_ignored_and_scroll_accumulates() {
        let rec = Rec::default();
        let mut b = rec.clone();
        let mut s = State::default();
        s.handle(&mut b, InputCmd::Event(InputEvent::Key { key: Key::KeyA, down: false })).unwrap();
        for _ in 0..3 {
            s.handle(&mut b, InputCmd::Event(InputEvent::Scroll { dx: 0.0, dy: 0.4 })).unwrap();
        }
        assert_eq!(rec.0.lock().unwrap().clone(), vec!["scroll 0 1".to_string()]);
    }

    /// A keyboard whose Caps Lock a press toggles, like a computer's.
    struct Lockable {
        rec: Rec,
        caps: Option<bool>,
    }

    impl Backend for Lockable {
        fn key(&mut self, key: Key, down: bool) -> Result<()> {
            if key == Key::CapsLock && down {
                self.caps = self.caps.map(|c| !c);
            }
            self.rec.key(key, down)
        }
        fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
            self.rec.mouse_abs(x, y)
        }
        fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
            self.rec.button(b, down)
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.rec.scroll(dx, dy)
        }
        fn text(&mut self, s: &str) -> Result<()> {
            self.rec.text(s)
        }
        fn caps_lock(&mut self) -> Option<bool> {
            self.caps
        }
    }

    #[test]
    fn caps_lock_follows_the_client() {
        let rec = Rec::default();
        let mut b = Lockable { rec: rec.clone(), caps: Some(true) };
        let mut s = State::default();
        // Stuck on here, off at the client: one press turns it off; the same again does nothing.
        s.handle(&mut b, InputCmd::Locks { caps: false }).unwrap();
        s.handle(&mut b, InputCmd::Locks { caps: false }).unwrap();
        assert_eq!(b.caps, Some(false));
        assert_eq!(rec.0.lock().unwrap().clone(), ["key CapsLock true", "key CapsLock false"]);
        s.handle(&mut b, InputCmd::Locks { caps: true }).unwrap();
        assert_eq!(b.caps, Some(true));

        // Held by the client (the keys menu): left alone.
        s.handle(&mut b, InputCmd::Event(InputEvent::Key { key: Key::CapsLock, down: true })).unwrap();
        s.handle(&mut b, InputCmd::Locks { caps: true }).unwrap();
        assert_eq!(rec.0.lock().unwrap().len(), 5);

        // A backend that cannot tell: nothing pressed.
        let rec = Rec::default();
        let mut b = Lockable { rec: rec.clone(), caps: None };
        State::default().handle(&mut b, InputCmd::Locks { caps: true }).unwrap();
        assert!(rec.0.lock().unwrap().is_empty());
    }
}
