//! H.264 for whole-screen motion: OpenH264 on computers, the phone's own hardware encoder
//! (MediaCodec) on Android. iPhones have none (they are never hosts), so there the picture
//! always goes as tiles.
//!
//! Every encoder gives access units in Annex B, each key frame with its SPS and PPS, in the
//! constrained baseline profile: what the clients' OpenH264 decoder reads.

#[cfg(desktop)]
pub use rdesk_common::h264::{H264Encoder, bitrate, usable};

#[cfg(target_os = "android")]
pub use mediacodec::{H264Encoder, bitrate, usable};

#[cfg(any(target_os = "ios", headless))]
pub use none::{H264Encoder, bitrate, usable};

/// RGBA → NV12 (Y plane, then interleaved U and V at half size), limited-range BT.601: the same
/// conversion as OpenH264's encoder, so the clients' decoder turns it back the same way.
/// `width` and `height` are even.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn rgba_to_nv12(rgba: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    out.resize(width * height * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(width * height);
    for (y_row, px_row) in y_plane.chunks_exact_mut(width).zip(rgba.chunks_exact(width * 4)) {
        for (y, px) in y_row.iter_mut().zip(px_row.as_chunks::<4>().0) {
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *y = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }
    for (j, uv_row) in uv_plane.chunks_exact_mut(width).enumerate() {
        let top = &rgba[2 * j * width * 4..][..width * 4];
        let bottom = &rgba[(2 * j + 1) * width * 4..][..width * 4];
        for (i, uv) in uv_row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for px in [
                &top[i * 8..i * 8 + 4],
                &top[i * 8 + 4..i * 8 + 8],
                &bottom[i * 8..i * 8 + 4],
                &bottom[i * 8 + 4..i * 8 + 8],
            ] {
                r += px[0] as i32;
                g += px[1] as i32;
                b += px[2] as i32;
            }
            let (r, g, b) = ((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
            uv[0] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            uv[1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

#[cfg(target_os = "android")]
mod mediacodec {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow, bail, ensure};
    use ndk::media::media_codec::{
        DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    };
    use ndk::media::media_format::MediaFormat;
    use rdesk_common::tiles::Frame;

    const MIME: &str = "video/avc";
    /// `MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar` (NV12).
    const COLOR_NV12: i32 = 21;
    /// `MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline`.
    const PROFILE_BASELINE: i32 = 1;
    /// `MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR`.
    const BITRATE_VBR: i32 = 1;
    const FLAG_KEY_FRAME: u32 = 1;
    const FLAG_CODEC_CONFIG: u32 = 2;
    /// How long a frame may take to come out of the encoder.
    const OUTPUT_WAIT: Duration = Duration::from_millis(500);

    // The same as `rdesk_common::h264` (which phones do not build: it is OpenH264): keep them alike.

    /// Bits per second for a picture of `w`×`h` at `fps`, scaled by JPEG-like `quality` (10..100).
    pub fn bitrate(w: u32, h: u32, fps: u8, quality: u8) -> u32 {
        let base = w as f64 * h as f64 * fps.clamp(5, 60) as f64 * 0.1;
        (base * quality.clamp(10, 100) as f64 / 70.0).clamp(500_000.0, 40_000_000.0) as u32
    }

    /// H.264 needs even sizes (4:2:0).
    pub fn usable(width: u32, height: u32) -> bool {
        width >= 64 && height >= 64 && width.is_multiple_of(2) && height.is_multiple_of(2)
    }

    /// The phone's hardware H.264 encoder, used synchronously: each [`H264Encoder::encode`] puts
    /// one picture in and waits for its access unit.
    pub struct H264Encoder {
        codec: MediaCodec,
        width: u32,
        height: u32,
        nv12: Vec<u8>,
        /// SPS and PPS (the encoder's codec-config output), put before every key frame.
        config: Vec<u8>,
        start: Instant,
        last_pts: u64,
    }

    // SAFETY: an NDK MediaCodec in synchronous mode has no thread affinity; it is only ever used
    // by one thread at a time (behind `&mut`).
    unsafe impl Send for H264Encoder {}

    impl H264Encoder {
        pub fn new(width: u32, height: u32, fps: u8, bps: u32) -> Result<Self> {
            ensure!(usable(width, height), "H.264 needs an even size, not {width}x{height}");
            let codec =
                MediaCodec::from_encoder_type(MIME).ok_or_else(|| anyhow!("this phone has no H.264 encoder"))?;
            let mut format = MediaFormat::new();
            format.set_str("mime", MIME);
            format.set_i32("width", width as i32);
            format.set_i32("height", height as i32);
            format.set_i32("color-format", COLOR_NV12);
            format.set_i32("bitrate", bps as i32);
            format.set_i32("bitrate-mode", BITRATE_VBR);
            format.set_i32("frame-rate", fps.clamp(5, 60) as i32);
            // Seconds between key frames: the host starts a new encoder for each burst of
            // motion, so key frames come at its start anyway.
            format.set_i32("i-frame-interval", 10);
            format.set_i32("profile", PROFILE_BASELINE);
            // Real time, and low latency where the encoder knows it (Android 11+).
            format.set_i32("priority", 0);
            format.set_i32("latency", 1);
            codec
                .configure(&format, None, MediaCodecDirection::Encoder)
                .with_context(|| format!("configuring the H.264 encoder for {width}x{height}"))?;
            codec.start().context("starting the H.264 encoder")?;
            Ok(Self { codec, width, height, nv12: Vec::new(), config: Vec::new(), start: Instant::now(), last_pts: 0 })
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn force_key(&mut self) {
            let mut p = MediaFormat::new();
            // `MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME`.
            p.set_i32("request-sync", 0);
            if let Err(e) = self.codec.set_parameters(p) {
                tracing::debug!("H.264: no key frame on request: {e:?}");
            }
        }

        /// One frame (RGBA) as an Annex B access unit.
        pub fn encode(&mut self, f: &Frame) -> Result<Vec<u8>> {
            ensure!((f.width, f.height) == (self.width, self.height), "frame of another size");
            super::rgba_to_nv12(&f.data, f.width as usize, f.height as usize, &mut self.nv12);
            // Timestamps must grow; equal ones confuse some encoders.
            let pts = (self.start.elapsed().as_micros() as u64).max(self.last_pts + 1);
            self.last_pts = pts;
            match self.codec.dequeue_input_buffer(OUTPUT_WAIT)? {
                DequeuedInputBufferResult::Buffer(mut buf) => {
                    let dst = buf.buffer_mut();
                    let n = self.nv12.len();
                    ensure!(dst.len() >= n, "encoder buffer of {} bytes for {n}", dst.len());
                    // SAFETY: `dst` holds at least `n` bytes (checked) and does not overlap ours.
                    unsafe { std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n) };
                    self.codec.queue_input_buffer(buf, 0, n, pts, 0)?;
                }
                DequeuedInputBufferResult::TryAgainLater => bail!("the H.264 encoder is not taking pictures"),
            }
            // Wait for this picture's access unit (earlier ones that came late are dropped).
            let deadline = Instant::now() + OUTPUT_WAIT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    bail!("the H.264 encoder gave nothing for a picture");
                }
                match self.codec.dequeue_output_buffer(left)? {
                    DequeuedOutputBufferInfoResult::Buffer(out) => {
                        let info = *out.info();
                        let data = &out.buffer()[info.offset() as usize..][..info.size() as usize];
                        let flags = info.flags();
                        let mine = info.presentation_time_us() as u64 == pts;
                        let au = if flags & FLAG_CODEC_CONFIG != 0 {
                            self.config = data.to_vec();
                            None
                        } else if mine && flags & FLAG_KEY_FRAME != 0 && !data.starts_with(&self.config) {
                            Some([self.config.as_slice(), data].concat())
                        } else if mine {
                            Some(data.to_vec())
                        } else {
                            None
                        };
                        self.codec.release_output_buffer(out, false)?;
                        if let Some(au) = au {
                            return Ok(au);
                        }
                    }
                    DequeuedOutputBufferInfoResult::TryAgainLater => {}
                    DequeuedOutputBufferInfoResult::OutputFormatChanged
                    | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
                }
            }
        }
    }

    impl Drop for H264Encoder {
        fn drop(&mut self) {
            let _ = self.codec.stop();
        }
    }
}

#[cfg(any(target_os = "ios", headless))]
mod none {
    use anyhow::{Result, bail};
    use rdesk_common::tiles::Frame;

    pub struct H264Encoder(std::convert::Infallible);

    pub fn bitrate(_: u32, _: u32, _: u8, _: u8) -> u32 {
        0
    }

    /// Never: the video pipeline keeps to tiles.
    pub fn usable(_: u32, _: u32) -> bool {
        false
    }

    impl H264Encoder {
        pub fn new(_: u32, _: u32, _: u8, _: u32) -> Result<Self> {
            bail!("no H.264 encoder here (an iPhone, a computer without a screen)")
        }

        pub fn size(&self) -> (u32, u32) {
            match self.0 {}
        }

        pub fn force_key(&mut self) {
            match self.0 {}
        }

        pub fn encode(&mut self, _: &Frame) -> Result<Vec<u8>> {
            match self.0 {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_matches_the_bt601_limited_range() {
        // 2x2: white, black, red, blue; one chroma sample from their average.
        let rgba = [255, 255, 255, 255, 0, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255];
        let mut out = Vec::new();
        rgba_to_nv12(&rgba, 2, 2, &mut out);
        assert_eq!(out.len(), 6);
        assert_eq!(&out[..4], &[235, 16, 82, 41]);
        // Average (128, 64, 128): a purple-ish chroma, close to OpenH264's.
        let (u, v) = (out[4] as i32, out[5] as i32);
        assert!((u - 147).abs() <= 1 && (v - 152).abs() <= 1, "u {u} v {v}");
    }
}
