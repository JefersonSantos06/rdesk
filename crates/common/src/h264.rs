//! H.264 for whole-screen motion (a video playing full screen, a window dragged around): the
//! host switches from tiles to H.264 while most of the screen keeps changing, and back to
//! tiles, refined losslessly, once it settles. Like the hybrid codecs of RDP (AVC444) and
//! the video modes of other remote desktops. Software (Cisco's OpenH264, built from source).

use anyhow::{Result, anyhow, ensure};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};

use crate::tiles::Frame;

/// Encodes the picture, frame after frame (the first one, and any after
/// [`H264Encoder::force_key`], is a key frame).
pub struct H264Encoder {
    enc: Encoder,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
}

// SAFETY: the OpenH264 encoder has no thread affinity; it is only ever used by one thread at a
// time (behind `&mut`).
unsafe impl Send for H264Encoder {}

/// Bits per second for a picture of `w`×`h` at `fps`, scaled by JPEG-like `quality` (10..100).
pub fn bitrate(w: u32, h: u32, fps: u8, quality: u8) -> u32 {
    let base = w as f64 * h as f64 * fps.clamp(5, 60) as f64 * 0.1;
    (base * quality.clamp(10, 100) as f64 / 70.0).clamp(500_000.0, 40_000_000.0) as u32
}

/// H.264 needs even sizes (4:2:0).
pub fn usable(width: u32, height: u32) -> bool {
    width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2)
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u8, bps: u32) -> Result<Self> {
        ensure!(usable(width, height), "H.264 needs an even size, not {width}x{height}");
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps.clamp(5, 60) as f32))
            .skip_frames(false);
        let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), cfg)?;
        Ok(Self { enc, yuv: YUVBuffer::new(width as usize, height as usize), width, height })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn force_key(&mut self) {
        self.enc.force_intra_frame();
    }

    /// One frame (RGBA) as an Annex B access unit.
    pub fn encode(&mut self, f: &Frame) -> Result<Vec<u8>> {
        ensure!((f.width, f.height) == (self.width, self.height), "frame of another size");
        self.yuv.read_rgba8(RgbaSliceU8::new(&f.data, (f.width as usize, f.height as usize)));
        let bits = self.enc.encode(&self.yuv)?;
        Ok(bits.to_vec())
    }
}

/// Decodes what [`H264Encoder`] made, in order.
pub struct H264Decoder {
    dec: Decoder,
}

// SAFETY: as for the encoder: no thread affinity, used behind `&mut` only.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self { dec: Decoder::new()? })
    }

    /// Decode one access unit into `frame` (RGBA, same size).
    pub fn decode_into(&mut self, au: &[u8], frame: &mut Frame) -> Result<()> {
        let pic = self.dec.decode(au)?.ok_or_else(|| anyhow!("H.264: no picture in this frame"))?;
        let (w, h) = pic.dimensions();
        ensure!(
            (w as u32, h as u32) == (frame.width, frame.height),
            "H.264 picture of {w}x{h}, expected {}x{}",
            frame.width,
            frame.height
        );
        pic.write_rgba8(&mut frame.data);
        // The decoder writes opaque pixels, but be sure.
        for px in frame.data.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moving(w: u32, h: u32, t: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                f.data[i..i + 3].copy_from_slice(&[((x + t * 4) % 256) as u8, ((y + t * 2) % 256) as u8, 128]);
            }
        }
        f
    }

    #[test]
    fn encodes_and_decodes_a_moving_picture() {
        let (w, h) = (320, 192);
        let mut e = H264Encoder::new(w, h, 30, bitrate(w, h, 30, 80)).unwrap();
        let mut d = H264Decoder::new().unwrap();
        let mut out = Frame::new(w, h);
        let mut total = 0;
        for t in 0..10 {
            let src = moving(w, h, t);
            let au = e.encode(&src).unwrap();
            total += au.len();
            d.decode_into(&au, &mut out).unwrap();
            // Close to the source (lossy).
            let err: u64 =
                src.data.iter().zip(&out.data).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
            let mean = err as f64 / src.data.len() as f64;
            assert!(mean < 12.0, "frame {t}: mean error {mean}");
        }
        assert!(total < (w * h * 4 * 10) as usize / 10, "{total} bytes for 10 frames");
        assert!(!usable(321, 192));
    }
}
