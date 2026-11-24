//! Screen capture on a dedicated thread (capture APIs are blocking and some
//! platform handles are not `Send`).
//!
//! Backends: `xcap` (Windows / macOS / X11), the desktop portal on Wayland
//! (`wayland`), one the program around the host brings ([`set_screen_source`]: the phone
//! app's MediaProjection on Android), and a synthetic animated test pattern
//! (`RDESK_FAKE_CAPTURE=1`) for tests and machines without a display.

use std::sync::{Arc, OnceLock, mpsc};
use std::time::Instant;

#[cfg(desktop)]
use anyhow::Context;
use anyhow::{Result, anyhow};
use rdesk_common::tiles::Frame;
use rdesk_proto::MonitorInfo;
use tokio::sync::oneshot;

/// A monitor plus the rectangle input injection must use for it. On macOS
/// that rectangle is in points while capture is in pixels; elsewhere both are pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorDesc {
    pub info: MonitorInfo,
    pub input_rect: InputRect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// How long [`Capturer::monitors`] waits for the capture thread.
const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

enum Req {
    List(oneshot::Sender<Result<Vec<MonitorDesc>>>),
    Grab(u32, oneshot::Sender<Result<Shot>>),
    Cursor(oneshot::Sender<Option<crate::cursor::Current>>),
}

#[derive(Clone)]
pub struct Capturer {
    tx: mpsc::Sender<Req>,
}

impl Capturer {
    pub fn spawn(fake: bool) -> Self {
        let (tx, rx) = mpsc::channel::<Req>();
        std::thread::Builder::new()
            .name("rdesk-capture".into())
            .spawn(move || {
                let mut backend: Box<dyn Backend> = if fake {
                    Box::new(Synthetic::new())
                } else if let Some(make) = SOURCE.get() {
                    make()
                } else {
                    platform_backend()
                };
                let mut cursor = crate::cursor::Reader::new();
                while let Ok(req) = rx.recv() {
                    // Started by the service: the sign-in, lock and UAC screens are another desktop.
                    #[cfg(windows)]
                    if !fake && let Some(d) = crate::win::desktop::follow_input_desktop() {
                        tracing::info!("capturing the {d} desktop");
                    }
                    match req {
                        Req::List(reply) => {
                            let _ = reply.send(backend.list());
                        }
                        Req::Grab(id, reply) => {
                            let _ = reply.send(backend.shot(id));
                        }
                        Req::Cursor(reply) => {
                            let _ = reply.send(if fake { None } else { cursor.read() });
                        }
                    }
                }
            })
            .expect("spawn capture thread");
        Self { tx }
    }

    /// The monitors now. Logins ask for them, so a display server that does not answer (gone,
    /// stuck, a desktop portal waiting for someone at the computer) gives an error after
    /// [`ANSWER_WITHIN`] instead of holding up every session, terminals and files too.
    pub async fn monitors(&self) -> Result<Vec<MonitorDesc>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::List(tx)).map_err(|_| anyhow!("capture thread gone"))?;
        match tokio::time::timeout(ANSWER_WITHIN, rx).await {
            Ok(r) => r?,
            Err(_) => Err(anyhow!("the screen did not answer within {} s", ANSWER_WITHIN.as_secs())),
        }
    }

    /// The pointer's image now (see [`crate::cursor`]); `None` if it cannot be told.
    pub async fn cursor(&self) -> Option<crate::cursor::Current> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::Cursor(tx)).ok()?;
        rx.await.ok().flatten()
    }

    pub async fn grab(&self, id: u32) -> Result<Shot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::Grab(id, tx)).map_err(|_| anyhow!("capture thread gone"))?;
        rx.await?
    }
}

/// A captured picture, and which of its tiles changed when the backend knows it.
pub struct Shot {
    pub frame: Arc<Frame>,
    pub changes: Option<Changes>,
}

/// Which tiles ([`rdesk_proto::TILE`] grid) of a backend's picture changed, so that the video
/// loop compares only those. The backend keeps the picture of `source` and numbers what it
/// reads: `tiles[i]` is the read that last changed tile `i`, `seq` the newest read. Whoever
/// saw read `s` of the same `source` needs to look again only at the tiles past `s`.
#[derive(Clone)]
pub struct Changes {
    pub source: u64,
    pub seq: u64,
    pub tiles: Arc<Vec<u64>>,
}

/// Where the pictures come from. Runs on the capture thread, so it may block (briefly: the
/// video loop waits for [`Backend::grab`]).
pub trait Backend {
    fn list(&mut self) -> Result<Vec<MonitorDesc>>;
    fn grab(&mut self, id: u32) -> Result<Frame>;

    /// [`Backend::grab`], saying what changed where the backend knows it.
    fn shot(&mut self, id: u32) -> Result<Shot> {
        Ok(Shot { frame: Arc::new(self.grab(id)?), changes: None })
    }
}

static SOURCE: OnceLock<fn() -> Box<dyn Backend>> = OnceLock::new();

/// Take the screen from the program the host runs in (the phone app) instead of this
/// system's own capture. Set once, before the host starts; `make` runs on the capture thread.
pub fn set_screen_source(make: fn() -> Box<dyn Backend>) {
    let _ = SOURCE.set(make);
}

/// This computer has no screen to share: built without one (`headless`), or a Linux host
/// started with no display to capture (rdesk-host as a system service on a server). Its
/// sessions get the terminal, files and tunnels ([`rdesk_common::auth::HostAuthConfig::screenless`]).
pub fn screenless() -> bool {
    #[cfg(headless)]
    {
        true
    }
    #[cfg(all(desktop, target_os = "linux"))]
    {
        static NO_DISPLAY: OnceLock<bool> = OnceLock::new();
        *NO_DISPLAY.get_or_init(|| {
            let no_x = std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty());
            let none = no_x && !crate::wayland::active();
            if none {
                tracing::info!("no display (DISPLAY, WAYLAND_DISPLAY): no screen to share, terminal and files only");
            }
            none
        })
    }
    #[cfg(not(any(headless, all(desktop, target_os = "linux"))))]
    {
        false
    }
}

#[cfg(all(desktop, target_os = "linux"))]
fn platform_backend() -> Box<dyn Backend> {
    if screenless() {
        return Box::new(NoScreen);
    }
    if crate::wayland::active() {
        tracing::info!("Wayland: the screen comes through the desktop portal");
        Box::new(WaylandBackend::default())
    } else {
        Box::new(X11Backend::default())
    }
}

/// X11: [`crate::x11`] (MIT-SHM and DAMAGE), or xcap where that cannot read the screen.
#[cfg(all(desktop, target_os = "linux"))]
#[derive(Default)]
struct X11Backend {
    x11: Option<crate::x11::Capture>,
    /// [`crate::x11`] could not start: xcap until then (the X server may come later: a
    /// computer started with no monitor, its desktop started once one is connected).
    xcap_until: Option<std::time::Instant>,
    /// Why it could not, as last said in the log.
    why: String,
    xcap: XcapBackend,
}

/// How long [`X11Backend`] keeps to xcap before trying [`crate::x11`] again.
#[cfg(all(desktop, target_os = "linux"))]
const X11_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(all(desktop, target_os = "linux"))]
impl Backend for X11Backend {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        // The same monitors (RandR outputs) either way; xcap names them.
        self.xcap.list()
    }

    fn grab(&mut self, id: u32) -> Result<Frame> {
        Ok(Arc::unwrap_or_clone(self.shot(id)?.frame))
    }

    fn shot(&mut self, id: u32) -> Result<Shot> {
        if self.x11.is_none() && self.xcap_until.is_none_or(|t| Instant::now() >= t) {
            match crate::x11::Capture::open() {
                Ok(c) => {
                    self.x11 = Some(c);
                    self.why.clear();
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    if why != self.why {
                        tracing::info!("X11: capturing with xcap ({why})");
                        self.why = why;
                    }
                    self.xcap_until = Some(Instant::now() + X11_RETRY);
                }
            }
        }
        let Some(x11) = &mut self.x11 else {
            return Ok(Shot { frame: Arc::new(self.xcap.grab(id)?), changes: None });
        };
        // A failure (the X server restarted, say) starts over with the next capture.
        let shot = x11.grab(id);
        if shot.is_err() {
            self.x11 = None;
        }
        shot
    }
}

#[cfg(all(desktop, not(target_os = "linux")))]
fn platform_backend() -> Box<dyn Backend> {
    Box::new(XcapBackend::default())
}

/// A server without a screen: no monitors, so sessions get the terminal, files and tunnels.
#[cfg(headless)]
fn platform_backend() -> Box<dyn Backend> {
    Box::new(NoScreen)
}

#[cfg(any(headless, all(desktop, target_os = "linux")))]
struct NoScreen;

#[cfg(any(headless, all(desktop, target_os = "linux")))]
impl Backend for NoScreen {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        Ok(Vec::new())
    }
    fn grab(&mut self, _: u32) -> Result<Frame> {
        Err(anyhow!("this computer has no screen"))
    }
}

/// Phones whose app brought no screen ([`set_screen_source`]): the test pattern.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn platform_backend() -> Box<dyn Backend> {
    tracing::warn!("no screen to share: sending the test pattern");
    Box::new(Synthetic::new())
}

#[cfg(all(desktop, target_os = "linux"))]
#[derive(Default)]
struct WaylandBackend(crate::wayland::Capture);

#[cfg(all(desktop, target_os = "linux"))]
impl Backend for WaylandBackend {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        self.0.list()
    }
    fn grab(&mut self, id: u32) -> Result<Frame> {
        self.0.grab(id)
    }
}

/// How a captured picture's pixels are laid out (before RGBA).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pixels {
    Rgbx,
    Bgrx,
    Rgb,
    Bgr,
}

/// Packed rows (`stride` bytes apart; 0 or less: tightly packed) → RGBA, opaque.
/// `None` if `src` is too short for that picture.
pub fn packed_to_rgba(src: &[u8], order: Pixels, width: u32, height: u32, stride: i32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let bpp = match order {
        Pixels::Rgbx | Pixels::Bgrx => 4,
        Pixels::Rgb | Pixels::Bgr => 3,
    };
    let stride = if stride > 0 { stride as usize } else { w * bpp };
    if w == 0 || h == 0 || stride < w * bpp || src.len() < stride * (h - 1) + w * bpp {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
        let line = &src[y * stride..y * stride + w * bpp];
        for (d, s) in row.as_chunks_mut::<4>().0.iter_mut().zip(line.chunks_exact(bpp)) {
            match order {
                Pixels::Rgbx | Pixels::Rgb => d[..3].copy_from_slice(&s[..3]),
                Pixels::Bgrx | Pixels::Bgr => {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }
            d[3] = 255;
        }
    }
    Some(out)
}

#[cfg(desktop)]
#[derive(Default)]
struct XcapBackend {
    monitors: Vec<xcap::Monitor>,
    #[cfg(windows)]
    dxgi: DxgiState,
}

/// Desktop Duplication on Windows ([`crate::win::dxgi`]), with GDI (xcap) as the fallback.
#[cfg(windows)]
#[derive(Default)]
struct DxgiState {
    dup: Option<crate::win::dxgi::Duplication>,
    /// After a failure, GDI until then.
    retry_at: Option<Instant>,
    warned: bool,
}

#[cfg(windows)]
impl DxgiState {
    /// `RDESK_DXGI=0` keeps GDI.
    fn wanted() -> bool {
        std::env::var("RDESK_DXGI").map(|v| v != "0").unwrap_or(true)
    }

    /// The monitor at `rect`, or `None` to use GDI this time.
    fn grab(&mut self, rect: (i32, i32, u32, u32)) -> Option<Frame> {
        use crate::win::dxgi::{Duplication, Lost};
        if !Self::wanted() || self.retry_at.is_some_and(|t| Instant::now() < t) {
            return None;
        }
        for attempt in 0..2 {
            if self.dup.as_ref().is_none_or(|d| d.rect != rect) {
                self.dup = None;
                match Duplication::new(rect) {
                    Ok(d) => self.dup = Some(d),
                    Err(e) => return self.failed(e),
                }
            }
            match self.dup.as_mut().expect("just made").grab(0) {
                Ok(f) => {
                    self.retry_at = None;
                    return Some(f);
                }
                // Another desktop (sign-in, lock, UAC) or a mode change: make it again, once.
                Err(e) if e.downcast_ref::<Lost>().is_some() && attempt == 0 => self.dup = None,
                Err(e) => return self.failed(e),
            }
        }
        None
    }

    fn failed(&mut self, e: anyhow::Error) -> Option<Frame> {
        if !self.warned {
            tracing::info!("desktop duplication unavailable ({e:#}); capturing with GDI");
            self.warned = true;
        }
        self.dup = None;
        self.retry_at = Some(Instant::now() + std::time::Duration::from_secs(5));
        None
    }
}

#[cfg(desktop)]
impl XcapBackend {
    fn refresh(&mut self) -> Result<()> {
        self.monitors = xcap::Monitor::all().context("listing monitors")?;
        Ok(())
    }

    fn find(&mut self, id: u32) -> Result<&xcap::Monitor> {
        if !self.monitors.iter().any(|m| m.id().ok() == Some(id)) {
            self.refresh()?;
        }
        self.monitors.iter().find(|m| m.id().ok() == Some(id)).ok_or_else(|| anyhow!("monitor {id} not found"))
    }
}

#[cfg(desktop)]
impl Backend for XcapBackend {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        self.refresh()?;
        let mut out = Vec::new();
        for m in &self.monitors {
            let input_rect = InputRect { x: m.x()?, y: m.y()?, w: m.width()?, h: m.height()? };
            let scale = m.scale_factor().unwrap_or(1.0);
            // Capture size in pixels: on macOS the rect is in points.
            let (pw, ph) = if cfg!(target_os = "macos") {
                ((input_rect.w as f32 * scale).round() as u32, (input_rect.h as f32 * scale).round() as u32)
            } else {
                (input_rect.w, input_rect.h)
            };
            out.push(MonitorDesc {
                info: MonitorInfo {
                    id: m.id()?,
                    name: m.friendly_name().or_else(|_| m.name()).unwrap_or_default(),
                    x: input_rect.x,
                    y: input_rect.y,
                    width: pw,
                    height: ph,
                    scale,
                    primary: m.is_primary().unwrap_or(false),
                },
                input_rect,
            });
        }
        if out.is_empty() {
            return Err(anyhow!("no monitors found"));
        }
        Ok(out)
    }

    fn grab(&mut self, id: u32) -> Result<Frame> {
        #[cfg(windows)]
        {
            let m = self.find(id)?;
            let rect = (m.x()?, m.y()?, m.width()?, m.height()?);
            if let Some(f) = self.dxgi.grab(rect) {
                return Ok(f);
            }
        }
        let img = match self.find(id)?.capture_image() {
            Ok(img) => img,
            Err(e) => {
                // Display configuration may have changed (resolution, hot-plug): retry once.
                self.refresh()?;
                self.find(id)?.capture_image().map_err(|_| anyhow!("capturing monitor {id}: {e}"))?
            }
        };
        let (width, height) = (img.width(), img.height());
        Ok(Frame { width, height, data: img.into_raw() })
    }
}

/// Moving gradient + bouncing box; two monitors.
struct Synthetic {
    start: Instant,
}

impl Synthetic {
    fn new() -> Self {
        Self { start: Instant::now() }
    }
}

impl Backend for Synthetic {
    fn list(&mut self) -> Result<Vec<MonitorDesc>> {
        let mk = |id: u32, x: i32, w: u32, h: u32, primary| MonitorDesc {
            info: MonitorInfo {
                id,
                name: format!("Synthetic {id}"),
                x,
                y: 0,
                width: w,
                height: h,
                scale: 1.0,
                primary,
            },
            input_rect: InputRect { x, y: 0, w, h },
        };
        Ok(vec![mk(1, 0, 1280, 720, true), mk(2, 1280, 800, 600, false)])
    }

    fn grab(&mut self, id: u32) -> Result<Frame> {
        let (w, h) = match id {
            1 => (1280, 720),
            2 => (800, 600),
            _ => return Err(anyhow!("monitor {id} not found")),
        };
        let t = self.start.elapsed().as_millis() as u32 / 33;
        let mut f = Frame::new(w, h);
        let bx = (t * 7) % (w - 100);
        let by = (t * 5) % (h - 100);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let inside = x >= bx && x < bx + 100 && y >= by && y < by + 100;
                if inside {
                    f.data[i..i + 4].copy_from_slice(&[255, 80, 0, 255]);
                } else {
                    f.data[i] = (x * 255 / w) as u8;
                    f.data[i + 1] = (y * 255 / h) as u8;
                    f.data[i + 2] = (id * 100) as u8;
                    f.data[i + 3] = 255;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_pixels_become_rgba() {
        // 2×2 BGRx with 4 bytes of padding per row.
        let src = [3, 2, 1, 0, 6, 5, 4, 0, 9, 9, 9, 9, 13, 12, 11, 0, 16, 15, 14, 0, 9, 9, 9, 9];
        let out = packed_to_rgba(&src, Pixels::Bgrx, 2, 2, 12).unwrap();
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255, 11, 12, 13, 255, 14, 15, 16, 255]);
        // RGB, tightly packed.
        assert_eq!(packed_to_rgba(&[1, 2, 3], Pixels::Rgb, 1, 1, 0).unwrap(), [1, 2, 3, 255]);
        // Too short.
        assert!(packed_to_rgba(&[1, 2, 3], Pixels::Rgbx, 1, 1, 0).is_none());
    }
}
