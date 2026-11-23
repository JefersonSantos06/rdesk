//! Screen capture on X11, the way the X11 remote desktops do it (x11vnc and others): one
//! connection kept open, MIT-SHM (the server writes the picture straight into memory shared
//! with us instead of through the socket) and DAMAGE (the server says what was drawn, so a
//! still screen is neither read again nor compared tile by tile). Without MIT-SHM the core
//! `GetImage` reads the same areas; without DAMAGE every capture reads everything.
//!
//! Each monitor keeps its picture; a capture reads only the tiles drawn on since the last one
//! and says which they were ([`Changes`]), so the video loop hashes only those. DAMAGE is
//! trusted, but every [`FULL_EVERY`] the whole monitor is read anyway: what a driver failed
//! to report shows up then.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use rdesk_common::tiles::{self, Frame};
use rdesk_proto::TILE;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _, ReportLevel};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

use crate::capture::{Changes, Shot};

/// The whole monitor is read this often, whatever DAMAGE says.
const FULL_EVERY: Duration = Duration::from_secs(2);
/// The monitors' places are asked for this often (and whenever one is not known).
const LIST_EVERY: Duration = Duration::from_secs(2);

/// A rectangle of the X screen (root window coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: i16,
    y: i16,
    w: u16,
    h: u16,
}

/// How the server's 32-bit pixels are laid out.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// Little-endian pixels (`LSBFirst`): B, G, R, X in memory.
    le: bool,
    /// Red in the low byte (blue in the high one).
    swap: bool,
}

pub struct Capture {
    conn: RustConnection,
    root: Window,
    layout: Layout,
    shm: Option<Segment>,
    damage: Option<(damage::Damage, xfixes::Region)>,
    /// RandR output (the monitor's id, as xcap gives it) and where it is.
    monitors: Vec<(u32, Rect)>,
    listed_at: Instant,
    screens: Vec<Mon>,
    /// Counts the reads; a tile's entry in [`Changes::tiles`] is the read that last changed it.
    seq: u64,
    /// Numbers the monitors' pictures ([`Changes::source`]).
    sources: u64,
}

/// A monitor being captured: its picture, kept up to date.
struct Mon {
    id: u32,
    rect: Rect,
    source: u64,
    cols: u32,
    rows: u32,
    frame: Arc<Frame>,
    /// Per tile, the read ([`Capture::seq`]) that last changed it.
    changed: Arc<Vec<u64>>,
    /// Per tile: drawn on since it was last read.
    pending: Vec<bool>,
    full_at: Instant,
}

impl Mon {
    fn new(id: u32, rect: Rect, source: u64) -> Self {
        let (cols, rows) = tiles::grid(rect.w as u32, rect.h as u32);
        let n = (cols * rows) as usize;
        Self {
            id,
            rect,
            source,
            cols,
            rows,
            frame: Arc::new(Frame::new(rect.w as u32, rect.h as u32)),
            changed: Arc::new(vec![0; n]),
            pending: vec![true; n],
            full_at: Instant::now() + FULL_EVERY,
        }
    }

    /// Something was drawn at (`x`, `y`, `w`, `h`) on the X screen.
    fn damage(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (rx, ry) = (self.rect.x as i32, self.rect.y as i32);
        let x0 = (x - rx).max(0);
        let y0 = (y - ry).max(0);
        let x1 = (x + w - rx).min(self.rect.w as i32);
        let y1 = (y + h - ry).min(self.rect.h as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let t = TILE as i32;
        for row in y0 / t..=(y1 - 1) / t {
            for col in x0 / t..=(x1 - 1) / t {
                self.pending[(row as u32 * self.cols + col as u32) as usize] = true;
            }
        }
    }
}

/// An area to read: tile rows `r0..=r1`, tile columns `c0..=c1` (clipped to the picture).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Band {
    r0: u32,
    r1: u32,
    c0: u32,
    c1: u32,
}

impl Band {
    /// In pixels of a `width`×`height` picture: x, y, w, h.
    fn px(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let (x, y) = (self.c0 * TILE, self.r0 * TILE);
        (x, y, ((self.c1 + 1) * TILE).min(width) - x, ((self.r1 + 1) * TILE).min(height) - y)
    }
}

/// The areas covering the `pending` tiles: per tile row, from its first pending tile to its
/// last, and rows alike in one area. Few requests, at the cost of reading some tiles between.
fn bands(pending: &[bool], cols: u32, rows: u32) -> Vec<Band> {
    let mut out: Vec<Band> = Vec::new();
    for r in 0..rows {
        let row = &pending[(r * cols) as usize..((r + 1) * cols) as usize];
        let (Some(c0), Some(c1)) = (row.iter().position(|&p| p), row.iter().rposition(|&p| p)) else { continue };
        let (c0, c1) = (c0 as u32, c1 as u32);
        match out.last_mut() {
            Some(b) if b.r1 + 1 == r && b.c0 == c0 && b.c1 == c1 => b.r1 = r,
            _ => out.push(Band { r0: r, r1: r, c0, c1 }),
        }
    }
    out
}

/// The server's pixels (`w` per row, tightly packed) into the RGBA picture at (`x`, `y`).
fn put(frame: &mut Frame, (x, y, w, h): (u32, u32, u32, u32), src: &[u8], layout: Layout) {
    match (layout.le, layout.swap) {
        (true, false) => put_as::<true, false>(frame, x, y, w, h, src),
        (true, true) => put_as::<true, true>(frame, x, y, w, h, src),
        (false, false) => put_as::<false, false>(frame, x, y, w, h, src),
        (false, true) => put_as::<false, true>(frame, x, y, w, h, src),
    }
}

/// [`put`] for one layout, so that the loop has no branch in it (and vectorizes).
fn put_as<const LE: bool, const SWAP: bool>(frame: &mut Frame, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
    let (fw, w) = (frame.width as usize, w as usize);
    for (row, line) in src.chunks_exact(w * 4).take(h as usize).enumerate() {
        let at = ((y as usize + row) * fw + x as usize) * 4;
        let dst = &mut frame.data[at..at + w * 4];
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(line.as_chunks::<4>().0) {
            let p = if LE { u32::from_le_bytes(*s) } else { u32::from_be_bytes(*s) };
            let (r, b) = if SWAP { (p, p >> 16) } else { (p >> 16, p) };
            *d = [r as u8, (p >> 8) as u8, b as u8, 255];
        }
    }
}

/// A System V shared memory segment the X server writes pictures into.
struct Segment {
    id: shm::Seg,
    addr: *mut u8,
    size: usize,
}

impl Segment {
    fn new(conn: &RustConnection, size: usize) -> Result<Self> {
        // SAFETY: plain System V calls; every result is checked before it is used.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        ensure!(shmid >= 0, "shmget: {}", std::io::Error::last_os_error());
        // SAFETY: as above.
        let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if addr as isize == -1 {
            let e = std::io::Error::last_os_error();
            // SAFETY: the segment just made, not attached anywhere.
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            bail!("shmat: {e}");
        }
        let id = conn.generate_id()?;
        let attached = conn
            .shm_attach(id, shmid as u32, false)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from));
        // Removed once both sides let go of it (also if this process dies).
        // SAFETY: as above.
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        let seg = Self { id, addr: addr.cast(), size };
        attached.context("the X server attaching shared memory")?;
        Ok(seg)
    }

    /// `len` bytes at `offset`, written by the server (its reply came: it is done).
    fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset + len <= self.size);
        // SAFETY: inside the segment (checked), which stays attached while `self` lives.
        unsafe { std::slice::from_raw_parts(self.addr.add(offset), len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: attached in `new`, detached once.
        unsafe { libc::shmdt(self.addr.cast()) };
    }
}

impl Capture {
    /// Connect to the X server (`DISPLAY`). Fails where this backend cannot read the screen
    /// (no RandR 1.5, a depth other than 24 or 32 bits); MIT-SHM and DAMAGE are optional.
    pub fn open() -> Result<Self> {
        let (conn, n) = x11rb::connect(None).context("connecting to the X server")?;
        let setup = conn.setup();
        let screen = setup.roots.get(n).ok_or_else(|| anyhow!("no X screen {n}"))?;
        let (root, depth) = (screen.root, screen.root_depth);
        ensure!(depth == 24 || depth == 32, "a {depth}-bit screen");
        let bpp = setup.pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel);
        ensure!(bpp == Some(32), "{depth}-bit pixels stored in {bpp:?} bits");
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == screen.root_visual)
            .ok_or_else(|| anyhow!("the screen's visual is not listed"))?;
        let swap = match (visual.red_mask, visual.green_mask, visual.blue_mask) {
            (0xff0000, 0xff00, 0xff) => false,
            (0xff, 0xff00, 0xff0000) => true,
            m => bail!("pixels laid out as {m:x?}"),
        };
        let layout = Layout { le: setup.image_byte_order == ImageOrder::LSB_FIRST, swap };

        ensure!(conn.extension_information(randr::X11_EXTENSION_NAME)?.is_some(), "no RandR");
        conn.randr_query_version(1, 5)?.reply().context("RandR")?;
        let damage = Self::start_damage(&conn, root)
            .inspect_err(|e| tracing::info!("X11: no DAMAGE ({e:#}): every capture reads the whole screen"))
            .ok();
        let shm = Self::start_shm(&conn, screen.width_in_pixels, screen.height_in_pixels)
            .inspect_err(|e| tracing::info!("X11: no MIT-SHM ({e:#}): pictures come through the socket"))
            .ok();
        tracing::info!("X11 capture: DAMAGE {}, MIT-SHM {}", damage.is_some(), shm.is_some());
        let mut me = Self {
            conn,
            root,
            layout,
            shm,
            damage,
            monitors: Vec::new(),
            listed_at: Instant::now(),
            screens: Vec::new(),
            seq: 0,
            sources: 0,
        };
        me.list()?;
        Ok(me)
    }

    fn start_damage(conn: &RustConnection, root: Window) -> Result<(damage::Damage, xfixes::Region)> {
        ensure!(conn.extension_information(xfixes::X11_EXTENSION_NAME)?.is_some(), "no XFIXES");
        ensure!(conn.extension_information(damage::X11_EXTENSION_NAME)?.is_some(), "not offered");
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;
        let (d, region) = (conn.generate_id()?, conn.generate_id()?);
        conn.xfixes_create_region(region, &[])?.check()?;
        // One event when there is something to fetch; the captures fetch it anyway.
        conn.damage_create(d, root, ReportLevel::NON_EMPTY)?.check()?;
        Ok((d, region))
    }

    fn start_shm(conn: &RustConnection, w: u16, h: u16) -> Result<Segment> {
        ensure!(conn.extension_information(shm::X11_EXTENSION_NAME)?.is_some(), "not offered");
        conn.shm_query_version()?.reply()?;
        Segment::new(conn, w as usize * h as usize * 4)
    }

    /// Where the monitors are now.
    fn list(&mut self) -> Result<()> {
        let reply = self.conn.randr_get_monitors(self.root, true)?.reply().context("listing the monitors")?;
        self.monitors = reply
            .monitors
            .iter()
            .flat_map(|m| {
                let rect = Rect { x: m.x, y: m.y, w: m.width, h: m.height };
                m.outputs.iter().map(move |&o| (o, rect))
            })
            .collect();
        self.listed_at = Instant::now();
        Ok(())
    }

    /// What was drawn since the last time, to every monitor kept.
    fn fetch_damage(&mut self) -> Result<()> {
        let Some((d, region)) = self.damage else { return Ok(()) };
        // The events only say there is damage; it is fetched below anyway.
        while self.conn.poll_for_event()?.is_some() {}
        self.conn.damage_subtract(d, x11rb::NONE, region)?;
        let reply = self.conn.xfixes_fetch_region(region)?.reply().context("fetching the damage")?;
        for r in &reply.rectangles {
            for m in &mut self.screens {
                m.damage(r.x as i32, r.y as i32, r.width as i32, r.height as i32);
            }
        }
        Ok(())
    }

    /// Monitor `id`'s picture, with the tiles changed since earlier captures.
    pub fn grab(&mut self, id: u32) -> Result<Shot> {
        let now = Instant::now();
        if now >= self.listed_at + LIST_EVERY || !self.monitors.iter().any(|m| m.0 == id) {
            self.list()?;
        }
        let rect =
            self.monitors.iter().find(|m| m.0 == id).map(|m| m.1).ok_or_else(|| anyhow!("monitor {id} not found"))?;
        ensure!(rect.w > 0 && rect.h > 0, "monitor {id} has no size");
        self.fetch_damage()?;
        let i = match self.screens.iter().position(|m| m.id == id) {
            Some(i) if self.screens[i].rect == rect => i,
            found => {
                // New, or moved or resized: start over.
                if let Some(i) = found {
                    self.screens.remove(i);
                }
                self.sources += 1;
                self.screens.push(Mon::new(id, rect, self.sources));
                self.screens.len() - 1
            }
        };
        let mon = &mut self.screens[i];
        if self.damage.is_none() || now >= mon.full_at {
            mon.pending.fill(true);
            mon.full_at = now + FULL_EVERY;
        }
        let bands = bands(&mon.pending, mon.cols, mon.rows);
        if !bands.is_empty() {
            self.seq += 1;
            self.read(i, &bands)?;
        }
        let mon = &self.screens[i];
        Ok(Shot {
            frame: mon.frame.clone(),
            changes: Some(Changes { source: mon.source, seq: self.seq, tiles: mon.changed.clone() }),
        })
    }

    /// Read `bands` of monitor `i` into its picture: all the requests at once, then the replies.
    fn read(&mut self, i: usize, bands: &[Band]) -> Result<()> {
        let Self { conn, root, layout, shm, screens, seq, .. } = self;
        let mon = &mut screens[i];
        let (fw, fh) = (mon.rect.w as u32, mon.rect.h as u32);
        let need = fw as usize * fh as usize * 4;
        if shm.as_ref().is_some_and(|s| s.size < need) {
            // The screen grew past the segment: a bigger one.
            if let Some(old) = shm.take() {
                let _ = conn.shm_detach(old.id);
            }
            *shm = Segment::new(conn, need)
                .inspect_err(|e| tracing::info!("X11: no MIT-SHM any more ({e:#}): pictures come through the socket"))
                .ok();
        }
        let areas: Vec<_> = bands.iter().map(|b| b.px(fw, fh)).collect();
        let (mx, my) = (mon.rect.x, mon.rect.y);
        let at = |(x, y): (u32, u32)| (mx + x as i16, my + y as i16);
        let format = ImageFormat::Z_PIXMAP;
        if let Some(seg) = shm.as_ref() {
            let mut cookies = Vec::with_capacity(areas.len());
            let mut offset = 0usize;
            for &(x, y, w, h) in &areas {
                let (sx, sy) = at((x, y));
                let c =
                    conn.shm_get_image(*root, sx, sy, w as u16, h as u16, !0, format.into(), seg.id, offset as u32)?;
                cookies.push((c, offset));
                offset += (w * h * 4) as usize;
            }
            let frame = Arc::make_mut(&mut mon.frame);
            for ((c, offset), &area) in cookies.into_iter().zip(&areas) {
                let reply = c.reply().context("reading the screen (MIT-SHM)")?;
                let len = (area.2 * area.3 * 4) as usize;
                ensure!(reply.size as usize == len, "MIT-SHM gave {} bytes for {len}", reply.size);
                put(frame, area, seg.bytes(offset, len), *layout);
            }
        } else {
            let mut cookies = Vec::with_capacity(areas.len());
            for &(x, y, w, h) in &areas {
                let (sx, sy) = at((x, y));
                cookies.push(conn.get_image(format, *root, sx, sy, w as u16, h as u16, !0)?);
            }
            let frame = Arc::make_mut(&mut mon.frame);
            for (c, &area) in cookies.into_iter().zip(&areas) {
                let reply = c.reply().context("reading the screen")?;
                let len = (area.2 * area.3 * 4) as usize;
                ensure!(reply.data.len() >= len, "GetImage gave {} bytes for {len}", reply.data.len());
                put(frame, area, &reply.data, *layout);
            }
        }
        // Everything read counts as changed (it may have been drawn without DAMAGE saying).
        let changed = Arc::make_mut(&mut mon.changed);
        for b in bands {
            for r in b.r0..=b.r1 {
                for c in b.c0..=b.c1 {
                    let t = (r * mon.cols + c) as usize;
                    changed[t] = *seq;
                    mon.pending[t] = false;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(seg) = &self.shm {
            let _ = self.conn.shm_detach(seg.id);
        }
        if let Some((d, region)) = self.damage {
            let _ = self.conn.damage_destroy(d);
            let _ = self.conn.xfixes_destroy_region(region);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_cover_the_pending_tiles_in_few_areas() {
        // 4×3 tiles; rows 0 and 1 alike, row 2 different.
        #[rustfmt::skip]
        let pending = [
            false, true, false, true,
            false, true, true, true,
            true, false, false, false,
        ];
        assert_eq!(bands(&pending, 4, 3), [Band { r0: 0, r1: 1, c0: 1, c1: 3 }, Band { r0: 2, r1: 2, c0: 0, c1: 0 }]);
        assert!(bands(&[false; 12], 4, 3).is_empty());
        // The last tiles are cut to the picture.
        assert_eq!(Band { r0: 0, r1: 1, c0: 1, c1: 3 }.px(200, 100), (64, 0, 136, 100));
    }

    #[test]
    fn damage_marks_the_tiles_it_touches_on_its_monitor() {
        let mut m = Mon::new(1, Rect { x: 100, y: 0, w: 256, h: 128 }, 1);
        m.pending.fill(false);
        // Left of the monitor: nothing.
        m.damage(0, 0, 100, 50);
        assert!(m.pending.iter().all(|&p| !p));
        // Across the 2nd and 3rd tile columns of the first row.
        m.damage(100 + 100, 10, 50, 20);
        assert_eq!(m.pending, [false, true, true, false, false, false, false, false]);
        // Past the bottom-right corner: clipped.
        m.damage(100 + 250, 120, 500, 500);
        assert!(m.pending[7]);
    }

    /// With an X server (`DISPLAY`, as Xvfb gives): what is drawn shows in the picture, and
    /// only its tiles count as changed. Skipped without one (CI).
    #[test]
    fn reads_what_is_drawn_and_only_there() {
        use x11rb::protocol::xproto::{CreateGCAux, Rectangle};
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("no DISPLAY: skipped");
            return;
        }
        let mut cap = Capture::open().expect("X11 capture");
        let (id, rect) = cap.monitors[0];
        let t = Instant::now();
        let first = cap.grab(id).unwrap();
        eprintln!("first picture {}x{} in {:?}", first.frame.width, first.frame.height, t.elapsed());
        let c1 = first.changes.clone().unwrap();
        assert!(c1.tiles.iter().all(|&s| s == c1.seq), "the first read is everything");

        // A red square on the root window, inside tile (1, 0).
        let (conn, n) = x11rb::connect(None).unwrap();
        let root = conn.setup().roots[n].root;
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0xff0000)).unwrap();
        conn.poly_fill_rectangle(root, gc, &[Rectangle { x: rect.x + 70, y: rect.y + 10, width: 20, height: 20 }])
            .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();

        let second = cap.grab(id).unwrap();
        let c2 = second.changes.clone().unwrap();
        assert_eq!(c2.source, c1.source);
        let at = ((15 * second.frame.width + 75) * 4) as usize;
        assert_eq!(&second.frame.data[at..at + 4], &[255, 0, 0, 255]);
        if cap.damage.is_some() {
            let changed: Vec<usize> = (0..c2.tiles.len()).filter(|&i| c2.tiles[i] > c1.seq).collect();
            assert_eq!(changed, [1], "only the square's tile was read");
            // Nothing drawn since: nothing read, the same picture.
            let t = Instant::now();
            let third = cap.grab(id).unwrap();
            eprintln!("a still screen in {:?}", t.elapsed());
            assert_eq!(third.changes.unwrap().seq, c2.seq);
            assert!(Arc::ptr_eq(&third.frame, &second.frame));
        }
        // A whole read, timed (MIT-SHM or not).
        cap.screens[0].pending.fill(true);
        let t = Instant::now();
        cap.grab(id).unwrap();
        eprintln!("the whole screen read in {:?} (MIT-SHM {})", t.elapsed(), cap.shm.is_some());
    }

    #[test]
    fn server_pixels_become_rgba() {
        let mut f = Frame::new(3, 2);
        // B, G, R, X (little-endian, red high).
        let src = [3, 2, 1, 0, 6, 5, 4, 0];
        put(&mut f, (1, 1, 2, 1), &src, Layout { le: true, swap: false });
        assert_eq!(&f.data[16..24], &[1, 2, 3, 255, 4, 5, 6, 255]);
        // X, R, G, B (big-endian).
        put(&mut f, (0, 0, 1, 1), &[0, 7, 8, 9], Layout { le: false, swap: false });
        assert_eq!(&f.data[..4], &[7, 8, 9, 255]);
        // Red in the low byte.
        put(&mut f, (0, 0, 1, 1), &[7, 8, 9, 0], Layout { le: true, swap: true });
        assert_eq!(&f.data[..4], &[7, 8, 9, 255]);
    }
}
