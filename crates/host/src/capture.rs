//! Screen capture on a dedicated thread (capture APIs are blocking and some
//! platform handles are not `Send`).
//!
//! Backends: `xcap` (Windows / macOS / X11), the desktop portal on Wayland
//! (`wayland`), nothing yet on phones (they get the test pattern), and a synthetic animated test pattern (`RDESK_FAKE_CAPTURE=1`) for
//! tests and machines without a display.

use std::sync::mpsc;
use std::time::Instant;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
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

enum Req {
    List(oneshot::Sender<Result<Vec<MonitorDesc>>>),
    Grab(u32, oneshot::Sender<Result<Frame>>),
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
                let mut backend: Box<dyn Backend> = if fake { Box::new(Synthetic::new()) } else { platform_backend() };
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
                            let _ = reply.send(backend.grab(id));
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

    pub async fn monitors(&self) -> Result<Vec<MonitorDesc>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::List(tx)).map_err(|_| anyhow!("capture thread gone"))?;
        rx.await?
    }

    /// The pointer's image now (see [`crate::cursor`]); `None` if it cannot be told.
    pub async fn cursor(&self) -> Option<crate::cursor::Current> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::Cursor(tx)).ok()?;
        rx.await.ok().flatten()
    }

    pub async fn grab(&self, id: u32) -> Result<Frame> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Req::Grab(id, tx)).map_err(|_| anyhow!("capture thread gone"))?;
        rx.await?
    }
}

trait Backend {
    fn list(&mut self) -> Result<Vec<MonitorDesc>>;
    fn grab(&mut self, id: u32) -> Result<Frame>;
}

#[cfg(target_os = "linux")]
fn platform_backend() -> Box<dyn Backend> {
    if crate::wayland::active() {
        tracing::info!("Wayland: the screen comes through the desktop portal");
        Box::new(WaylandBackend::default())
    } else {
        Box::new(XcapBackend::default())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "ios")))]
fn platform_backend() -> Box<dyn Backend> {
    Box::new(XcapBackend::default())
}

/// Phones: no screen backend yet (Android's MediaProjection comes next), the test pattern.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn platform_backend() -> Box<dyn Backend> {
    tracing::warn!("this phone cannot share its screen yet: sending the test pattern");
    Box::new(Synthetic::new())
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct WaylandBackend(crate::wayland::Capture);

#[cfg(target_os = "linux")]
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

#[cfg(not(any(target_os = "android", target_os = "ios")))]
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

#[cfg(not(any(target_os = "android", target_os = "ios")))]
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

#[cfg(not(any(target_os = "android", target_os = "ios")))]
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
