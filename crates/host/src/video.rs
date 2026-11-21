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
use rdesk_proto::{FrameHeader, Tile, encode_into};
use tokio::sync::{mpsc, oneshot, watch};

use crate::capture::Capturer;

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
                out.push(Planned { rect: self.rect(i), lossy, hash });
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

/// Run the video loop until the connection closes or `settings` is dropped.
pub async fn run(
    conn: transport::Connection,
    capturer: Capturer,
    mut settings: watch::Receiver<VideoSettings>,
    mut acks: mpsc::UnboundedReceiver<u64>,
    stats: Arc<std::sync::Mutex<VideoStats>>,
) -> Result<()> {
    let mut sched: Option<Scheduler> = None;
    let mut current = *settings.borrow_and_update();
    let mut next_id: u64 = 1;
    let mut inflight: HashMap<u64, InFlight> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<WriteDone>();
    let mut last_capture = Instant::now() - Duration::from_secs(1);
    let mut full_pending = true;
    let mut consecutive_errors = 0u32;

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
                for (_, f) in inflight.drain() {
                    if let Some(c) = f.cancel {
                        let _ = c.send(());
                    }
                }
            }
            current = s;
        }
        while let Ok(id) = acks.try_recv() {
            if inflight.remove(&id).is_some()
                && let Some(s) = sched.as_mut()
            {
                s.acked(id);
            }
        }
        while let Ok(d) = done_rx.try_recv() {
            if let WriteDone::Failed(id) = d
                && inflight.remove(&id).is_some()
                && let Some(s) = sched.as_mut()
            {
                s.lost(id);
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
                if let Some(s) = sched.as_mut() {
                    s.lost(id);
                }
            }
        }

        let interval = Duration::from_secs_f64(1.0 / current.max_fps.clamp(1, 60) as f64);
        let since = last_capture.elapsed();
        if inflight.len() >= MAX_IN_FLIGHT || since < interval {
            let wait = if inflight.len() >= MAX_IN_FLIGHT { Duration::from_millis(50) } else { interval - since };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                Some(id) = acks.recv() => {
                    if inflight.remove(&id).is_some() && let Some(s) = sched.as_mut() { s.acked(id); }
                }
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
        let frame = match crate::scale::fit(frame.width, frame.height, current.max_size.0, current.max_size.1) {
            Some((w, h)) => tokio::task::spawn_blocking(move || crate::scale::downscale(&frame, w, h)).await?,
            None => frame,
        };
        if sched.as_ref().is_none_or(|s| s.dims() != (frame.width, frame.height)) {
            sched = Some(Scheduler::new(frame.width, frame.height));
            full_pending = true;
        }
        let s = sched.as_mut().expect("scheduler just set");
        let id = next_id;
        let planned = s.plan(&frame, id, Instant::now());
        if planned.is_empty() {
            continue;
        }
        next_id += 1;
        let header = FrameHeader {
            frame_id: id,
            monitor_id: current.monitor,
            width: frame.width,
            height: frame.height,
            full: std::mem::take(&mut full_pending),
        };
        let quality = current.jpeg_quality;
        let frame = Arc::new(frame);
        let buf = {
            let frame = frame.clone();
            match tokio::task::spawn_blocking(move || encode_frame(&frame, header, &planned, quality)).await? {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("encoding frame failed: {e:#}");
                    s.lost(id);
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
