//! The client's half of the second frame format ([`rdesk_proto::Tile2`]): frames applied in
//! order, the tile cache, and copies of what is already on screen.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use rdesk_common::tiles::{Frame, TileRect};

/// A tile ready to paint.
pub enum Pix {
    Pixels(Vec<u8>),
    Cached(u32),
    Copy {
        x: u32,
        y: u32,
    },
    /// The whole picture as an H.264 access unit (decoded in order, when applied).
    H264(Vec<u8>),
}

pub struct DTile {
    pub rect: TileRect,
    pub pix: Pix,
    pub store: Option<u32>,
}

/// A frame read and decoded, waiting for its turn.
pub struct Decoded {
    pub monitor: u32,
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<DTile>,
}

/// How long a missing frame is waited for before the frames after it are applied anyway (the
/// host only drops a frame when it starts over with a full one; this is a safety net).
pub const GAP_WAIT: Duration = Duration::from_millis(500);

pub struct V2 {
    /// The next frame to apply (`None` until the first one arrives).
    pub next: Option<u64>,
    pending: BTreeMap<u64, Decoded>,
    blocked_since: Option<Instant>,
    cache: Vec<Option<(u32, u32, Vec<u8>)>>,
    #[cfg(feature = "h264")]
    h264: Option<rdesk_common::h264::H264Decoder>,
}

impl V2 {
    pub fn new(cache_slots: u32) -> Self {
        Self {
            next: None,
            pending: BTreeMap::new(),
            blocked_since: None,
            cache: vec![None; cache_slots as usize],
            #[cfg(feature = "h264")]
            h264: None,
        }
    }

    /// A full frame `id`: nothing before it matters any more.
    pub fn skip_to(&mut self, id: u64) {
        self.next = Some(id);
        self.pending = self.pending.split_off(&id);
        self.blocked_since = None;
    }

    /// Frame `id` was painted as it arrived (a full one).
    pub fn done(&mut self, id: u64) {
        if self.next.is_none_or(|n| n <= id) {
            self.next = Some(id + 1);
        }
    }

    pub fn push(&mut self, id: u64, f: Decoded) {
        if self.next.is_none() {
            self.next = Some(id);
        }
        if self.next.is_some_and(|n| id >= n) {
            self.pending.insert(id, f);
        }
    }

    /// The frames that can be applied now, in order. After [`GAP_WAIT`] (counted from the
    /// first call that found a gap) a missing frame is given up on.
    pub fn ready(&mut self, now: Instant) -> Vec<(u64, Decoded)> {
        let mut out = Vec::new();
        while let Some(next) = self.next {
            if let Some(f) = self.pending.remove(&next) {
                out.push((next, f));
                self.next = Some(next + 1);
                self.blocked_since = None;
                continue;
            }
            let Some(&first) = self.pending.keys().next() else { break };
            let since = *self.blocked_since.get_or_insert(now);
            if now.duration_since(since) < GAP_WAIT {
                break;
            }
            tracing::debug!("frames {next}..{first} never came; going on");
            self.next = Some(first);
            self.blocked_since = None;
        }
        out
    }

    /// Whether frames are waiting for a missing one.
    pub fn blocked(&self) -> bool {
        !self.pending.is_empty()
    }

    fn cached(&self, slot: u32, r: TileRect) -> Result<&[u8]> {
        match self.cache.get(slot as usize) {
            Some(Some((w, h, px))) if (*w, *h) == (r.w, r.h) => Ok(px),
            Some(Some(_)) => Err(anyhow!("cache slot {slot} holds a tile of another size")),
            Some(None) => Err(anyhow!("cache slot {slot} is empty")),
            None => Err(anyhow!("cache slot {slot} out of range")),
        }
    }

    fn keep(&mut self, slot: Option<u32>, r: TileRect, px: &[u8]) -> Result<()> {
        if let Some(s) = slot {
            let e = self.cache.get_mut(s as usize).ok_or_else(|| anyhow!("cache slot {s} out of range"))?;
            *e = Some((r.w, r.h, px.to_vec()));
        }
        Ok(())
    }

    /// Paint one tile of a full frame as it arrives (copies make no sense there).
    pub fn paint_now(&mut self, frame: &mut Frame, t: DTile) -> Result<TileRect> {
        let px = match t.pix {
            Pix::Pixels(px) => px,
            Pix::Cached(slot) => self.cached(slot, t.rect)?.to_vec(),
            Pix::Copy { .. } | Pix::H264(_) => return Err(anyhow!("a full frame cannot copy or be H.264")),
        };
        frame.blit(t.rect, &px)?;
        self.keep(t.store, t.rect, &px)?;
        Ok(t.rect)
    }

    /// Apply a whole frame: its copies read the picture as it was before it. Returns the
    /// rectangles painted.
    pub fn apply(&mut self, frame: &mut Frame, f: Decoded) -> Result<Vec<TileRect>> {
        ensure!(frame.width == f.width && frame.height == f.height, "frame of another size");
        let mut copies: Vec<Vec<u8>> = Vec::new();
        for t in &f.tiles {
            if let Pix::Copy { x, y } = t.pix {
                let src = TileRect { x, y, w: t.rect.w, h: t.rect.h };
                ensure!(x + src.w <= frame.width && y + src.h <= frame.height, "copy from outside the picture");
                let mut buf = Vec::new();
                frame.extract(src, &mut buf);
                copies.push(buf);
            }
        }
        let mut copies = copies.into_iter();
        let mut painted = Vec::with_capacity(f.tiles.len());
        for t in f.tiles {
            let px = match t.pix {
                Pix::Pixels(px) => px,
                Pix::Cached(slot) => self.cached(slot, t.rect)?.to_vec(),
                Pix::Copy { .. } => copies.next().expect("one per copy"),
                Pix::H264(au) => {
                    self.h264_into(&au, frame)?;
                    painted.push(TileRect { x: 0, y: 0, w: frame.width, h: frame.height });
                    continue;
                }
            };
            frame.blit(t.rect, &px)?;
            self.keep(t.store, t.rect, &px)?;
            painted.push(t.rect);
        }
        Ok(painted)
    }
}

impl V2 {
    #[cfg(feature = "h264")]
    fn h264_into(&mut self, au: &[u8], frame: &mut Frame) -> Result<()> {
        if self.h264.is_none() {
            self.h264 = Some(rdesk_common::h264::H264Decoder::new()?);
        }
        let r = self.h264.as_mut().expect("just made").decode_into(au, frame);
        if r.is_err() {
            // Start over with the next key frame.
            self.h264 = None;
        }
        r
    }

    #[cfg(not(feature = "h264"))]
    fn h264_into(&mut self, _: &[u8], _: &mut Frame) -> Result<()> {
        Err(anyhow!("this app does not decode H.264"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(tiles: Vec<DTile>) -> Decoded {
        Decoded { monitor: 0, width: 128, height: 64, tiles }
    }

    fn px(v: u8) -> Vec<u8> {
        vec![v; 64 * 64 * 4]
    }

    fn r(col: u32) -> TileRect {
        TileRect { x: col * 64, y: 0, w: 64, h: 64 }
    }

    #[test]
    fn frames_apply_in_order() {
        let mut v = V2::new(0);
        let t = Instant::now();
        v.push(1, dec(vec![]));
        v.push(3, dec(vec![]));
        assert_eq!(v.ready(t).iter().map(|f| f.0).collect::<Vec<_>>(), vec![1]);
        assert!(v.blocked());
        v.push(2, dec(vec![]));
        assert_eq!(v.ready(t).iter().map(|f| f.0).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn a_missing_frame_is_given_up_on() {
        let mut v = V2::new(0);
        let t = Instant::now();
        v.push(1, dec(vec![]));
        v.ready(t);
        v.push(3, dec(vec![]));
        assert!(v.ready(t).is_empty());
        assert_eq!(v.ready(t + GAP_WAIT).iter().map(|f| f.0).collect::<Vec<_>>(), vec![3]);
        // A full frame skips everything before it.
        v.push(5, dec(vec![]));
        v.skip_to(7);
        v.push(6, dec(vec![]));
        assert!(v.ready(t).is_empty());
    }

    #[test]
    fn copies_read_the_picture_before_the_frame_and_the_cache_keeps_tiles() {
        let mut v = V2::new(2);
        let mut f = Frame::new(128, 64);
        v.apply(&mut f, dec(vec![DTile { rect: r(0), pix: Pix::Pixels(px(5)), store: Some(1) }])).unwrap();
        // Swap the two tiles: tile 1 copies tile 0, tile 0 gets new pixels, in the same frame.
        let swap = vec![
            DTile { rect: r(0), pix: Pix::Pixels(px(9)), store: None },
            DTile { rect: r(1), pix: Pix::Copy { x: 0, y: 0 }, store: None },
        ];
        v.apply(&mut f, dec(swap)).unwrap();
        assert_eq!((f.data[0], f.data[64 * 4]), (9, 5));
        // From the cache.
        v.apply(&mut f, dec(vec![DTile { rect: r(0), pix: Pix::Cached(1), store: None }])).unwrap();
        assert_eq!(f.data[0], 5);
        assert!(v.apply(&mut f, dec(vec![DTile { rect: r(0), pix: Pix::Cached(0), store: None }])).is_err());
        assert!(v.apply(&mut f, dec(vec![DTile { rect: r(0), pix: Pix::Cached(7), store: None }])).is_err());
    }
}
