//! Tile codec: the screen is cut into 64×64 tiles; each changed tile is sent as
//! a solid color, lossless zstd, or JPEG (while it is changing quickly).
//! Pixels are tightly packed RGBA everywhere.

use anyhow::{Result, bail, ensure};
use rdesk_proto::{TILE, Tile, TileEncoding};

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
        for px in data.chunks_exact_mut(4) {
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
    let first: [u8; 4] = px[..4].try_into().ok()?;
    px.chunks_exact(4).all(|p| p == first).then_some(first)
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
