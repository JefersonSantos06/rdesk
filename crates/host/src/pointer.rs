//! The pointer where the screen capture leaves it out (Wayland, with the cursor sent as
//! metadata beside the picture): its image, drawn back into the picture for clients that
//! show the picture as it is, and left out for clients that draw the pointer themselves
//! from its position (the phone app, see [`client_draws`]).

use std::sync::atomic::{AtomicBool, Ordering};

use rdesk_common::tiles::Frame;

/// The pointer's image, tightly packed RGBA (straight alpha).
#[derive(Debug, Clone, PartialEq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// The point of the image that sits at the pointer's position.
    pub hotspot: (i32, i32),
    pub rgba: Vec<u8>,
}

/// How a cursor bitmap's bytes are ordered (per pixel, in memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapOrder {
    Rgba,
    Bgra,
    Argb,
    Abgr,
}

static CLIENT_DRAWS: AtomicBool = AtomicBool::new(false);

/// The connected client draws the pointer itself (it follows the position): leave it out
/// of the picture, or it would show twice.
pub fn set_client_draws(on: bool) {
    CLIENT_DRAWS.store(on, Ordering::Relaxed);
}

pub fn client_draws() -> bool {
    CLIENT_DRAWS.load(Ordering::Relaxed)
}

/// A bitmap of `width`×`height` with rows `stride` bytes apart → [`CursorImage`] pixels.
pub fn bitmap_to_rgba(order: BitmapOrder, width: u32, height: u32, stride: usize, data: &[u8]) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || stride < w * 4 || data.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for px in data[row * stride..row * stride + w * 4].as_chunks::<4>().0 {
            let [r, g, b, a] = match order {
                BitmapOrder::Rgba => [px[0], px[1], px[2], px[3]],
                BitmapOrder::Bgra => [px[2], px[1], px[0], px[3]],
                BitmapOrder::Argb => [px[1], px[2], px[3], px[0]],
                BitmapOrder::Abgr => [px[3], px[2], px[1], px[0]],
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

/// Draw `img` over `frame` with its hotspot at (`x`, `y`) (frame pixels), clipped to the frame.
pub fn blend(frame: &mut Frame, img: &CursorImage, x: i32, y: i32) {
    let (fw, fh) = (frame.width as i64, frame.height as i64);
    let (left, top) = (x as i64 - img.hotspot.0 as i64, y as i64 - img.hotspot.1 as i64);
    for row in 0..img.height as i64 {
        let fy = top + row;
        if fy < 0 || fy >= fh {
            continue;
        }
        for col in 0..img.width as i64 {
            let fx = left + col;
            if fx < 0 || fx >= fw {
                continue;
            }
            let s = ((row * img.width as i64 + col) * 4) as usize;
            let a = img.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((fy * fw + fx) * 4) as usize;
            for c in 0..3 {
                let (src, dst) = (img.rgba[s + c] as u32, frame.data[d + c] as u32);
                frame.data[d + c] = ((src * a + dst * (255 - a) + 127) / 255) as u8;
            }
            frame.data[d + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrow() -> CursorImage {
        // 2×2: opaque white, transparent, half-transparent red, opaque black.
        CursorImage {
            width: 2,
            height: 2,
            hotspot: (0, 0),
            rgba: vec![255, 255, 255, 255, 9, 9, 9, 0, 255, 0, 0, 128, 0, 0, 0, 255],
        }
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        f.data[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn blends_with_alpha_and_hotspot() {
        let mut f = Frame::new(4, 4); // opaque black
        let mut img = arrow();
        img.hotspot = (1, 1);
        blend(&mut f, &img, 2, 2);
        assert_eq!(px(&f, 1, 1), [255, 255, 255, 255], "hotspot (1,1) at (2,2): image starts at (1,1)");
        assert_eq!(px(&f, 2, 1), [0, 0, 0, 255], "transparent pixel leaves the picture alone");
        assert_eq!(px(&f, 1, 2), [128, 0, 0, 255], "half red over black");
        assert_eq!(px(&f, 0, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn clipped_at_the_edges() {
        let mut f = Frame::new(2, 2);
        blend(&mut f, &arrow(), 1, 1);
        assert_eq!(px(&f, 1, 1), [255, 255, 255, 255]);
        blend(&mut f, &arrow(), -1, -1);
        blend(&mut f, &arrow(), 5, 5);
        assert_eq!(px(&f, 0, 0), [0, 0, 0, 255], "the black pixel of the image at (-1,-1)+(1,1)");
    }

    #[test]
    fn bitmap_orders() {
        let one = |order, bytes: [u8; 4]| bitmap_to_rgba(order, 1, 1, 4, &bytes).unwrap();
        assert_eq!(one(BitmapOrder::Rgba, [1, 2, 3, 4]), [1, 2, 3, 4]);
        assert_eq!(one(BitmapOrder::Bgra, [3, 2, 1, 4]), [1, 2, 3, 4]);
        assert_eq!(one(BitmapOrder::Argb, [4, 1, 2, 3]), [1, 2, 3, 4]);
        assert_eq!(one(BitmapOrder::Abgr, [4, 3, 2, 1]), [1, 2, 3, 4]);
        // Rows with padding; too short a buffer is refused.
        let padded = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        assert_eq!(bitmap_to_rgba(BitmapOrder::Rgba, 1, 2, 6, &padded).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(bitmap_to_rgba(BitmapOrder::Rgba, 2, 2, 8, &[0; 10]).is_none());
    }
}
