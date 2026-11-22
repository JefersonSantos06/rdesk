//! The pointer's image, for clients that show it as their own pointer over the picture
//! ([`rdesk_proto::ExtHost::CursorShape`]): an I-beam over text, arrows on window edges, a
//! hand on links, with no delay since the client draws it where its own mouse is.
//!
//! Read on the capture thread (which follows the input desktop on Windows). Windows only
//! for now; elsewhere the feature is not offered and clients keep their arrow.

use rdesk_proto::CursorShape;

/// Whether this computer can tell the pointer's image.
pub fn supported() -> bool {
    cfg!(windows)
}

/// The pointer now: an id that changes when its image does (0: hidden) and the image.
pub type Current = (u64, Option<CursorShape>);

/// Shrink an image wider or taller than [`CursorShape::MAX`] (nearest pixel).
pub fn fit(shape: CursorShape) -> CursorShape {
    let max = CursorShape::MAX as u32;
    let (w, h) = (shape.width as u32, shape.height as u32);
    if w <= max && h <= max {
        return shape;
    }
    let s = (max as f32 / w as f32).min(max as f32 / h as f32);
    let (nw, nh) = (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1));
    let mut rgba = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        for x in 0..nw {
            let (sx, sy) = ((x * w / nw).min(w - 1), (y * h / nh).min(h - 1));
            let i = ((sy * w + sx) * 4) as usize;
            rgba.extend_from_slice(&shape.rgba[i..i + 4]);
        }
    }
    CursorShape {
        width: nw as u16,
        height: nh as u16,
        hot_x: (shape.hot_x as f32 * s) as u16,
        hot_y: (shape.hot_y as f32 * s) as u16,
        rgba,
    }
}

/// An id for an image (its content), never 0.
pub fn id_of(shape: &CursorShape) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(
        &[shape.width.to_le_bytes(), shape.height.to_le_bytes(), shape.hot_x.to_le_bytes(), shape.hot_y.to_le_bytes()]
            .concat(),
    );
    h.update(&shape.rgba);
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap()).max(1)
}

#[cfg(windows)]
pub use win::Reader;

#[cfg(not(windows))]
#[derive(Default)]
pub struct Reader;

#[cfg(not(windows))]
impl Reader {
    pub fn read(&mut self) -> Option<Current> {
        None
    }
}

#[cfg(windows)]
mod win {
    use std::mem::{size_of, zeroed};

    use rdesk_proto::CursorShape;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
        HBITMAP, ReleaseDC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, HCURSOR, ICONINFO,
    };

    use super::Current;

    /// Remembers the last pointer, so an unchanged one is not read again.
    #[derive(Default)]
    pub struct Reader {
        last: Option<(HCURSOR, u64)>,
    }

    impl Reader {
        pub fn read(&mut self) -> Option<Current> {
            // SAFETY: plain Win32 calls on a zeroed, sized struct.
            let mut ci: CURSORINFO = unsafe { zeroed() };
            ci.cbSize = size_of::<CURSORINFO>() as u32;
            if unsafe { GetCursorInfo(&mut ci) } == 0 {
                return None;
            }
            if ci.flags & CURSOR_SHOWING == 0 || ci.hCursor.is_null() {
                self.last = None;
                return Some((0, None));
            }
            if let Some((h, id)) = self.last
                && h == ci.hCursor
            {
                return Some((id, None));
            }
            let shape = super::fit(image(ci.hCursor)?);
            let id = super::id_of(&shape);
            self.last = Some((ci.hCursor, id));
            Some((id, Some(shape)))
        }
    }

    /// The bitmap's pixels as top-down BGRA, and its size.
    fn bits(bmp: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
        // SAFETY: GetObjectW fills a BITMAP of the size given; GetDIBits writes at most
        // width * height * 4 bytes for a 32-bit top-down request.
        unsafe {
            let mut b: BITMAP = zeroed();
            if GetObjectW(bmp, size_of::<BITMAP>() as i32, &mut b as *mut _ as *mut _) == 0 {
                return None;
            }
            let (w, h) = (b.bmWidth as u32, b.bmHeight as u32);
            if w == 0 || h == 0 || w > 1024 || h > 1024 {
                return None;
            }
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..zeroed()
            };
            let mut out = vec![0u8; (w * h * 4) as usize];
            let dc = GetDC(std::ptr::null_mut());
            let lines = GetDIBits(dc, bmp, 0, h, out.as_mut_ptr() as *mut _, &mut info, DIB_RGB_COLORS);
            ReleaseDC(std::ptr::null_mut(), dc);
            (lines as u32 == h).then_some((out, w, h))
        }
    }

    fn image(cursor: HCURSOR) -> Option<CursorShape> {
        // SAFETY: GetIconInfo fills ICONINFO and hands us two bitmaps we must delete.
        let mut ii: ICONINFO = unsafe { zeroed() };
        if unsafe { GetIconInfo(cursor, &mut ii) } == 0 {
            return None;
        }
        let color = (!ii.hbmColor.is_null()).then(|| bits(ii.hbmColor)).flatten();
        let mask = (!ii.hbmMask.is_null()).then(|| bits(ii.hbmMask)).flatten();
        unsafe {
            if !ii.hbmColor.is_null() {
                DeleteObject(ii.hbmColor);
            }
            if !ii.hbmMask.is_null() {
                DeleteObject(ii.hbmMask);
            }
        }
        let (hot_x, hot_y) = (ii.xHotspot as u16, ii.yHotspot as u16);
        let (rgba, w, h) = match (color, mask) {
            (Some((bgra, w, h)), mask) => (colour_cursor(&bgra, mask.as_ref().map(|m| &m.0[..]), w, h), w, h),
            // Monochrome: the mask is twice as tall, AND above XOR.
            (None, Some((m, w, h2))) => {
                let h = h2 / 2;
                (mono_cursor(&m, w, h), w, h)
            }
            (None, None) => return None,
        };
        Some(CursorShape { width: w as u16, height: h as u16, hot_x, hot_y, rgba })
    }

    /// A color pointer (BGRA). Old ones have no alpha: the AND mask says what is transparent.
    pub(super) fn colour_cursor(bgra: &[u8], mask: Option<&[u8]>, w: u32, h: u32) -> Vec<u8> {
        let has_alpha = bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for (i, p) in bgra.as_chunks::<4>().0.iter().enumerate() {
            let a = if has_alpha {
                p[3]
            } else {
                match mask {
                    // A white AND pixel (all bits set) leaves the screen as is: transparent.
                    Some(m) if m.get(i * 4).copied().unwrap_or(0) != 0 => 0,
                    _ => 255,
                }
            };
            out.extend_from_slice(&[p[2], p[1], p[0], a]);
        }
        out
    }

    /// A black-and-white pointer: AND 0 / XOR 0 black, AND 0 / XOR 1 white, AND 1 / XOR 0
    /// transparent, AND 1 / XOR 1 inverts the screen (drawn black, which reads on light
    /// backgrounds — the I-beam is the common case).
    pub(super) fn mono_cursor(mask: &[u8], w: u32, h: u32) -> Vec<u8> {
        let px = |i: usize| mask.get(i * 4).copied().unwrap_or(0) != 0;
        let n = (w * h) as usize;
        let mut out = Vec::with_capacity(n * 4);
        for i in 0..n {
            let (and, xor) = (px(i), px(i + n));
            out.extend_from_slice(match (and, xor) {
                (false, false) => &[0, 0, 0, 255],
                (false, true) => &[255, 255, 255, 255],
                (true, false) => &[0, 0, 0, 0],
                (true, true) => &[0, 0, 0, 255],
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(w: u16, h: u16) -> CursorShape {
        CursorShape { width: w, height: h, hot_x: w / 2, hot_y: h / 2, rgba: vec![7; w as usize * h as usize * 4] }
    }

    #[test]
    fn big_pointers_are_shrunk_and_ids_follow_content() {
        let s = fit(shape(192, 96));
        assert_eq!((s.width, s.height, s.hot_x, s.hot_y), (96, 48, 48, 24));
        assert_eq!(s.rgba.len(), 96 * 48 * 4);
        let small = shape(32, 32);
        assert_eq!(fit(small.clone()), small);
        assert_eq!(id_of(&small), id_of(&shape(32, 32)));
        assert_ne!(id_of(&small), id_of(&shape(32, 31)));
    }

    #[cfg(windows)]
    #[test]
    fn monochrome_and_maskless_pointers() {
        // 2×1: AND (white, black) then XOR (white, white) → inverted (drawn black), white.
        let mask = [255u8, 255, 255, 0, 0, 0, 0, 0, 255, 255, 255, 0, 255, 255, 255, 0];
        assert_eq!(win::mono_cursor(&mask, 2, 1), vec![0, 0, 0, 255, 255, 255, 255, 255]);
        // No alpha in the color bitmap: the mask decides.
        let bgra = [1, 2, 3, 0, 4, 5, 6, 0];
        let and = [255u8, 255, 255, 0, 0, 0, 0, 0];
        assert_eq!(win::colour_cursor(&bgra, Some(&and), 2, 1), vec![3, 2, 1, 0, 6, 5, 4, 255]);
    }
}
