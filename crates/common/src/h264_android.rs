//! H.264 decoding on Android phones (the app as a client): the phone's own decoder
//! (MediaCodec) draws into an image reader, and the picture is turned into RGBA here. The same
//! interface as [`crate::h264::H264Decoder`] (OpenH264 on computers), which phones do not build
//! (it is C++).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::tiles::Frame;

const MIME: &str = "video/avc";
/// How long one access unit may take to come out as a picture.
const WAIT: Duration = Duration::from_millis(300);

/// Whether this phone has an H.264 decoder (what the client tells the host). Asked once: it
/// opens one to find out.
pub fn available() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| MediaCodec::from_decoder_type(MIME).is_some())
}

struct Codec {
    codec: MediaCodec,
    reader: ImageReader,
    size: (u32, u32),
}

/// Decodes what the host's encoder made, in order.
#[derive(Default)]
pub struct H264Decoder {
    inner: Option<Codec>,
    pts: u64,
}

// SAFETY: the NDK codec and image reader have no thread affinity in synchronous use; they are
// only ever used by one thread at a time (behind `&mut`).
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        if !available() {
            bail!("this phone has no H.264 decoder");
        }
        Ok(Self::default())
    }

    fn open(width: u32, height: u32) -> Result<Codec> {
        let codec = MediaCodec::from_decoder_type(MIME).context("no H.264 decoder")?;
        let reader = ImageReader::new(width as i32, height as i32, ImageFormat::YUV_420_888, 3)
            .map_err(|e| anyhow!("image reader: {e:?}"))?;
        let window = reader.window().map_err(|e| anyhow!("image reader window: {e:?}"))?;
        let mut format = MediaFormat::new();
        format.set_str("mime", MIME);
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
        // Each picture as soon as it is decoded (Android 11+); real time.
        format.set_i32("low-latency", 1);
        format.set_i32("priority", 0);
        codec
            .configure(&format, Some(&window), MediaCodecDirection::Decoder)
            .with_context(|| format!("configuring the H.264 decoder for {width}x{height}"))?;
        codec.start().context("starting the H.264 decoder")?;
        Ok(Codec { codec, reader, size: (width, height) })
    }

    /// Decode one access unit into `frame` (RGBA, same size). A picture the decoder keeps back
    /// for longer leaves `frame` as it was (the next one catches up).
    pub fn decode_into(&mut self, au: &[u8], frame: &mut Frame) -> Result<()> {
        if self.inner.as_ref().is_none_or(|c| c.size != (frame.width, frame.height)) {
            self.inner = None;
            self.inner = Some(Self::open(frame.width, frame.height)?);
        }
        let c = self.inner.as_ref().expect("just opened");
        self.pts += 33_333;
        match c.codec.dequeue_input_buffer(WAIT)? {
            DequeuedInputBufferResult::Buffer(mut buf) => {
                let dst = buf.buffer_mut();
                if dst.len() < au.len() {
                    bail!("decoder buffer of {} bytes for {}", dst.len(), au.len());
                }
                // SAFETY: `dst` holds at least `au.len()` bytes (checked) and does not overlap it.
                unsafe { std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), au.len()) };
                c.codec.queue_input_buffer(buf, 0, au.len(), self.pts, 0)?;
            }
            DequeuedInputBufferResult::TryAgainLater => bail!("the H.264 decoder is not taking frames"),
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            match c.codec.dequeue_output_buffer(left)? {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    // To the image reader.
                    c.codec.release_output_buffer(out, true)?;
                    let image = wait_image(&c.reader, deadline)?;
                    if let Some(img) = image {
                        yuv_to_rgba(&img, frame)?;
                    }
                    return Ok(());
                }
                DequeuedOutputBufferInfoResult::TryAgainLater => {}
                DequeuedOutputBufferInfoResult::OutputFormatChanged
                | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
            }
        }
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        let _ = self.codec.stop();
    }
}

/// The newest decoded picture, waiting until `deadline` for one.
fn wait_image(reader: &ImageReader, deadline: Instant) -> Result<Option<Image>> {
    loop {
        match reader.acquire_latest_image().map_err(|e| anyhow!("image reader: {e:?}"))? {
            AcquireResult::Image(img) => return Ok(Some(img)),
            AcquireResult::NoBufferAvailable | AcquireResult::MaxImagesAcquired => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// YUV 4:2:0 (any plane layout) → RGBA, limited-range BT.601: the inverse of the encoders'.
fn yuv_to_rgba(img: &Image, frame: &mut Frame) -> Result<()> {
    let e = |e| anyhow!("decoded picture: {e:?}");
    let crop = img.crop_rect().map_err(e)?;
    let (y, u, v) = (img.plane_data(0).map_err(e)?, img.plane_data(1).map_err(e)?, img.plane_data(2).map_err(e)?);
    let y_row = img.plane_row_stride(0).map_err(e)? as usize;
    let (uv_row, uv_px) =
        (img.plane_row_stride(1).map_err(e)? as usize, img.plane_pixel_stride(1).map_err(e)? as usize);
    let (x0, y0) = (crop.left.max(0) as usize, crop.top.max(0) as usize);
    let (w, h) = (frame.width as usize, frame.height as usize);
    for row in 0..h {
        let yr = (y0 + row) * y_row + x0;
        let cr = (y0 + row) / 2 * uv_row;
        let out = &mut frame.data[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let Some(&yy) = y.get(yr + col) else { bail!("decoded picture smaller than {w}x{h}") };
            let ci = cr + (x0 + col) / 2 * uv_px;
            let (Some(&uu), Some(&vv)) = (u.get(ci), v.get(ci)) else { bail!("decoded picture smaller than {w}x{h}") };
            let c = 298 * (yy as i32 - 16);
            let (d, e) = (uu as i32 - 128, vv as i32 - 128);
            let px = &mut out[col * 4..col * 4 + 4];
            px[0] = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
    Ok(())
}
