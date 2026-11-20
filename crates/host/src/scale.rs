//! Scaling the picture down for small screens (a phone asks for at most its own size):
//! an area average, so text stays as legible as the smaller size allows.

use rayon::prelude::*;
use rdesk_common::tiles::Frame;

/// The size `width`×`height` becomes inside `max_w`×`max_h` (aspect kept), or `None`
/// when it already fits or there is no limit (0×0).
pub fn fit(width: u32, height: u32, max_w: u32, max_h: u32) -> Option<(u32, u32)> {
    if max_w == 0 || max_h == 0 || (width <= max_w && height <= max_h) || width == 0 || height == 0 {
        return None;
    }
    let s = (max_w as f64 / width as f64).min(max_h as f64 / height as f64);
    let w = ((width as f64 * s).floor() as u32).clamp(1, max_w);
    let h = ((height as f64 * s).floor() as u32).clamp(1, max_h);
    Some((w, h))
}

/// `src` scaled to `w`×`h` (smaller on both axes), each pixel the average of the source
/// pixels it covers.
pub fn downscale(src: &Frame, w: u32, h: u32) -> Frame {
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (w, h) = (w as usize, h as usize);
    let span = |i: usize, n: usize, of: usize| (i * of / n, ((i + 1) * of / n).max(i * of / n + 1).min(of));
    let cols: Vec<(usize, usize)> = (0..w).map(|x| span(x, w, sw)).collect();
    let mut data = vec![0u8; w * h * 4];
    data.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let (y0, y1) = span(y, h, sh);
        let mut acc = vec![[0u32; 4]; w];
        for sy in y0..y1 {
            let line = &src.data[sy * sw * 4..(sy + 1) * sw * 4];
            for (a, &(x0, x1)) in acc.iter_mut().zip(&cols) {
                for px in line[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    a[0] += px[0] as u32;
                    a[1] += px[1] as u32;
                    a[2] += px[2] as u32;
                    a[3] += px[3] as u32;
                }
            }
        }
        for ((out, a), &(x0, x1)) in row.as_chunks_mut::<4>().0.iter_mut().zip(&acc).zip(&cols) {
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            for c in 0..4 {
                out[c] = ((a[c] + n / 2) / n) as u8;
            }
        }
    });
    Frame { width: w as u32, height: h as u32, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_inside_keeping_the_aspect() {
        assert_eq!(fit(3840, 2160, 1920, 1080), Some((1920, 1080)));
        assert_eq!(fit(3840, 2160, 1280, 1280), Some((1280, 720)));
        assert_eq!(fit(1920, 1080, 2400, 1080), None, "already fits");
        assert_eq!(fit(1920, 1080, 0, 0), None, "no limit");
        assert_eq!(fit(2560, 1600, 1170, 2532), Some((1170, 731)));
    }

    #[test]
    fn halving_averages_each_block() {
        let mut f = Frame::new(4, 2);
        // Left 2×2 block: black and white pixels → mid gray; right block stays white.
        for (i, px) in f.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = if i % 4 >= 2 || i == 1 || i == 4 { 255 } else { 0 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let d = downscale(&f, 2, 1);
        assert_eq!((d.width, d.height), (2, 1));
        assert_eq!(&d.data[..4], &[128, 128, 128, 255]);
        assert_eq!(&d.data[4..], &[255, 255, 255, 255]);
    }

    #[test]
    fn odd_sizes_cover_every_pixel() {
        let f = Frame::new(1366, 768);
        let (w, h) = fit(1366, 768, 1000, 1000).unwrap();
        let d = downscale(&f, w, h);
        assert_eq!(d.data.len(), (w * h * 4) as usize);
        assert!(d.data.as_chunks::<4>().0.iter().all(|p| *p == [0, 0, 0, 255]));
    }
}
