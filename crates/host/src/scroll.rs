//! Scrolling: content that moved up or down since the previous capture is sent as a copy
//! of what the client already shows ([`rdesk_proto::TileData::Copy`]) instead of new pixels
//! (VNC's CopyRect, RDP's ScrBlt).
//!
//! Each column of tiles (a 64-pixel band) is hashed row by row; rows of the new picture that
//! changed vote for the offset at which the old picture had them. The winning offsets are
//! only candidates: a tile becomes a copy only if its pixels match exactly.

use std::collections::HashMap;

use rayon::prelude::*;
use rdesk_common::tiles::{Frame, TileRect};
use rdesk_proto::TILE;

/// Row hashes of the bands looked at so far (`bands[col][row]`), worked out on demand.
pub struct RowHashes {
    pub width: u32,
    pub height: u32,
    bands: Vec<Option<Vec<u64>>>,
}

/// A row hash that is fast and good enough to find candidates (they are verified anyway).
fn hash_row(px: &[u8]) -> u64 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15 ^ px.len() as u64;
    let (chunks, rest) = px.as_chunks::<8>();
    for c in chunks {
        h = (h ^ u64::from_le_bytes(*c)).wrapping_mul(0x0100_0000_01B3).rotate_left(29);
    }
    for &b in rest {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01B3);
    }
    h
}

impl RowHashes {
    /// None of `frame`'s bands hashed yet.
    pub fn new(frame: &Frame) -> Self {
        Self { width: frame.width, height: frame.height, bands: vec![None; frame.width.div_ceil(TILE) as usize] }
    }

    /// Every band of `frame` hashed.
    pub fn of(frame: &Frame) -> Self {
        let mut r = Self::new(frame);
        r.fill(frame, &vec![true; r.bands.len()]);
        r
    }

    /// Hash the bands of `frame` marked in `cols` that are not hashed yet.
    pub fn fill(&mut self, frame: &Frame, cols: &[bool]) {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let todo: Vec<usize> = (0..self.bands.len())
            .filter(|&c| cols.get(c).copied().unwrap_or(false) && self.bands[c].is_none())
            .collect();
        let done: Vec<(usize, Vec<u64>)> = todo
            .into_par_iter()
            .map(|c| {
                let x0 = c * TILE as usize;
                let x1 = (x0 + TILE as usize).min(w);
                (c, (0..h).map(|y| hash_row(&frame.data[(y * w + x0) * 4..(y * w + x1) * 4])).collect())
            })
            .collect();
        for (c, b) in done {
            self.bands[c] = Some(b);
        }
    }

    fn same_size(&self, o: &RowHashes) -> bool {
        self.width == o.width && self.height == o.height
    }
}

/// Fewest changed rows that must agree on an offset for it to count.
const MIN_VOTES: u32 = 8;
/// Rows whose hash repeats more often than this in the old band are ignored (blank lines
/// match everywhere and would vote for every offset).
const MAX_REPEATS: usize = 2;

/// The vertical offset (`old row - new row`) most of each band's changed rows moved by, if
/// any (`out[col]`), plus the one most bands agree on.
pub fn candidates(old: &RowHashes, new: &RowHashes, changed_cols: &[bool]) -> (Vec<Option<i32>>, Option<i32>) {
    if !old.same_size(new) {
        return (vec![None; new.bands.len()], None);
    }
    let per_band: Vec<Option<(i32, u32)>> = (0..new.bands.len())
        .into_par_iter()
        .map(|c| {
            if !changed_cols.get(c).copied().unwrap_or(false) {
                return None;
            }
            let (Some(ob), Some(nb)) = (&old.bands[c], &new.bands[c]) else { return None };
            let mut at: HashMap<u64, Vec<u32>> = HashMap::with_capacity(ob.len());
            for (y, h) in ob.iter().enumerate() {
                let v = at.entry(*h).or_default();
                if v.len() <= MAX_REPEATS {
                    v.push(y as u32);
                }
            }
            let mut votes: HashMap<i32, u32> = HashMap::new();
            for (y, h) in nb.iter().enumerate() {
                if ob[y] == *h {
                    continue;
                }
                if let Some(rows) = at.get(h).filter(|r| r.len() <= MAX_REPEATS) {
                    for &oy in rows {
                        *votes.entry(oy as i32 - y as i32).or_default() += 1;
                    }
                }
            }
            votes.into_iter().filter(|&(dy, n)| dy != 0 && n >= MIN_VOTES).max_by_key(|&(dy, n)| (n, -dy.abs()))
        })
        .collect();
    let mut total: HashMap<i32, u32> = HashMap::new();
    for (dy, n) in per_band.iter().flatten() {
        *total.entry(*dy).or_default() += n;
    }
    let global = total.into_iter().max_by_key(|&(dy, n)| (n, -dy.abs())).map(|(dy, _)| dy);
    (per_band.into_iter().map(|b| b.map(|(dy, _)| dy)).collect(), global)
}

/// Whether `new`'s tile `r` equals `old` at the same column, `dy` rows down (the source
/// fully inside the picture).
pub fn matches(old: &Frame, new: &Frame, r: TileRect, dy: i32) -> bool {
    if old.width != new.width || old.height != new.height {
        return false;
    }
    let sy = r.y as i64 + dy as i64;
    if sy < 0 || sy + r.h as i64 > old.height as i64 {
        return false;
    }
    let stride = new.width as usize * 4;
    let (x, len) = (r.x as usize * 4, r.w as usize * 4);
    (0..r.h as usize).all(|i| {
        let n = (r.y as usize + i) * stride + x;
        let o = (sy as usize + i) * stride + x;
        new.data[n..n + len] == old.data[o..o + len]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of "text": each row's content depends on its document line, so a scroll moves them.
    pub fn page(w: u32, h: u32, scroll: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            let line = y + scroll;
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = (line.wrapping_mul(2654435761) >> 7 ^ x.wrapping_mul(40503)) as u8;
                f.data[i..i + 3].copy_from_slice(&[v, v.wrapping_mul(3), line as u8]);
            }
        }
        f
    }

    #[test]
    fn finds_a_scroll_and_verifies_tiles() {
        let (old, new) = (page(256, 256, 0), page(256, 256, 40));
        let (bands, global) = candidates(&RowHashes::of(&old), &RowHashes::of(&new), &[true; 4]);
        assert_eq!(global, Some(40));
        assert!(bands.iter().all(|b| *b == Some(40)));
        let top = TileRect { x: 64, y: 0, w: 64, h: 64 };
        assert!(matches(&old, &new, top, 40));
        assert!(!matches(&old, &new, top, 39));
        // The bottom tiles come from below the old picture: no copy.
        let bottom = TileRect { x: 0, y: 192, w: 64, h: 64 };
        assert!(!matches(&old, &new, bottom, 40));
    }

    #[test]
    fn no_candidates_when_nothing_moved() {
        let (old, new) = (page(128, 128, 0), page(128, 128, 0));
        let (_, global) = candidates(&RowHashes::of(&old), &RowHashes::of(&new), &[true; 2]);
        assert_eq!(global, None);
        let (_, global) = candidates(&RowHashes::of(&old), &RowHashes::of(&page(128, 64, 0)), &[true; 2]);
        assert_eq!(global, None, "another size");
    }
}
