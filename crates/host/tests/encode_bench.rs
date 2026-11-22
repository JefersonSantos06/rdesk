//! Lossless tile sizes of this computer's screen, first format against the second.
//! Needs a real screen: `cargo test -p rdesk-host --release --test encode_bench -- --ignored --nocapture`.

use std::time::Instant;

use rdesk_common::tiles::{self, encode_tile, encode_tile2, tile_rect};
use rdesk_host::capture::Capturer;
use rdesk_proto::TileData;

#[test]
#[ignore = "needs a screen"]
fn lossless_sizes_on_this_screen() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let cap = Capturer::spawn(false);
    let monitors = rt.block_on(cap.monitors()).expect("monitors");
    let m = monitors.iter().find(|m| m.info.primary).unwrap_or(&monitors[0]);
    // The same picture every run (delete the file for a new one).
    let saved = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-frame.raw");
    let frame = match std::fs::read(&saved) {
        Ok(b) => {
            let w = u32::from_le_bytes(b[0..4].try_into().unwrap());
            let h = u32::from_le_bytes(b[4..8].try_into().unwrap());
            rdesk_common::tiles::Frame { width: w, height: h, data: b[8..].to_vec() }
        }
        Err(_) => {
            let f = rt.block_on(cap.grab(m.info.id)).expect("capture");
            let mut b = f.width.to_le_bytes().to_vec();
            b.extend_from_slice(&f.height.to_le_bytes());
            b.extend_from_slice(&f.data);
            std::fs::write(&saved, b).unwrap();
            f
        }
    };
    let (cols, rows) = tiles::grid(frame.width, frame.height);
    let (mut v1, mut v2, mut jpeg) = (0usize, 0usize, 0usize);
    let mut kinds = [0usize; 4];
    let mut buf = Vec::new();
    let (mut t1, mut t2) = (0f64, 0f64);
    for row in 0..rows {
        for col in 0..cols {
            let r = tile_rect(col, row, frame.width, frame.height);
            frame.extract(r, &mut buf);
            let s = Instant::now();
            v1 += encode_tile(r, &buf, false, 70).unwrap().data.len();
            t1 += s.elapsed().as_secs_f64();
            let s = Instant::now();
            let d = encode_tile2(r.w, r.h, &buf, false, 70).unwrap();
            t2 += s.elapsed().as_secs_f64();
            jpeg += encode_tile(r, &buf, true, 70).unwrap().data.len();
            v2 += match &d {
                TileData::Solid(_) => {
                    kinds[0] += 1;
                    3
                }
                TileData::Palette(z) => {
                    kinds[1] += 1;
                    z.len()
                }
                TileData::Rgb(z) => {
                    kinds[2] += 1;
                    z.len()
                }
                other => {
                    kinds[3] += 1;
                    format!("{other:?}").len()
                }
            };
        }
    }
    println!(
        "{}x{}: first format {} KB in {:.0} ms, second {} KB in {:.0} ms ({:.0}%), JPEG 70 {} KB; \
         solid {} palette {} rgb {} other {}",
        frame.width,
        frame.height,
        v1 / 1024,
        t1 * 1000.0,
        v2 / 1024,
        t2 * 1000.0,
        v2 as f64 * 100.0 / v1 as f64,
        jpeg / 1024,
        kinds[0],
        kinds[1],
        kinds[2],
        kinds[3]
    );
}
