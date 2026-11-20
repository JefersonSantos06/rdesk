//! The app icon, drawn in code (no image files to ship): a white monitor on a blue
//! rounded square.

/// RGBA pixels of a `size`×`size` icon.
pub fn rgba(size: u32) -> Vec<u8> {
    let n = size as usize;
    let mut out = vec![0u8; n * n * 4];
    let blue = [46u8, 125, 233, 255];
    let dark = [24u8, 70, 150, 255];
    let white = [255u8, 255, 255, 255];
    // Supersample 4×4 per pixel for smooth edges.
    const SS: usize = 4;
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0u32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let c = shade(fx, fy, blue, dark, white);
                    for i in 0..4 {
                        acc[i] += c[i] as u32;
                    }
                }
            }
            let px = &mut out[(y * n + x) * 4..][..4];
            for i in 0..4 {
                px[i] = (acc[i] / (SS * SS) as u32) as u8;
            }
        }
    }
    out
}

fn shade(x: f32, y: f32, blue: [u8; 4], dark: [u8; 4], white: [u8; 4]) -> [u8; 4] {
    if !rounded(x, y, 0.0, 0.0, 1.0, 1.0, 0.22) {
        return [0, 0, 0, 0];
    }
    // Screen bezel, screen, stand and base.
    if rounded(x, y, 0.16, 0.22, 0.84, 0.66, 0.06) {
        if rounded(x, y, 0.21, 0.27, 0.79, 0.61, 0.03) {
            return dark;
        }
        return white;
    }
    if (0.45..0.55).contains(&x) && (0.66..0.75).contains(&y) {
        return white;
    }
    if rounded(x, y, 0.32, 0.74, 0.68, 0.80, 0.03) {
        return white;
    }
    blue
}

fn rounded(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

#[cfg(test)]
mod tests {
    /// The Linux menu icon (`deploy/rdesk.png`, installed by the .deb) comes from here:
    /// `cargo test -p rdesk-client --lib export_menu_icon -- --ignored` after changing the drawing.
    #[test]
    #[ignore = "writes deploy/rdesk.png"]
    fn export_menu_icon() {
        let size = 256;
        let img = image::RgbaImage::from_raw(size, size, super::rgba(size)).unwrap();
        img.save(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/rdesk.png")).unwrap();
    }

    #[test]
    fn has_transparent_corners_and_opaque_middle() {
        let px = super::rgba(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert_eq!(px[3], 0, "corner is transparent");
        let mid = (16 * 32 + 16) * 4;
        assert_eq!(px[mid + 3], 255);
    }
}
