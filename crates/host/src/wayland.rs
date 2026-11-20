//! Linux on Wayland, where a program may neither read the screen nor fake input: both go
//! through the desktop portal (xdg-desktop-portal, as GNOME and KDE provide it).
//!
//! One RemoteDesktop session with ScreenCast in it: the portal asks the person at the
//! computer once, then hands over a PipeWire stream per monitor (the picture) and takes
//! keyboard and pointer events for those monitors. The permission is kept with a restore
//! token in the host's folder, so later sessions start without asking (where the desktop
//! supports it: GNOME 46+, KDE Plasma 6.1+).

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, NotifyKeyboardKeycodeOptions, NotifyKeyboardKeysymOptions,
    NotifyPointerAxisDiscreteOptions, NotifyPointerButtonOptions, NotifyPointerMotionAbsoluteOptions, RemoteDesktop,
    SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{CreateSessionOptions, Session};
use rdesk_common::tiles::Frame;
use rdesk_proto::{Key, MonitorInfo, MouseButton};

use crate::capture::{InputRect, MonitorDesc, Pixels, packed_to_rgba};

/// Is this a Wayland session? (`RDESK_WAYLAND=0` forces X11, `=1` Wayland.)
pub fn active() -> bool {
    match std::env::var("RDESK_WAYLAND").as_deref() {
        Ok("0") => return false,
        Ok("1") => return true,
        _ => {}
    }
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// One monitor shared through the portal, in the desktop's (logical) coordinates.
#[derive(Debug, Clone, Copy)]
struct Shared {
    node: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// The portal session (both capture and input use it).
pub struct Portal {
    remote: RemoteDesktop,
    session: Session<RemoteDesktop>,
    screens: Vec<Shared>,
    /// PipeWire, as the portal opened it for this session.
    pipewire: OwnedFd,
}

fn token_file() -> Result<PathBuf> {
    Ok(rdesk_common::config::host_dir()?.join("wayland-token"))
}

static PORTAL: Mutex<Option<Arc<Portal>>> = Mutex::new(None);
static LAST_TRY: Mutex<Option<Instant>> = Mutex::new(None);

/// The portal session, started (and asked for) on first use.
pub fn portal() -> Result<Arc<Portal>> {
    let mut slot = PORTAL.lock().unwrap();
    if let Some(p) = &*slot {
        return Ok(p.clone());
    }
    // Refused or failed: do not ask again at every frame.
    {
        let mut last = LAST_TRY.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(20)) {
            bail!("the desktop did not allow sharing the screen (waiting before asking again)");
        }
        *last = Some(Instant::now());
    }
    let p = Arc::new(pollster::block_on(start())?);
    *slot = Some(p.clone());
    Ok(p)
}

/// Forget the session (it ended): the next use starts another.
fn reset() {
    *PORTAL.lock().unwrap() = None;
    CAPTURES.lock().unwrap().clear();
}

async fn start() -> Result<Portal> {
    let remote = RemoteDesktop::new().await.context("no desktop portal (xdg-desktop-portal) for remote desktop")?;
    let cast = Screencast::new().await.context("no desktop portal (xdg-desktop-portal) for screen sharing")?;
    let session = remote.create_session(CreateSessionOptions::default()).await.context("starting a portal session")?;
    let token = token_file().ok().and_then(|f| std::fs::read_to_string(f).ok());
    remote
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref().map(str::trim)),
        )
        .await
        .context("asking for keyboard and pointer")?;
    let cursor = cast.available_cursor_modes().await.unwrap_or(CursorMode::HIDDEN);
    let cursor = if cursor.contains(CursorMode::EMBEDDED) { CursorMode::EMBEDDED } else { CursorMode::HIDDEN };
    cast.select_sources(
        &session,
        SelectSourcesOptions::default().set_sources(SourceType::MONITOR).set_multiple(true).set_cursor_mode(cursor),
    )
    .await
    .context("asking for the screens")?;
    tracing::info!("asking the desktop to share the screen (it may show a dialog on this computer)");
    let started = remote
        .start(&session, None, StartOptions::default())
        .await
        .context("starting the portal session")?
        .response()
        .map_err(|e| anyhow!("the desktop did not allow sharing the screen and input ({e})"))?;
    if let (Some(t), Ok(f)) = (started.restore_token(), token_file()) {
        // Single use: each session hands out the next one.
        let _ = rdesk_common::config::ensure_host_dir().and_then(|_| Ok(std::fs::write(f, t)?));
    }
    let screens: Vec<Shared> = started
        .streams()
        .iter()
        .map(|s| {
            let (x, y) = s.position().unwrap_or((0, 0));
            let (w, h) = s.size().unwrap_or((0, 0));
            Shared { node: s.pipe_wire_node_id(), x, y, w, h }
        })
        .collect();
    if screens.is_empty() {
        bail!("the desktop shared no screen");
    }
    if !started.devices().contains(DeviceType::POINTER) {
        tracing::warn!("the desktop shared the screen without input: view only");
    }
    let pipewire = cast.open_pipe_wire_remote(&session, Default::default()).await.context("opening PipeWire")?;
    tracing::info!("sharing {} screen(s) through the desktop portal", screens.len());
    Ok(Portal { remote, session, screens, pipewire })
}

// ---- the picture ----------------------------------------------------------------

/// The latest frame of one monitor, filled by its PipeWire thread.
#[derive(Default)]
struct Latest {
    frame: Mutex<(Option<Arc<Frame>>, u64)>,
    new: Condvar,
    ended: std::sync::atomic::AtomicBool,
}

static CAPTURES: Mutex<Vec<(u32, Arc<Latest>)>> = Mutex::new(Vec::new());

fn capture_of(p: &Portal, node: u32) -> Result<Arc<Latest>> {
    let mut all = CAPTURES.lock().unwrap();
    if let Some((_, l)) = all.iter().find(|(n, l)| *n == node && !l.ended.load(std::sync::atomic::Ordering::Relaxed)) {
        return Ok(l.clone());
    }
    all.retain(|(n, _)| *n != node);
    let latest = Arc::new(Latest::default());
    let fd = p.pipewire.try_clone().context("sharing the PipeWire connection")?;
    let l2 = latest.clone();
    std::thread::Builder::new().name(format!("rdesk-pw-{node}")).spawn(move || {
        if let Err(e) = pipewire_thread(fd, node, &l2) {
            tracing::warn!("screen stream {node}: {e:#}");
        }
        l2.ended.store(true, std::sync::atomic::Ordering::Relaxed);
        l2.new.notify_all();
    })?;
    all.push((node, latest.clone()));
    Ok(latest)
}

fn pipewire_thread(fd: OwnedFd, node: u32, latest: &Arc<Latest>) -> Result<()> {
    use pipewire::context::ContextRc;
    use pipewire::main_loop::MainLoopRc;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::format_utils;
    use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
    use pipewire::spa::pod::{self, Pod, serialize::PodSerializer};
    use pipewire::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
    use pipewire::stream::{StreamFlags, StreamRc};

    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(pipewire::init);
    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = StreamRc::new(
        core.clone(),
        "rdesk",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let sink = latest.clone();
    let quit = main_loop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .state_changed(move |_, _, _, new| {
            if matches!(new, pipewire::stream::StreamState::Error(_) | pipewire::stream::StreamState::Unconnected) {
                quit.quit();
            }
        })
        .param_changed(|_, format, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            if let Ok((MediaType::Video, MediaSubtype::Raw)) = format_utils::parse_format(param) {
                let _ = format.parse(param);
            }
        })
        .process(move |stream, format| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, stride) = (d.chunk().offset() as usize, d.chunk().stride());
            let size = format.size();
            let order = match format.format() {
                VideoFormat::BGRx | VideoFormat::BGRA => Pixels::Bgrx,
                VideoFormat::RGBx | VideoFormat::RGBA => Pixels::Rgbx,
                VideoFormat::RGB => Pixels::Rgb,
                VideoFormat::BGR => Pixels::Bgr,
                _ => return,
            };
            let Some(data) = d.data() else { return };
            let Some(rgba) = packed_to_rgba(&data[offset.min(data.len())..], order, size.width, size.height, stride)
            else {
                return;
            };
            let mut slot = sink.frame.lock().unwrap();
            slot.0 = Some(Arc::new(Frame { width: size.width, height: size.height, data: rgba }));
            slot.1 += 1;
            sink.new.notify_all();
        })
        .register()?;

    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::RGB,
            VideoFormat::BGR
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 16384, height: 16384 }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
    );
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pod::Value::Object(obj))
        .map_err(|e| anyhow!("{e:?}"))?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&bytes).context("building the video format")?];
    stream.connect(Direction::Input, Some(node), StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS, &mut params)?;
    main_loop.run();
    Ok(())
}

/// Capture backend: the portal's monitors.
#[derive(Default)]
pub struct Capture {
    /// Which frame each monitor last returned.
    seen: Vec<(u32, u64)>,
}

impl Capture {
    pub fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        let p = portal()?;
        let mut out = Vec::new();
        for (i, s) in p.screens.iter().enumerate() {
            // The picture's size in pixels (scaled screens), once a frame came.
            let (pw, ph) = capture_of(&p, s.node)
                .ok()
                .and_then(|l| l.frame.lock().unwrap().0.as_ref().map(|f| (f.width, f.height)))
                .unwrap_or((s.w.max(1) as u32, s.h.max(1) as u32));
            out.push(MonitorDesc {
                info: MonitorInfo {
                    id: s.node,
                    name: format!("Screen {}", i + 1),
                    x: s.x,
                    y: s.y,
                    width: pw,
                    height: ph,
                    scale: if s.w > 0 { pw as f32 / s.w as f32 } else { 1.0 },
                    primary: i == 0,
                },
                input_rect: InputRect { x: s.x, y: s.y, w: s.w.max(1) as u32, h: s.h.max(1) as u32 },
            });
        }
        Ok(out)
    }

    /// The newest frame (waiting a little for one newer than the last returned).
    pub fn grab(&mut self, id: u32) -> Result<Frame> {
        let p = portal()?;
        if !p.screens.iter().any(|s| s.node == id) {
            bail!("monitor {id} not found");
        }
        let latest = capture_of(&p, id)?;
        let seen = self.seen.iter().find(|(n, _)| *n == id).map(|s| s.1).unwrap_or(0);
        let deadline = Instant::now() + Duration::from_millis(if seen == 0 { 5000 } else { 100 });
        let mut slot = latest.frame.lock().unwrap();
        while slot.1 == seen && Instant::now() < deadline {
            if latest.ended.load(std::sync::atomic::Ordering::Relaxed) {
                drop(slot);
                reset();
                bail!("the screen sharing ended");
            }
            slot = latest.new.wait_timeout(slot, deadline - Instant::now()).unwrap().0;
        }
        let Some(frame) = slot.0.clone() else { bail!("no picture from the desktop yet") };
        let count = slot.1;
        drop(slot);
        self.seen.retain(|(n, _)| *n != id);
        self.seen.push((id, count));
        Ok(Arc::unwrap_or_clone(frame))
    }
}

// ---- input --------------------------------------------------------------------

/// Input backend: events through the portal.
pub struct Input {
    portal: Arc<Portal>,
}

impl Input {
    pub fn new() -> Result<Self> {
        Ok(Self { portal: portal()? })
    }

    fn call(&mut self, f: impl AsyncFnOnce(&Portal) -> ashpd::Result<()>) -> Result<()> {
        let p = self.portal.clone();
        match pollster::block_on(f(&p)) {
            Ok(()) => Ok(()),
            Err(e) => {
                // The session may have ended (the user stopped sharing): start again next time.
                reset();
                if let Ok(np) = portal() {
                    self.portal = np;
                }
                Err(anyhow!("desktop portal: {e}"))
            }
        }
    }
}

/// Evdev pointer buttons.
fn button_code(b: MouseButton) -> i32 {
    match b {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
    }
}

/// The X keysym that types `c`.
pub fn keysym(c: char) -> i32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as i32,
        c => 0x0100_0000 + c as i32,
    }
}

fn state(down: bool) -> KeyState {
    if down { KeyState::Pressed } else { KeyState::Released }
}

impl crate::input::Backend for Input {
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        let Some(code) = crate::keymap::evdev(key) else { return Ok(()) };
        self.call(async |p| {
            p.remote
                .notify_keyboard_keycode(&p.session, code as i32, state(down), NotifyKeyboardKeycodeOptions::default())
                .await
        })
    }

    fn mouse_abs(&mut self, x: i32, y: i32) -> Result<()> {
        // Which shared screen the point is on, and where on it.
        let screens = self.portal.screens.clone();
        let Some(s) =
            screens.iter().find(|s| x >= s.x && y >= s.y && x < s.x + s.w && y < s.y + s.h).or(screens.first())
        else {
            return Ok(());
        };
        let (rx, ry) = ((x - s.x).clamp(0, s.w.max(1) - 1) as f64, (y - s.y).clamp(0, s.h.max(1) - 1) as f64);
        let node = s.node;
        self.call(async |p| {
            p.remote
                .notify_pointer_motion_absolute(&p.session, node, rx, ry, NotifyPointerMotionAbsoluteOptions::default())
                .await
        })
    }

    fn button(&mut self, b: MouseButton, down: bool) -> Result<()> {
        self.call(async |p| {
            p.remote
                .notify_pointer_button(&p.session, button_code(b), state(down), NotifyPointerButtonOptions::default())
                .await
        })
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        // Positive steps scroll down / right.
        if dy != 0 {
            self.call(async |p| {
                p.remote
                    .notify_pointer_axis_discrete(
                        &p.session,
                        Axis::Vertical,
                        -dy,
                        NotifyPointerAxisDiscreteOptions::default(),
                    )
                    .await
            })?;
        }
        if dx != 0 {
            self.call(async |p| {
                p.remote
                    .notify_pointer_axis_discrete(
                        &p.session,
                        Axis::Horizontal,
                        dx,
                        NotifyPointerAxisDiscreteOptions::default(),
                    )
                    .await
            })?;
        }
        Ok(())
    }

    fn text(&mut self, s: &str) -> Result<()> {
        for c in s.chars() {
            let sym = keysym(c);
            for down in [true, false] {
                self.call(async |p| {
                    p.remote
                        .notify_keyboard_keysym(&p.session, sym, state(down), NotifyKeyboardKeysymOptions::default())
                        .await
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(keysym('a'), 0x61);
        assert_eq!(keysym('ç'), 0xe7);
        assert_eq!(keysym('€'), 0x0100_20ac);
        assert_eq!(keysym('\n'), 0xff0d);
    }
}
