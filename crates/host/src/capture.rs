//! Screen capture on a dedicated thread (capture APIs are blocking and some
//! platform handles are not `Send`).
//!
//! Backends: `xcap` (Windows / macOS / X11) and a synthetic animated test
//! pattern (`RDESK_FAKE_CAPTURE=1`) for tests and machines without a display.

use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
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
                let mut backend: Box<dyn Backend> =
                    if fake { Box::new(Synthetic::new()) } else { Box::new(XcapBackend::default()) };
                while let Ok(req) = rx.recv() {
                    match req {
                        Req::List(reply) => {
                            let _ = reply.send(backend.list());
                        }
                        Req::Grab(id, reply) => {
                            let _ = reply.send(backend.grab(id));
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

#[derive(Default)]
struct XcapBackend {
    monitors: Vec<xcap::Monitor>,
}

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
