//! Video pipeline: capture → tile diff → encode → one QUIC stream per frame,
//! gated by client acknowledgements.
//!
//! At most [`MAX_IN_FLIGHT`] frames are unacknowledged at once. On a slow link
//! the frame rate drops instead of latency growing. Frames travel on reliable
//! streams, so a slow frame is left to finish: resetting it would throw away what
//! was already sent and the next frame, larger still, would meet the same fate
//! (the picture froze while it kept changing). Only a frame stuck for
//! [`STUCK_AFTER`] is reset and its tiles become dirty again. Lossless refinement
//! goes [`REFINE_PER_FRAME`] tiles at a time, so it never holds back what changes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;
use rdesk_common::tiles::{self, Frame, TileHash, TileRect};
use rdesk_common::transport;
use rdesk_proto::{FrameHeader, Tile, Tile2, TileData, VideoCaps, encode_into};
use tokio::sync::{mpsc, oneshot, watch};

use crate::autoq::{self, AutoQuality};
use crate::capture::Capturer;
use crate::scroll::{self, RowHashes};
use crate::tilecache::TileCache;

pub const MAX_IN_FLIGHT: usize = 2;
/// A tile that changed within this window is sent lossy (JPEG);
/// once still for this long it is refined losslessly.
const MOTION_WINDOW: Duration = Duration::from_millis(300);
/// A frame unacknowledged this long is given up on (a safety net: acks travel on a
/// reliable stream, so this means the client stopped reading).
const STUCK_AFTER: Duration = Duration::from_secs(10);
/// Lossless refinements per frame (a 64×64 tile is up to 16 KB): a settled screen is
/// refined over a few frames instead of in one that takes seconds on a slow link.
const REFINE_PER_FRAME: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub monitor: u32,
    pub max_fps: u8,
    pub jpeg_quality: u8,
    /// Incremented to force a full resend.
    pub epoch: u64,
    /// The client's largest useful picture (0×0: the monitor's own size); bigger captures
    /// are scaled down before they are cut into tiles.
    pub max_size: (u32, u32),
    /// Automatic quality: `jpeg_quality` is only the ceiling.
    pub auto: bool,
}

#[derive(Clone, Default)]
struct TileState {
    /// Hash of what the client has (acknowledged), and from which frame: acks can arrive out
    /// of order, and the client keeps the newest frame's version of a tile.
    confirmed: Option<TileHash>,
    confirmed_lossless: bool,
    confirmed_frame: u64,
    /// Versions sent and not acknowledged yet, oldest first: (frame, hash, lossless). At most
    /// [`MAX_IN_FLIGHT`]. Once they land the client shows the newest, so that is what the
    /// next frame is compared with: comparing with `confirmed` alone left a tile that changed
    /// and changed back (something passing by) showing the in-between version.
    inflight: Vec<(u64, TileHash, bool)>,
    /// Hash at the previous capture and when it last changed.
    current: Option<TileHash>,
    changed_at: Option<Instant>,
}

/// Pure bookkeeping for which tiles must be sent; independent of I/O so it can be tested.
pub struct Scheduler {
    width: u32,
    height: u32,
    cols: u32,
    tiles: Vec<TileState>,
}

pub struct Planned {
    pub rect: TileRect,
    pub lossy: bool,
    pub hash: TileHash,
    /// Which tile of the grid.
    pub index: usize,
    /// How it goes (second frame format only; the first always encodes).
    pub kind: Kind,
    /// Cache slot the client keeps it in (second format).
    pub store: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Encode,
    Cached(u32),
    Copy { x: u32, y: u32 },
}

impl Scheduler {
    pub fn new(width: u32, height: u32) -> Self {
        let (cols, rows) = tiles::grid(width, height);
        Self { width, height, cols, tiles: vec![TileState::default(); (cols * rows) as usize] }
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Decide which tiles of `frame` to send in frame `frame_id`.
    pub fn plan(&mut self, frame: &Frame, frame_id: u64, now: Instant) -> Vec<Planned> {
        let hashes: Vec<TileHash> =
            (0..self.tiles.len()).into_par_iter().map(|i| frame.hash_tile(self.rect(i))).collect();
        let mut out = Vec::new();
        let mut refinements = 0;
        for (i, hash) in hashes.into_iter().enumerate() {
            let t = &mut self.tiles[i];
            if t.current != Some(hash) {
                if t.current.is_some() {
                    t.changed_at = Some(now);
                }
                t.current = Some(hash);
            }
            let moving = t.changed_at.is_some_and(|c| now.duration_since(c) < MOTION_WINDOW);
            // What the client will show once everything sent has landed.
            let newest = t.inflight.last().map(|&(_, h, _)| h);
            let send = if newest.or(t.confirmed) != Some(hash) {
                // The client has (or will have) something else.
                true
            } else if !t.confirmed_lossless && !moving && newest.is_none() && refinements < REFINE_PER_FRAME {
                // Client has this content lossy and it has settled: refine it.
                refinements += 1;
                true
            } else {
                false
            };
            if send {
                let lossy = moving;
                t.inflight.push((frame_id, hash, !lossy));
                out.push(Planned { rect: self.rect(i), lossy, hash, index: i, kind: Kind::Encode, store: None });
            }
        }
        out
    }

    pub fn acked(&mut self, frame_id: u64) {
        for t in &mut self.tiles {
            if let Some(i) = t.inflight.iter().position(|&(f, _, _)| f == frame_id) {
                let (f, h, lossless) = t.inflight.remove(i);
                // A late ack of an older frame: the client kept the newer version.
                if t.confirmed.is_none() || f > t.confirmed_frame {
                    t.confirmed = Some(h);
                    t.confirmed_lossless = lossless;
                    t.confirmed_frame = f;
                }
            }
        }
    }

    /// The frame was given up on. The client may have painted some of it (a full frame is
    /// painted as it arrives), so its tiles are unknown there: send them again.
    pub fn lost(&mut self, frame_id: u64) {
        for t in &mut self.tiles {
            if let Some(i) = t.inflight.iter().position(|&(f, _, _)| f == frame_id) {
                t.inflight.remove(i);
                t.confirmed = None;
            }
        }
    }

    /// Per tile, whether the client will show exactly the previous capture there once every
    /// frame in flight has landed (in order): a copy may read from it. Call before
    /// [`Scheduler::plan`] with the next capture (it still compares with the previous one).
    pub fn copy_sources(&self) -> Vec<bool> {
        self.tiles
            .iter()
            .map(|t| {
                let newest =
                    t.inflight.last().map(|&(_, h, l)| (h, l)).or(t.confirmed.map(|h| (h, t.confirmed_lossless)));
                matches!((newest, t.current), (Some((h, true)), Some(c)) if h == c)
            })
            .collect()
    }

    /// Frame `frame_id` repaints every tile (H.264): the client will show this capture
    /// everywhere, not losslessly.
    pub fn mark_all_sent(&mut self, frame_id: u64) {
        for t in &mut self.tiles {
            t.inflight.retain(|e| e.0 != frame_id);
            if let Some(h) = t.current {
                t.inflight.push((frame_id, h, false));
                // Refined once it settles, like any lossy tile.
                t.changed_at.get_or_insert_with(Instant::now);
            }
        }
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Tile `i` of frame `frame_id` goes exactly (a copy, from the cache): nothing to refine.
    pub fn make_exact(&mut self, i: usize, frame_id: u64) {
        if let Some(e) = self.tiles[i].inflight.iter_mut().find(|e| e.0 == frame_id) {
            e.2 = true;
        }
    }

    /// Tile column and row of tile `i`.
    pub fn col_row(&self, i: usize) -> (u32, u32) {
        (i as u32 % self.cols, i as u32 / self.cols)
    }

    pub fn index_of(&self, col: u32, row: u32) -> usize {
        (row * self.cols + col) as usize
    }

    /// Tiles whose content is known to the client but was sent lossy.
    #[cfg(test)]
    fn lossy_count(&self) -> usize {
        self.tiles.iter().filter(|t| t.confirmed.is_some() && !t.confirmed_lossless).count()
    }

    fn rect(&self, i: usize) -> TileRect {
        let i = i as u32;
        tiles::tile_rect(i % self.cols, i / self.cols, self.width, self.height)
    }
}

/// Encode planned tiles into one framed buffer (header + tiles) in parallel.
fn encode_frame(frame: &Frame, header: FrameHeader, planned: &[Planned], quality: u8) -> Result<Vec<u8>> {
    let encoded: Vec<Result<Tile>> = planned
        .par_iter()
        .map_init(Vec::new, |buf, p| {
            frame.extract(p.rect, buf);
            tiles::encode_tile(p.rect, buf, p.lossy, quality)
        })
        .collect();
    let mut out = Vec::with_capacity(64 * 1024);
    encode_into(&header, &mut out)?;
    for t in encoded {
        encode_into(&t?, &mut out)?;
    }
    Ok(out)
}

/// Encode planned tiles in the second format ([`Tile2`]).
fn encode_frame2(frame: &Frame, header: FrameHeader, planned: &[Planned], quality: u8) -> Result<Vec<u8>> {
    let encoded: Vec<Result<Tile2>> = planned
        .par_iter()
        .map_init(Vec::new, |buf, p| {
            let data = match p.kind {
                Kind::Cached(slot) => TileData::Cached(slot),
                Kind::Copy { x, y } => TileData::Copy { x, y },
                Kind::Encode => {
                    frame.extract(p.rect, buf);
                    tiles::encode_tile2(p.rect.w, p.rect.h, buf, p.lossy, quality)?
                }
            };
            Ok(Tile2 { x: p.rect.x, y: p.rect.y, w: p.rect.w as u16, h: p.rect.h as u16, data, store: p.store })
        })
        .collect();
    let mut out = Vec::with_capacity(64 * 1024);
    encode_into(&header, &mut out)?;
    for t in encoded {
        encode_into(&t?, &mut out)?;
    }
    Ok(out)
}

/// Whether tile `r` of `frame` is one color (not worth a cache slot).
fn is_solid(frame: &Frame, r: TileRect) -> bool {
    let stride = frame.width as usize * 4;
    let at = |x: u32, y: u32| y as usize * stride + x as usize * 4;
    let first = &frame.data[at(r.x, r.y)..at(r.x, r.y) + 3];
    (r.y..r.y + r.h).all(|y| {
        let row = &frame.data[at(r.x, y)..at(r.x, y) + r.w as usize * 4];
        row.as_chunks::<4>().0.iter().all(|p| p[..3] == *first)
    })
}

/// The second frame format's extras: the tile cache, copies of what scrolled, H.264.
struct Extras {
    cache: TileCache,
    /// The previous capture and its row hashes (for scrolling).
    prev: Option<(Arc<Frame>, Option<RowHashes>)>,
    video: VideoMode,
}

/// Switching to H.264 while most of the screen keeps moving, and back to tiles once it settles.
#[derive(Default)]
struct VideoMode {
    /// The client decodes it.
    allowed: bool,
    encoder: Option<crate::h264::H264Encoder>,
    /// Consecutive frames with most of the screen moving, or nearly nothing.
    busy: u32,
    calm: u32,
}

/// Share of the tiles moving (sent lossy) for a frame to count as busy, or as calm.
const BUSY_SHARE: f64 = 0.4;
const CALM_SHARE: f64 = 0.1;
/// Frames in a row before switching.
const BUSY_FRAMES: u32 = 3;
const CALM_FRAMES: u32 = 4;

impl VideoMode {
    fn active(&self) -> bool {
        self.encoder.is_some()
    }

    /// Look at a planned frame; returns whether it goes as H.264.
    fn decide(&mut self, planned: &[Planned], tiles: usize, frame: &Frame, full: bool, fps: u8, quality: u8) -> bool {
        if !self.allowed || full || !crate::h264::usable(frame.width, frame.height) {
            self.encoder = None;
            self.busy = 0;
            return false;
        }
        let moving = planned.iter().filter(|p| p.lossy).count() as f64 / tiles.max(1) as f64;
        if self.encoder.as_ref().is_some_and(|e| e.size() != (frame.width, frame.height)) {
            self.encoder = None;
        }
        if self.active() {
            self.calm = if moving < CALM_SHARE { self.calm + 1 } else { 0 };
            if self.calm >= CALM_FRAMES {
                tracing::debug!("the screen settled: back to tiles");
                self.encoder = None;
                self.busy = 0;
                return false;
            }
            return true;
        }
        self.busy = if moving >= BUSY_SHARE { self.busy + 1 } else { 0 };
        if self.busy >= BUSY_FRAMES {
            let bps = crate::h264::bitrate(frame.width, frame.height, fps, quality);
            match crate::h264::H264Encoder::new(frame.width, frame.height, fps, bps) {
                Ok(e) => {
                    tracing::debug!("most of the screen is moving: H.264 at {} kbit/s", bps / 1000);
                    self.encoder = Some(e);
                    self.calm = 0;
                    return true;
                }
                Err(e) => {
                    tracing::warn!("H.264 unavailable: {e:#}");
                    self.allowed = false;
                }
            }
        }
        false
    }
}

/// Fewest changed tiles in a column before scrolling is looked for there (something that
/// scrolls changes a run of tiles; a small thing moving, a few).
const SCROLL_MIN_TILES: usize = 4;

impl Extras {
    fn new(caps: VideoCaps) -> Self {
        Self {
            cache: TileCache::new(caps.cache_slots),
            prev: None,
            video: VideoMode { allowed: caps.h264, ..Default::default() },
        }
    }

    /// Turn planned tiles into cache references and copies where possible, and choose the
    /// cache slots new lossless tiles go into. `sources` is [`Scheduler::copy_sources`] taken
    /// before planning; `copies` says whether this frame may copy.
    fn refine(
        &mut self,
        sched: &mut Scheduler,
        frame: &Arc<Frame>,
        sources: &[bool],
        planned: &mut [Planned],
        frame_id: u64,
        copies: bool,
    ) {
        let tile = rdesk_proto::TILE;
        // Scrolling: candidate offsets per tile column.
        let mut offsets: Option<(Vec<Option<i32>>, Option<i32>)> = None;
        let mut new_rows = None;
        if copies
            && planned.len() >= SCROLL_MIN_TILES
            && let Some((prev, prev_rows)) = &mut self.prev
            && prev.width == frame.width
            && prev.height == frame.height
        {
            let mut per_col = vec![0usize; frame.grid().0 as usize];
            for p in planned.iter() {
                per_col[(p.rect.x / tile) as usize] += 1;
            }
            let changed: Vec<bool> = per_col.iter().map(|&n| n >= SCROLL_MIN_TILES).collect();
            if changed.iter().any(|&c| c) {
                let old = prev_rows.get_or_insert_with(|| RowHashes::new(prev));
                old.fill(prev, &changed);
                let mut new = RowHashes::new(frame);
                new.fill(frame, &changed);
                offsets = Some(scroll::candidates(old, &new, &changed));
                new_rows = Some(new);
            }
        }
        for p in planned.iter_mut() {
            if let Some(slot) = self.cache.lookup(&p.hash, frame_id) {
                p.kind = Kind::Cached(slot);
                sched.make_exact(p.index, frame_id);
                continue;
            }
            if let (Some((bands, global)), Some((prev, _))) = (&offsets, &self.prev) {
                let (col, _) = sched.col_row(p.index);
                let source = [bands[col as usize], *global].into_iter().flatten().find_map(|dy| {
                    if !scroll::matches(prev, frame, p.rect, dy) {
                        return None;
                    }
                    // Every tile the source overlaps must show the previous capture on the client.
                    let sy = (p.rect.y as i64 + dy as i64) as u32;
                    let (r0, r1) = (sy / tile, (sy + p.rect.h - 1) / tile);
                    (r0..=r1).all(|r| sources.get(sched.index_of(col, r)).copied().unwrap_or(false)).then_some(sy)
                });
                if let Some(sy) = source {
                    p.kind = Kind::Copy { x: p.rect.x, y: sy };
                    sched.make_exact(p.index, frame_id);
                    continue;
                }
            }
            if !p.lossy && self.cache.enabled() && !is_solid(frame, p.rect) {
                p.store = self.cache.store(&p.hash, frame_id);
            }
        }
        self.prev = Some((frame.clone(), new_rows));
    }
}

/// What automatic quality chose, for the client: (frames per second, JPEG quality, scale %).
pub type QualityReport = watch::Sender<(u8, u8, u8)>;

pub struct VideoStats {
    pub frames: u64,
    pub bytes: u64,
}

struct InFlight {
    sent_at: Instant,
    cancel: Option<oneshot::Sender<()>>,
}

/// Results reported back by frame-writer tasks.
enum WriteDone {
    Ok,
    Failed(u64),
}

/// How the client wants its frames.
pub struct VideoOpts {
    /// The second frame format, with what the client decodes (`None`: the first format).
    pub caps: Option<VideoCaps>,
    /// Where automatic quality reports what it chose.
    pub report: Option<QualityReport>,
}

/// Run the video loop until the connection closes or `settings` is dropped.
pub async fn run(
    conn: transport::Connection,
    capturer: Capturer,
    mut settings: watch::Receiver<VideoSettings>,
    mut acks: mpsc::UnboundedReceiver<u64>,
    stats: Arc<std::sync::Mutex<VideoStats>>,
    opts: VideoOpts,
) -> Result<()> {
    let mut sched: Option<Scheduler> = None;
    let mut current = *settings.borrow_and_update();
    let mut next_id: u64 = 1;
    let mut inflight: HashMap<u64, InFlight> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<WriteDone>();
    let mut last_capture = Instant::now() - Duration::from_secs(1);
    let mut full_pending = true;
    let mut consecutive_errors = 0u32;
    let mut extras = opts.caps.map(Extras::new);
    let mut auto = AutoQuality::new(current.jpeg_quality, Instant::now());

    // What happens when the client acknowledges a frame, or it is lost.
    let on_ack = |id: u64,
                  inflight: &mut HashMap<u64, InFlight>,
                  sched: &mut Option<Scheduler>,
                  extras: &mut Option<Extras>,
                  auto: &mut AutoQuality| {
        if let Some(f) = inflight.remove(&id) {
            auto.acked(f.sent_at.elapsed());
            if let Some(s) = sched.as_mut() {
                s.acked(id);
            }
        }
        if let Some(x) = extras.as_mut() {
            x.cache.acked(id);
        }
    };
    // A lost frame: its tiles are sent again. In the second format the client applies frames in
    // order, so later frames may have copied from what it would have drawn: start over.
    let on_lost = |id: u64, sched: &mut Option<Scheduler>, extras: &mut Option<Extras>, full_pending: &mut bool| {
        if let Some(x) = extras.as_mut() {
            x.cache.lost(id);
            *sched = None;
            *full_pending = true;
        } else if let Some(s) = sched.as_mut() {
            s.lost(id);
        }
    };

    loop {
        // Housekeeping: settings, acks, finished writes, stale frames. The settings are read
        // every turn: the wait below (`settings.changed()`) marks a new value seen, so asking
        // `has_changed` here lost every change that came during it (most of them: frames per
        // second, quality, monitor).
        if settings.has_changed().is_err() {
            return Ok(());
        }
        let s = *settings.borrow_and_update();
        if s != current {
            if s.monitor != current.monitor || s.epoch != current.epoch || s.max_size != current.max_size {
                sched = None;
                full_pending = true;
                for (id, f) in inflight.drain() {
                    if let Some(c) = f.cancel {
                        let _ = c.send(());
                    }
                    if let Some(x) = extras.as_mut() {
                        x.cache.lost(id);
                    }
                }
            }
            if s.auto && !current.auto {
                auto = AutoQuality::new(s.jpeg_quality, Instant::now());
            }
            current = s;
        }
        while let Ok(id) = acks.try_recv() {
            on_ack(id, &mut inflight, &mut sched, &mut extras, &mut auto);
        }
        while let Ok(d) = done_rx.try_recv() {
            if let WriteDone::Failed(id) = d
                && inflight.remove(&id).is_some()
            {
                on_lost(id, &mut sched, &mut extras, &mut full_pending);
            }
        }
        let stuck: Vec<u64> =
            inflight.iter().filter(|(_, f)| f.sent_at.elapsed() > STUCK_AFTER).map(|(id, _)| *id).collect();
        for id in stuck {
            if let Some(mut f) = inflight.remove(&id) {
                tracing::warn!("frame {id} unacknowledged after {STUCK_AFTER:?}, resending its tiles");
                if let Some(c) = f.cancel.take() {
                    let _ = c.send(());
                }
                on_lost(id, &mut sched, &mut extras, &mut full_pending);
            }
        }

        let interval = Duration::from_secs_f64(1.0 / current.max_fps.clamp(1, 60) as f64);
        if current.auto {
            let oldest = inflight.values().map(|f| f.sent_at.elapsed()).max();
            if let Some(c) = auto.decide(Instant::now(), transport::rtt(&conn), interval, current.jpeg_quality, oldest)
            {
                tracing::debug!("automatic quality: JPEG {} at {}%", c.jpeg_quality, c.scale_pct);
                if let Some(r) = &opts.report {
                    let _ = r.send((current.max_fps, c.jpeg_quality, c.scale_pct));
                }
            }
        }
        let since = last_capture.elapsed();
        if inflight.len() >= MAX_IN_FLIGHT || since < interval {
            let wait = if inflight.len() >= MAX_IN_FLIGHT { Duration::from_millis(50) } else { interval - since };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                Some(id) = acks.recv() => on_ack(id, &mut inflight, &mut sched, &mut extras, &mut auto),
                r = settings.changed() => { if r.is_err() { return Ok(()); } }
                _ = conn.closed() => return Ok(()),
            }
            continue;
        }
        last_capture = Instant::now();

        let frame = match capturer.grab(current.monitor).await {
            Ok(f) => {
                consecutive_errors = 0;
                f
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(50) {
                    tracing::warn!("capture failed ({consecutive_errors}x): {e:#}");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        // The client's size limit, then what automatic quality chose.
        let (mut w, mut h) = crate::scale::fit(frame.width, frame.height, current.max_size.0, current.max_size.1)
            .unwrap_or((frame.width, frame.height));
        let (quality, scale_pct) = if current.auto {
            let c = auto.choice();
            (c.jpeg_quality.min(current.jpeg_quality), c.scale_pct)
        } else {
            (current.jpeg_quality, 100)
        };
        (w, h) = autoq::scaled(w, h, scale_pct);
        let frame = if (w, h) != (frame.width, frame.height) {
            tokio::task::spawn_blocking(move || crate::scale::downscale(&frame, w, h)).await?
        } else {
            frame
        };
        if sched.as_ref().is_none_or(|s| s.dims() != (frame.width, frame.height)) {
            sched = Some(Scheduler::new(frame.width, frame.height));
            full_pending = true;
        }
        let s = sched.as_mut().expect("scheduler just set");
        let id = next_id;
        let sources = extras.as_ref().map(|_| s.copy_sources());
        let mut planned = s.plan(&frame, id, Instant::now());
        let frame = Arc::new(frame);
        if planned.is_empty() {
            if let Some(x) = extras.as_mut() {
                x.prev = Some((frame, None));
            }
            continue;
        }
        next_id += 1;
        let full = std::mem::take(&mut full_pending);
        let tiles_total = s.tile_count();
        let video = extras
            .as_mut()
            .is_some_and(|x| x.video.decide(&planned, tiles_total, &frame, full, current.max_fps, quality));
        let header =
            FrameHeader { frame_id: id, monitor_id: current.monitor, width: frame.width, height: frame.height, full };
        if video && let Some(x) = extras.as_mut() {
            // The whole picture as H.264; the tiles' bookkeeping says it is all there, lossy.
            s.mark_all_sent(id);
            x.prev = Some((frame.clone(), None));
            let mut enc = x.video.encoder.take().expect("video mode has an encoder");
            let f = frame.clone();
            let (enc, au) = tokio::task::spawn_blocking(move || {
                let r = enc.encode(&f);
                (enc, r)
            })
            .await?;
            let buf = match au {
                Ok(au) => {
                    x.video.encoder = Some(enc);
                    let tile = Tile2 {
                        x: 0,
                        y: 0,
                        w: frame.width as u16,
                        h: frame.height as u16,
                        data: TileData::H264(au),
                        store: None,
                    };
                    let mut out = Vec::with_capacity(64 * 1024);
                    encode_into(&header, &mut out)?;
                    encode_into(&tile, &mut out)?;
                    out
                }
                Err(e) => {
                    tracing::warn!("H.264 encoding failed, back to tiles: {e:#}");
                    x.video.allowed = false;
                    on_lost(id, &mut sched, &mut extras, &mut full_pending);
                    continue;
                }
            };
            {
                let mut st = stats.lock().unwrap();
                st.frames += 1;
                st.bytes += buf.len() as u64;
            }
            let (cancel_tx, cancel_rx) = oneshot::channel();
            inflight.insert(id, InFlight { sent_at: Instant::now(), cancel: Some(cancel_tx) });
            let (conn2, done) = (conn.clone(), done_tx.clone());
            tokio::spawn(async move {
                let ok = write_frame(&conn2, &buf, cancel_rx).await;
                let _ = done.send(if ok { WriteDone::Ok } else { WriteDone::Failed(id) });
            });
            continue;
        }
        if let (Some(x), Some(sources)) = (extras.as_mut(), &sources) {
            x.refine(s, &frame, sources, &mut planned, id, !full);
        }
        let second = extras.is_some();
        let buf = {
            let frame = frame.clone();
            let encoded = tokio::task::spawn_blocking(move || {
                if second {
                    encode_frame2(&frame, header, &planned, quality)
                } else {
                    encode_frame(&frame, header, &planned, quality)
                }
            })
            .await?;
            match encoded {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("encoding frame failed: {e:#}");
                    on_lost(id, &mut sched, &mut extras, &mut full_pending);
                    continue;
                }
            }
        };
        {
            let mut st = stats.lock().unwrap();
            st.frames += 1;
            st.bytes += buf.len() as u64;
        }

        let (cancel_tx, cancel_rx) = oneshot::channel();
        inflight.insert(id, InFlight { sent_at: Instant::now(), cancel: Some(cancel_tx) });
        let conn2 = conn.clone();
        let done = done_tx.clone();
        tokio::spawn(async move {
            let ok = write_frame(&conn2, &buf, cancel_rx).await;
            let _ = done.send(if ok { WriteDone::Ok } else { WriteDone::Failed(id) });
        });
    }
}

async fn write_frame(conn: &transport::Connection, buf: &[u8], mut cancel: oneshot::Receiver<()>) -> bool {
    let Ok(mut s) = conn.open_uni().await else { return false };
    let _ = s.set_priority(10);
    let r = tokio::select! {
        r = s.write_all(buf) => r.is_ok(),
        _ = &mut cancel => false,
    };
    if r {
        s.finish().is_ok()
    } else {
        let _ = s.reset(transport::VarInt::from_u32(0));
        false
    }
}

#[cfg(test)]
mod second_format_tests {
    use rdesk_core::video2::{DTile, Decoded, Pix, V2};
    use rdesk_proto::{Tile2, VideoCaps};

    use super::*;

    /// A page of "text" lines scrolled down by `scroll` rows, with a fixed sidebar on the left.
    fn page(w: u32, h: u32, scroll: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let px = if x < 64 {
                    [40, 40, (y / 8) as u8]
                } else {
                    let line = y + scroll;
                    let ink = (line % 16 < 11) && ((x * 7 + line * 13) % 23 < 9);
                    if ink { [(line * 3) as u8, 30, 30] } else { [250, 250, 250] }
                };
                f.data[i..i + 3].copy_from_slice(&px);
            }
        }
        f
    }

    /// Decode a second-format frame the way the client does.
    fn decode(buf: &[u8]) -> (FrameHeader, Decoded, usize) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut r: &[u8] = buf;
            let h: FrameHeader = rdesk_proto::expect_msg(&mut r, rdesk_proto::MAX_VIDEO_MSG).await.unwrap();
            let mut tiles = Vec::new();
            let mut special = 0;
            while let Some(t) = rdesk_proto::read_msg::<_, Tile2>(&mut r, rdesk_proto::MAX_VIDEO_MSG).await.unwrap() {
                let rect = TileRect { x: t.x, y: t.y, w: t.w as u32, h: t.h as u32 };
                let pix = match t.data {
                    TileData::Cached(s) => {
                        special += 1;
                        Pix::Cached(s)
                    }
                    TileData::Copy { x, y } => {
                        special += 1;
                        Pix::Copy { x, y }
                    }
                    TileData::H264(au) => Pix::H264(au),
                    d => {
                        let mut px = Vec::new();
                        tiles::decode_tile2(&d, rect.w, rect.h, &mut px).unwrap();
                        Pix::Pixels(px)
                    }
                };
                tiles.push(DTile { rect, pix, store: t.store });
            }
            (h, Decoded { monitor: h.monitor_id, width: h.width, height: h.height, tiles }, special)
        })
    }

    /// Host and client side by side, every frame acknowledged once applied.
    struct Link {
        sched: Scheduler,
        extras: Extras,
        client: Frame,
        v2: V2,
        id: u64,
        now: Instant,
        bytes: usize,
        special: usize,
        h264_frames: usize,
    }

    impl Link {
        fn new(w: u32, h: u32) -> Self {
            Self::with_h264(w, h, false)
        }

        fn with_h264(w: u32, h: u32, h264: bool) -> Self {
            let caps = VideoCaps { cache_slots: 256, h264 };
            Self {
                sched: Scheduler::new(w, h),
                extras: Extras::new(caps),
                client: Frame::new(w, h),
                v2: V2::new(caps.cache_slots),
                id: 1,
                now: Instant::now(),
                bytes: 0,
                special: 0,
                h264_frames: 0,
            }
        }

        /// Send `f` (if anything changed); returns how many tiles went.
        fn send(&mut self, f: &Frame, dt: Duration) -> usize {
            self.now += dt;
            let id = self.id;
            let sources = self.sched.copy_sources();
            let mut planned = self.sched.plan(f, id, self.now);
            let frame = Arc::new(f.clone());
            if planned.is_empty() {
                self.extras.prev = Some((frame, None));
                return 0;
            }
            self.id += 1;
            let full = id == 1;
            let header = FrameHeader { frame_id: id, monitor_id: 0, width: f.width, height: f.height, full };
            let tiles = self.sched.tile_count();
            let buf = if self.extras.video.decide(&planned, tiles, f, full, 30, 80) {
                // As the video loop does.
                self.sched.mark_all_sent(id);
                self.extras.prev = Some((frame.clone(), None));
                let au = self.extras.video.encoder.as_mut().unwrap().encode(f).unwrap();
                let tile =
                    Tile2 { x: 0, y: 0, w: f.width as u16, h: f.height as u16, data: TileData::H264(au), store: None };
                let mut out = Vec::new();
                encode_into(&header, &mut out).unwrap();
                encode_into(&tile, &mut out).unwrap();
                self.h264_frames += 1;
                out
            } else {
                self.extras.refine(&mut self.sched, &frame, &sources, &mut planned, id, !full);
                encode_frame2(f, header, &planned, 80).unwrap()
            };
            self.bytes += buf.len();
            let (h, dec, special) = decode(&buf);
            self.special += special;
            if h.full {
                self.v2.skip_to(id);
            }
            self.v2.push(id, dec);
            for (fid, d) in self.v2.ready(self.now) {
                self.v2.apply(&mut self.client, d).unwrap();
                self.sched.acked(fid);
                self.extras.cache.acked(fid);
            }
            planned.len()
        }

        /// Send `f` until nothing is left to refine.
        fn settle(&mut self, f: &Frame) {
            for _ in 0..50 {
                if self.send(f, Duration::from_secs(1)) == 0 {
                    return;
                }
            }
            panic!("never settled");
        }
    }

    #[test]
    fn scrolling_and_coming_back_end_exactly_like_the_host() {
        let (w, h) = (448, 384);
        let mut link = Link::new(w, h);
        let top = page(w, h, 0);
        link.settle(&top);
        assert!(link.client.data == top.data, "first picture");
        // Scroll down in steps, as a wheel does.
        let before = link.bytes;
        for s in [24, 48, 72, 96] {
            link.send(&page(w, h, s), Duration::from_millis(30));
        }
        let scrolled = page(w, h, 96);
        link.settle(&scrolled);
        assert!(link.client.data == scrolled.data, "after scrolling");
        assert!(link.special > 0, "scrolling went as copies");
        let scroll_bytes = link.bytes - before;
        // Back to the top: the old tiles come from the cache.
        let specials = link.special;
        link.settle(&top);
        assert!(link.client.data == top.data, "back at the top");
        assert!(link.special > specials, "coming back used the cache or copies");
        eprintln!("scrolling cost {scroll_bytes} bytes; a full picture is {}", w * h * 4);
    }

    /// Everything moving (a video full screen): H.264, then tiles again, ending exact.
    #[test]
    fn whole_screen_motion_goes_as_h264_and_settles_exactly() {
        let (w, h) = (448, 384);
        let mut link = Link::with_h264(w, h, true);
        let mut f = page(w, h, 0);
        link.settle(&f);
        for t in 1..20u32 {
            for (i, px) in f.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                *px = [((x + t * 9) % 256) as u8, ((y * 2 + t * 5) % 256) as u8, (t * 13 % 256) as u8, 255];
            }
            link.send(&f, Duration::from_millis(33));
        }
        assert!(link.h264_frames >= 10, "only {} H.264 frames", link.h264_frames);
        link.settle(&f);
        assert!(!link.extras.video.active(), "still in video mode once still");
        assert!(link.client.data == f.data, "the settled picture is exact");
    }

    #[test]
    fn copies_only_read_what_the_client_shows_exactly() {
        let (w, h) = (448, 384);
        let mut link = Link::new(w, h);
        link.settle(&page(w, h, 0));
        // Scroll while the previous scroll's tiles were sent lossy (still moving): whatever
        // goes as a copy must still end exact.
        for s in 1..12 {
            link.send(&page(w, h, s * 8), Duration::from_millis(20));
        }
        let last = page(w, h, 88);
        link.settle(&last);
        assert!(link.client.data == last.data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with(w: u32, h: u32, v: u8) -> Frame {
        let mut f = Frame::new(w, h);
        for (i, b) in f.data.iter_mut().enumerate() {
            if i % 4 != 3 {
                *b = v.wrapping_add((i / 4 % 251) as u8);
            }
        }
        f
    }

    #[test]
    fn first_frame_sends_everything_then_nothing() {
        let f = frame_with(200, 100, 1);
        let mut s = Scheduler::new(200, 100);
        let t0 = Instant::now();
        let p = s.plan(&f, 1, t0);
        assert_eq!(p.len(), 4 * 2);
        assert!(p.iter().all(|p| !p.lossy), "static first frame is lossless");
        // Not acked yet, same content: nothing new.
        assert!(s.plan(&f, 2, t0).is_empty());
        s.acked(1);
        assert!(s.plan(&f, 3, t0 + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn lost_frame_tiles_are_resent() {
        let f = frame_with(128, 64, 1);
        let mut s = Scheduler::new(128, 64);
        let t0 = Instant::now();
        assert_eq!(s.plan(&f, 1, t0).len(), 2);
        s.lost(1);
        assert_eq!(s.plan(&f, 2, t0).len(), 2);
    }

    #[test]
    fn motion_is_lossy_then_refined() {
        let mut s = Scheduler::new(64, 64);
        let t0 = Instant::now();
        let a = frame_with(64, 64, 1);
        let b = frame_with(64, 64, 9);
        s.plan(&a, 1, t0);
        s.acked(1);
        let p = s.plan(&b, 2, t0 + Duration::from_millis(10));
        assert_eq!(p.len(), 1);
        assert!(p[0].lossy);
        s.acked(2);
        assert_eq!(s.lossy_count(), 1);
        // Still moving window: no refinement yet.
        assert!(s.plan(&b, 3, t0 + Duration::from_millis(100)).is_empty());
        // Settled: lossless refinement.
        let p = s.plan(&b, 4, t0 + Duration::from_millis(500));
        assert_eq!(p.len(), 1);
        assert!(!p[0].lossy);
        s.acked(4);
        assert_eq!(s.lossy_count(), 0);
        assert!(s.plan(&b, 5, t0 + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn refinement_is_spread_over_frames() {
        let (w, h) = (64 * 20, 64 * 5); // 100 tiles
        let mut s = Scheduler::new(w, h);
        let t0 = Instant::now();
        s.plan(&frame_with(w, h, 1), 1, t0);
        s.acked(1);
        // Everything changes: all of it goes at once, lossy.
        let b = frame_with(w, h, 9);
        let p = s.plan(&b, 2, t0 + Duration::from_millis(10));
        assert_eq!(p.len(), 100);
        assert!(p.iter().all(|p| p.lossy));
        s.acked(2);
        // Settled: refined a batch at a time.
        let t = t0 + Duration::from_secs(1);
        let mut id = 3;
        let mut batches = Vec::new();
        loop {
            let p = s.plan(&b, id, t);
            if p.is_empty() {
                break;
            }
            assert!(p.iter().all(|p| !p.lossy));
            batches.push(p.len());
            s.acked(id);
            id += 1;
        }
        assert_eq!(batches, vec![REFINE_PER_FRAME, REFINE_PER_FRAME, 100 - 2 * REFINE_PER_FRAME]);
        assert_eq!(s.lossy_count(), 0);
        // A change still goes out in full while a refinement batch is in flight.
        let c = frame_with(w, h, 20);
        assert_eq!(s.plan(&c, id, t + Duration::from_millis(10)).len(), 100);
    }

    #[test]
    fn change_during_flight_is_sent_again() {
        let mut s = Scheduler::new(64, 64);
        let t0 = Instant::now();
        s.plan(&frame_with(64, 64, 1), 1, t0);
        // Content changes before frame 1 is acked: frame 2 must carry it.
        assert_eq!(s.plan(&frame_with(64, 64, 2), 2, t0).len(), 1);
        // Late ack for frame 1 must not mark the newer content as confirmed.
        s.acked(1);
        assert_eq!(s.plan(&frame_with(64, 64, 2), 3, t0).len(), 0, "frame 2 still in flight");
        s.lost(2);
        assert_eq!(s.plan(&frame_with(64, 64, 2), 4, t0).len(), 1);
    }

    /// Something passing over a tile: frame 2 draws it, frame 3 (the tile back to what the
    /// client confirmed) must still be sent, or the client keeps frame 2's version.
    #[test]
    fn content_back_to_the_confirmed_one_is_sent_while_another_is_in_flight() {
        let mut s = Scheduler::new(64, 64);
        let t0 = Instant::now();
        let (bg, square) = (frame_with(64, 64, 1), frame_with(64, 64, 2));
        s.plan(&bg, 1, t0);
        s.acked(1);
        assert_eq!(s.plan(&square, 2, t0).len(), 1);
        assert_eq!(s.plan(&bg, 3, t0).len(), 1, "the client would keep showing frame 2");
        s.acked(2);
        s.acked(3);
        // Settled: only the lossless refinement of what frame 3 sent as JPEG is left.
        let p = s.plan(&bg, 4, t0 + Duration::from_secs(1));
        assert!(p.len() == 1 && !p[0].lossy);
        s.acked(4);
        assert!(s.plan(&bg, 5, t0 + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn acks_out_of_order_keep_the_newest_version() {
        let mut s = Scheduler::new(64, 64);
        let t0 = Instant::now();
        s.plan(&frame_with(64, 64, 1), 1, t0);
        s.acked(1);
        s.plan(&frame_with(64, 64, 2), 2, t0);
        let c = frame_with(64, 64, 3);
        s.plan(&c, 3, t0);
        // Frame 3 lands first; frame 2's tile is then older than what the client shows.
        s.acked(3);
        s.acked(2);
        assert!(s.plan(&c, 4, t0).is_empty(), "the client has frame 3's version");
    }

    #[test]
    fn encoded_frame_decodes_to_same_image() {
        let f = frame_with(130, 70, 3);
        let mut s = Scheduler::new(130, 70);
        let planned = s.plan(&f, 1, Instant::now());
        let header = FrameHeader { frame_id: 1, monitor_id: 0, width: 130, height: 70, full: true };
        let buf = encode_frame(&f, header, &planned, 80).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let out = rt.block_on(async {
            let mut r: &[u8] = &buf;
            let h: FrameHeader = rdesk_proto::expect_msg(&mut r, rdesk_proto::MAX_VIDEO_MSG).await.unwrap();
            assert_eq!(h, header);
            let mut fb = Frame::new(h.width, h.height);
            while let Some(t) = rdesk_proto::read_msg::<_, Tile>(&mut r, rdesk_proto::MAX_VIDEO_MSG).await.unwrap() {
                fb.apply(&t, &mut Vec::new()).unwrap();
            }
            fb
        });
        assert!(out.data == f.data);
    }
}
