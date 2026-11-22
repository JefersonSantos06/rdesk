//! Tile codec: the screen is cut into 64×64 tiles; each changed tile is sent as
//! a solid color, lossless zstd, or JPEG (while it is changing quickly).
//! Pixels are tightly packed RGBA everywhere.

use std::collections::HashMap;

use anyhow::{Result, bail, ensure};
use rdesk_proto::{TILE, Tile, TileData, TileEncoding};

pub type TileHash = [u8; 16];

/// A captured or reconstructed screen image (tightly packed RGBA).
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        // Opaque black.
        for px in data.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Self { width, height, data }
    }

    pub fn grid(&self) -> (u32, u32) {
        grid(self.width, self.height)
    }

    /// Hash a tile in place (no copy).
    pub fn hash_tile(&self, r: TileRect) -> TileHash {
        let mut h = blake3::Hasher::new();
        let stride = self.width as usize * 4;
        for row in r.y..r.y + r.h {
            let start = row as usize * stride + r.x as usize * 4;
            h.update(&self.data[start..start + r.w as usize * 4]);
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.finalize().as_bytes()[..16]);
        out
    }

    /// Copy a tile out into a tight buffer.
    pub fn extract(&self, r: TileRect, out: &mut Vec<u8>) {
        out.clear();
        let stride = self.width as usize * 4;
        for row in r.y..r.y + r.h {
            let start = row as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&self.data[start..start + r.w as usize * 4]);
        }
    }

    /// Copy a tight tile buffer into the frame.
    pub fn blit(&mut self, r: TileRect, src: &[u8]) -> Result<()> {
        ensure!(
            r.x + r.w <= self.width && r.y + r.h <= self.height,
            "tile {r:?} outside {}x{}",
            self.width,
            self.height
        );
        ensure!(src.len() == r.w as usize * r.h as usize * 4, "tile data has wrong size");
        let stride = self.width as usize * 4;
        let row_len = r.w as usize * 4;
        for (i, row) in (r.y..r.y + r.h).enumerate() {
            let dst = row as usize * stride + r.x as usize * 4;
            self.data[dst..dst + row_len].copy_from_slice(&src[i * row_len..(i + 1) * row_len]);
        }
        Ok(())
    }

    /// Decode a tile message and write it into the frame.
    pub fn apply(&mut self, tile: &Tile, scratch: &mut Vec<u8>) -> Result<TileRect> {
        let r = TileRect { x: tile.x, y: tile.y, w: tile.w as u32, h: tile.h as u32 };
        decode_tile(tile, scratch)?;
        self.blit(r, scratch)?;
        Ok(r)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Number of tile columns and rows covering a `width`×`height` image.
pub fn grid(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(TILE), height.div_ceil(TILE))
}

pub fn tile_rect(col: u32, row: u32, width: u32, height: u32) -> TileRect {
    let x = col * TILE;
    let y = row * TILE;
    TileRect { x, y, w: TILE.min(width - x), h: TILE.min(height - y) }
}

fn solid_color(px: &[u8]) -> Option<[u8; 4]> {
    let pixels = px.as_chunks::<4>().0;
    let first = *pixels.first()?;
    pixels.iter().all(|p| *p == first).then_some(first)
}

/// Encode a tight RGBA tile. `lossy` selects JPEG (for content in motion).
pub fn encode_tile(r: TileRect, px: &[u8], lossy: bool, jpeg_quality: u8) -> Result<Tile> {
    let (enc, data) = if let Some(c) = solid_color(px) {
        (TileEncoding::Solid, c.to_vec())
    } else if lossy {
        let mut out = Vec::with_capacity(px.len() / 8);
        let mut enc = jpeg_encoder::Encoder::new(&mut out, jpeg_quality.clamp(10, 100));
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
        enc.encode(px, r.w as u16, r.h as u16, jpeg_encoder::ColorType::Rgba)?;
        (TileEncoding::Jpeg, out)
    } else {
        (TileEncoding::Zstd, zstd::bulk::compress(px, 1)?)
    };
    Ok(Tile { x: r.x, y: r.y, w: r.w as u16, h: r.h as u16, enc, data })
}

/// zstd level for lossless tiles of the second format (a better ratio than the first
/// format's level 1, still well under a millisecond per tile).
const ZSTD_LEVEL: i32 = 3;

/// Encode a tight RGBA tile in the second format ([`TileData`]): JPEG when `lossy`, else
/// the smallest lossless form that suits it (one color, a palette, filtered RGB). Alpha is
/// not kept (the picture is opaque).
pub fn encode_tile2(w: u32, h: u32, px: &[u8], lossy: bool, jpeg_quality: u8) -> Result<TileData> {
    let pixels = px.as_chunks::<4>().0;
    ensure!(pixels.len() == (w * h) as usize, "tile data has wrong size");
    if let Some(c) = solid_color(px) {
        return Ok(TileData::Solid([c[0], c[1], c[2]]));
    }
    if lossy {
        let mut out = Vec::with_capacity(px.len() / 8);
        let mut enc = jpeg_encoder::Encoder::new(&mut out, jpeg_quality.clamp(10, 100));
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
        enc.encode(px, w as u16, h as u16, jpeg_encoder::ColorType::Rgba)?;
        return Ok(TileData::Jpeg(out));
    }
    if let Some(raw) = palette_raw(w as usize, h as usize, pixels) {
        return Ok(TileData::Palette(zstd::bulk::compress(&raw, ZSTD_LEVEL)?));
    }
    Ok(TileData::Rgb(zstd::bulk::compress(&rgb_filtered(w as usize, pixels), ZSTD_LEVEL)?))
}

/// Bits per palette index for `n` colors.
fn index_bits(n: usize) -> usize {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// The palette form (before zstd), if the tile has at most 256 colors.
fn palette_raw(w: usize, h: usize, pixels: &[[u8; 4]]) -> Option<Vec<u8>> {
    let mut colors: Vec<[u8; 3]> = Vec::with_capacity(16);
    let mut index: HashMap<[u8; 3], u8> = HashMap::with_capacity(64);
    let mut idx = Vec::with_capacity(pixels.len());
    let mut last: Option<([u8; 3], u8)> = None;
    for p in pixels {
        let c = [p[0], p[1], p[2]];
        let i = match last {
            Some((lc, li)) if lc == c => li,
            _ => match index.get(&c) {
                Some(&i) => i,
                None => {
                    if colors.len() == 256 {
                        return None;
                    }
                    let i = colors.len() as u8;
                    colors.push(c);
                    index.insert(c, i);
                    i
                }
            },
        };
        last = Some((c, i));
        idx.push(i);
    }
    let bits = index_bits(colors.len());
    let row_bytes = (w * bits).div_ceil(8);
    let mut out = Vec::with_capacity(1 + colors.len() * 3 + row_bytes * h);
    out.push((colors.len() - 1) as u8);
    for c in &colors {
        out.extend_from_slice(c);
    }
    let per_byte = 8 / bits;
    for row in idx.chunks_exact(w) {
        for group in row.chunks(per_byte) {
            let mut b = 0u8;
            for (k, &i) in group.iter().enumerate() {
                b |= i << (8 - bits * (k + 1));
            }
            out.push(b);
        }
    }
    Some(out)
}

fn palette_decode(raw: &[u8], w: usize, h: usize, out: &mut Vec<u8>) -> Result<()> {
    ensure!(!raw.is_empty(), "empty palette tile");
    let n = raw[0] as usize + 1;
    ensure!(raw.len() > n * 3, "palette tile too short");
    let colors = raw[1..1 + n * 3].as_chunks::<3>().0;
    let bits = index_bits(n);
    let row_bytes = (w * bits).div_ceil(8);
    let data = &raw[1 + n * 3..];
    ensure!(
        data.len() == row_bytes * h,
        "palette tile has {} bytes of indexes, expected {}",
        data.len(),
        row_bytes * h
    );
    out.clear();
    out.reserve(w * h * 4);
    let mask = ((1u16 << bits) - 1) as u8;
    for row in data.chunks_exact(row_bytes) {
        for x in 0..w {
            let bit = x * bits;
            let i = (row[bit / 8] >> (8 - bits - bit % 8)) & mask;
            let c = colors.get(i as usize).ok_or_else(|| anyhow::anyhow!("palette index {i} out of range"))?;
            out.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    Ok(())
}

/// The value a pixel's channel is predicted to have from its neighbors (left `a`, above
/// `b`, above-left `c`): the median edge detector of JPEG-LS.
fn med(a: u8, b: u8, c: u8) -> u8 {
    let (lo, hi) = (a.min(b), a.max(b));
    if c >= hi {
        lo
    } else if c <= lo {
        hi
    } else {
        (a as i16 + b as i16 - c as i16) as u8
    }
}

/// The neighbors of pixel `i` (left, above, above-left) in a tile `w` wide: in the first row
/// only the left one counts, in the first column only the one above.
fn neighbors(px: impl Fn(usize) -> [u8; 3], i: usize, w: usize) -> ([u8; 3], [u8; 3], [u8; 3]) {
    let (x, y) = (i % w, i / w);
    match (x, y) {
        (0, 0) => ([0; 3], [0; 3], [0; 3]),
        (_, 0) => (px(i - 1), px(i - 1), px(i - 1)),
        (0, _) => (px(i - w), px(i - w), px(i - w)),
        _ => (px(i - 1), px(i - w), px(i - w - 1)),
    }
}

/// RGB as prediction residuals: each channel minus its [`med`] prediction, then red and blue
/// minus green (they move together).
fn rgb_filtered(w: usize, pixels: &[[u8; 4]]) -> Vec<u8> {
    let px = |i: usize| [pixels[i][0], pixels[i][1], pixels[i][2]];
    let mut out = Vec::with_capacity(pixels.len() * 3);
    for i in 0..pixels.len() {
        let (a, b, c) = neighbors(px, i, w);
        let p = px(i);
        let r: [u8; 3] = std::array::from_fn(|k| p[k].wrapping_sub(med(a[k], b[k], c[k])));
        out.extend_from_slice(&[r[1], r[0].wrapping_sub(r[1]), r[2].wrapping_sub(r[1])]);
    }
    out
}

fn rgb_unfilter(raw: &[u8], w: usize, h: usize, out: &mut Vec<u8>) -> Result<()> {
    ensure!(raw.len() == w * h * 3, "rgb tile has {} bytes, expected {}", raw.len(), w * h * 3);
    out.clear();
    out.resize(w * h * 4, 255);
    for (i, d) in raw.as_chunks::<3>().0.iter().enumerate() {
        let (a, b, c) = {
            let o: &[u8] = out;
            neighbors(|j| [o[j * 4], o[j * 4 + 1], o[j * 4 + 2]], i, w)
        };
        let r = [d[1].wrapping_add(d[0]), d[0], d[2].wrapping_add(d[0])];
        for k in 0..3 {
            out[i * 4 + k] = r[k].wrapping_add(med(a[k], b[k], c[k]));
        }
    }
    Ok(())
}

/// Decode a second-format tile that carries its own pixels (not [`TileData::Cached`],
/// [`TileData::Copy`] or [`TileData::H264`], which need the client's state) into a tight
/// RGBA buffer of `w*h*4` bytes.
pub fn decode_tile2(data: &TileData, w: u32, h: u32, out: &mut Vec<u8>) -> Result<()> {
    let (w, h) = (w as usize, h as usize);
    ensure!(w > 0 && h > 0 && w <= TILE as usize && h <= TILE as usize, "bad tile size {w}x{h}");
    let len = w * h * 4;
    // A tile's zstd payload never decodes to more than its RGBA size (plus a palette).
    let cap = len + 1 + 256 * 3;
    match data {
        TileData::Solid(c) => {
            out.clear();
            out.reserve(len);
            for _ in 0..w * h {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        TileData::Zstd(z) => {
            *out = zstd::bulk::decompress(z, len)?;
            ensure!(out.len() == len, "zstd tile decoded to {} bytes, expected {len}", out.len());
        }
        TileData::Jpeg(j) => {
            decode_tile(&Tile { x: 0, y: 0, w: w as u16, h: h as u16, enc: TileEncoding::Jpeg, data: j.clone() }, out)?
        }
        TileData::Palette(z) => palette_decode(&zstd::bulk::decompress(z, cap)?, w, h, out)?,
        TileData::Rgb(z) => rgb_unfilter(&zstd::bulk::decompress(z, w * h * 3)?, w, h, out)?,
        TileData::Cached(_) | TileData::Copy { .. } | TileData::H264(_) => {
            bail!("this tile needs the client's picture or cache")
        }
    }
    Ok(())
}

/// Decode a tile into a tight RGBA buffer of `w*h*4` bytes.
pub fn decode_tile(tile: &Tile, out: &mut Vec<u8>) -> Result<()> {
    let w = tile.w as usize;
    let h = tile.h as usize;
    ensure!(w > 0 && h > 0 && w <= TILE as usize && h <= TILE as usize, "bad tile size {w}x{h}");
    let len = w * h * 4;
    match tile.enc {
        TileEncoding::Solid => {
            ensure!(tile.data.len() == 4, "solid tile must carry 4 bytes");
            out.clear();
            out.reserve(len);
            for _ in 0..w * h {
                out.extend_from_slice(&tile.data);
            }
        }
        TileEncoding::Zstd => {
            *out = zstd::bulk::decompress(&tile.data, len)?;
            ensure!(out.len() == len, "zstd tile decoded to {} bytes, expected {len}", out.len());
        }
        TileEncoding::Jpeg => {
            use zune_jpeg::zune_core::bytestream::ZCursor;
            use zune_jpeg::zune_core::colorspace::ColorSpace;
            use zune_jpeg::zune_core::options::DecoderOptions;
            let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
            let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(&tile.data[..]), opts);
            let px = match dec.decode() {
                Ok(px) => px,
                Err(e) => bail!("jpeg tile: {e:?}"),
            };
            ensure!(px.len() == len, "jpeg tile decoded to {} bytes, expected {len}", px.len());
            *out = px;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: u32, h: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                f.data[i] = (x * 7 + y) as u8;
                f.data[i + 1] = (x ^ y) as u8;
                f.data[i + 2] = (y * 3) as u8;
                f.data[i + 3] = 255;
            }
        }
        f
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse: f64 = a
            .iter()
            .zip(b)
            .enumerate()
            .filter(|(i, _)| i % 4 != 3)
            .map(|(_, (x, y))| (*x as f64 - *y as f64).powi(2))
            .sum::<f64>()
            / (a.len() as f64 * 0.75);
        if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
    }

    #[test]
    fn grid_covers_odd_sizes() {
        assert_eq!(grid(1920, 1080), (30, 17));
        let r = tile_rect(29, 16, 1920, 1080);
        assert_eq!((r.x, r.y, r.w, r.h), (1856, 1024, 64, 56));
        assert_eq!(grid(1, 1), (1, 1));
    }

    #[test]
    fn lossless_roundtrip_is_exact() {
        let src = pattern(150, 100);
        let mut dst = Frame::new(150, 100);
        let (cols, rows) = src.grid();
        let mut buf = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                let r = tile_rect(col, row, 150, 100);
                src.extract(r, &mut buf);
                let t = encode_tile(r, &buf, false, 70).unwrap();
                dst.apply(&t, &mut Vec::new()).unwrap();
            }
        }
        assert!(src.data == dst.data);
    }

    #[test]
    fn jpeg_roundtrip_is_close() {
        let src = pattern(64, 64);
        let r = tile_rect(0, 0, 64, 64);
        let mut buf = Vec::new();
        src.extract(r, &mut buf);
        let t = encode_tile(r, &buf, true, 80).unwrap();
        assert_eq!(t.enc, TileEncoding::Jpeg);
        let mut out = Vec::new();
        decode_tile(&t, &mut out).unwrap();
        let p = psnr(&buf, &out);
        assert!(p > 25.0, "psnr {p}");
    }

    #[test]
    fn jpeg_handles_edge_tiles() {
        let src = pattern(70, 70);
        let r = tile_rect(1, 1, 70, 70); // 6x6
        let mut buf = Vec::new();
        src.extract(r, &mut buf);
        let t = encode_tile(r, &buf, true, 80).unwrap();
        let mut out = Vec::new();
        decode_tile(&t, &mut out).unwrap();
        assert_eq!(out.len(), 6 * 6 * 4);
    }

    #[test]
    fn solid_tiles_are_tiny() {
        let f = Frame::new(64, 64);
        let r = tile_rect(0, 0, 64, 64);
        let mut buf = Vec::new();
        f.extract(r, &mut buf);
        let t = encode_tile(r, &buf, true, 80).unwrap();
        assert_eq!(t.enc, TileEncoding::Solid);
        assert_eq!(t.data, vec![0, 0, 0, 255]);
    }

    #[test]
    fn hash_detects_single_pixel_change() {
        let mut f = pattern(128, 128);
        let r = tile_rect(1, 1, 128, 128);
        let h1 = f.hash_tile(r);
        f.data[(100 * 128 + 100) * 4] ^= 1;
        assert_ne!(h1, f.hash_tile(r));
        assert_eq!(f.hash_tile(tile_rect(0, 0, 128, 128)), pattern(128, 128).hash_tile(tile_rect(0, 0, 128, 128)));
    }

    fn roundtrip2(px: &[u8], w: u32, h: u32) -> TileData {
        let d = encode_tile2(w, h, px, false, 70).unwrap();
        let mut out = Vec::new();
        decode_tile2(&d, w, h, &mut out).unwrap();
        assert!(out == px, "lossless round trip of {:?}", std::mem::discriminant(&d));
        d
    }

    #[test]
    fn second_format_lossless_forms_round_trip() {
        // Many colors: filtered RGB.
        let src = pattern(64, 64);
        assert!(matches!(roundtrip2(&src.data, 64, 64), TileData::Rgb(_)));
        // Edge tiles of odd sizes.
        let src = pattern(37, 5);
        roundtrip2(&src.data, 37, 5);
        // 2, 3, 5, 17 and 256 colors: every index width, odd widths too.
        for (n, w, h) in [(2u32, 64u32, 64u32), (3, 13, 7), (5, 33, 9), (17, 64, 64), (256, 64, 64)] {
            let mut px = Vec::new();
            for i in 0..w * h {
                let c = (i * 7 + i / w) % n;
                px.extend_from_slice(&[c as u8, (c * 3) as u8, 255 - c as u8, 255]);
            }
            assert!(matches!(roundtrip2(&px, w, h), TileData::Palette(_)), "{n} colors");
        }
        // One color.
        let px = [9u8, 8, 7, 255].repeat(64 * 64);
        assert_eq!(roundtrip2(&px, 64, 64), TileData::Solid([9, 8, 7]));
    }

    #[test]
    fn palette_is_smaller_than_the_first_format_for_text_like_tiles() {
        let mut px = Vec::new();
        for y in 0..64u32 {
            for x in 0..64u32 {
                let ink = (x / 3 + y / 5) % 4 == 0 && y % 12 < 9;
                px.extend_from_slice(if ink { &[20, 20, 20, 255] } else { &[250, 250, 250, 255] });
            }
        }
        let v1 = encode_tile(tile_rect(0, 0, 64, 64), &px, false, 70).unwrap().data.len();
        let TileData::Palette(v2) = roundtrip2(&px, 64, 64) else { panic!("expected a palette") };
        assert!(v2.len() < v1, "palette {} bytes, first format {v1}", v2.len());
    }

    #[test]
    fn second_format_rejects_malformed_tiles() {
        let mut out = Vec::new();
        assert!(decode_tile2(&TileData::Rgb(zstd::bulk::compress(&[1, 2, 3], 1).unwrap()), 64, 64, &mut out).is_err());
        assert!(decode_tile2(&TileData::Palette(zstd::bulk::compress(&[3, 1], 1).unwrap()), 8, 8, &mut out).is_err());
        assert!(decode_tile2(&TileData::Solid([1, 2, 3]), 65, 1, &mut out).is_err());
        assert!(decode_tile2(&TileData::Cached(1), 8, 8, &mut out).is_err());
        // An index past the palette.
        let raw = [0u8, 1, 2, 3, 0b1000_0000];
        assert!(decode_tile2(&TileData::Palette(zstd::bulk::compress(&raw, 1).unwrap()), 1, 1, &mut out).is_err());
    }

    #[test]
    fn rejects_malformed_tiles() {
        let mut f = Frame::new(64, 64);
        let bad = Tile { x: 32, y: 0, w: 64, h: 64, enc: TileEncoding::Solid, data: vec![1, 2, 3, 4] };
        assert!(f.apply(&bad, &mut Vec::new()).is_err());
        let bad = Tile { x: 0, y: 0, w: 64, h: 64, enc: TileEncoding::Zstd, data: vec![1, 2, 3] };
        assert!(f.apply(&bad, &mut Vec::new()).is_err());
        let bad = Tile { x: 0, y: 0, w: 0, h: 64, enc: TileEncoding::Solid, data: vec![1, 2, 3, 4] };
        assert!(f.apply(&bad, &mut Vec::new()).is_err());
    }
}
