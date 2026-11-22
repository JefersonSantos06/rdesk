//! Clipboard sync on a dedicated thread (used by both host and client).
//!
//! The thread polls the local clipboard every 500 ms and reports changes; what is received
//! from the peer is set locally and remembered so it is not echoed back. Text always; images
//! (as PNG) once both sides have agreed to send them ([`ClipboardSync::set_images`]), looked
//! at every second (reading a big image costs more than text).
//! Phones have none here (their app passes the text through the session).

#![cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use tokio::sync::mpsc::UnboundedSender;

pub const MAX_CLIPBOARD: usize = 10 * 1024 * 1024;
/// Largest image sent (pixels on a side; bigger ones are not sent).
pub const MAX_IMAGE_SIDE: u32 = 8192;

/// Something received from the peer, for the local clipboard.
enum Incoming {
    Text(String),
    Image(Vec<u8>),
}

pub struct ClipboardSync {
    set_tx: mpsc::Sender<Incoming>,
    images: Arc<AtomicBool>,
}

/// An RGBA image as PNG.
pub fn png_encode(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(rgba)?;
    }
    Ok(out)
}

/// A PNG as RGBA (width, height, pixels), refusing images over [`MAX_IMAGE_SIDE`].
pub fn png_decode(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = dec.read_info()?;
    let (w, h) = (reader.info().width, reader.info().height);
    ensure!(w <= MAX_IMAGE_SIDE && h <= MAX_IMAGE_SIDE, "image of {w}x{h} is too big");
    let mut buf = vec![0; reader.output_buffer_size().ok_or_else(|| anyhow!("image too big"))?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        c => return Err(anyhow!("unexpected PNG color type {c:?}")),
    };
    ensure!(rgba.len() == (w * h * 4) as usize, "PNG decoded to the wrong size");
    Ok((w, h, rgba))
}

impl ClipboardSync {
    /// Start watching. Local changes are sent on `on_change` (text) and `on_image` (PNG,
    /// once [`ClipboardSync::set_images`] turns it on). Returns `None` if the platform
    /// clipboard is unavailable.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn spawn(_on_change: UnboundedSender<String>, _on_image: Option<UnboundedSender<Vec<u8>>>) -> Option<Self> {
        // No clipboard to watch from here: a phone app passes text in and out itself.
        None
    }

    /// Start watching. Local changes are sent on `on_change` (text) and `on_image` (PNG,
    /// once [`ClipboardSync::set_images`] turns it on). Returns `None` if the platform
    /// clipboard is unavailable.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn spawn(on_change: UnboundedSender<String>, on_image: Option<UnboundedSender<Vec<u8>>>) -> Option<Self> {
        let (set_tx, set_rx) = mpsc::channel::<Incoming>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let images = Arc::new(AtomicBool::new(false));
        let images_on = images.clone();
        std::thread::Builder::new()
            .name("rdesk-clipboard".into())
            .spawn(move || {
                // Keep one instance alive: on X11 dropping it loses clipboard ownership.
                let mut clip = match arboard::Clipboard::new() {
                    Ok(c) => {
                        let _ = ready_tx.send(true);
                        c
                    }
                    Err(e) => {
                        tracing::warn!("clipboard unavailable: {e}");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let mut last = clip.get_text().ok().map(|t| blake3::hash(t.as_bytes()));
                // The image on the clipboard when images were turned on is not sent.
                let mut last_image: Option<blake3::Hash> = None;
                let mut images_were_on = false;
                let mut tick = 0u64;
                loop {
                    match set_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(Incoming::Text(text)) => {
                            last = Some(blake3::hash(text.as_bytes()));
                            if let Err(e) = clip.set_text(text) {
                                tracing::debug!("setting clipboard failed: {e}");
                            }
                        }
                        Ok(Incoming::Image(png)) => match png_decode(&png) {
                            Ok((w, h, rgba)) => {
                                last_image = Some(blake3::hash(&rgba));
                                let img =
                                    arboard::ImageData { width: w as usize, height: h as usize, bytes: rgba.into() };
                                if let Err(e) = clip.set_image(img) {
                                    tracing::debug!("setting clipboard image failed: {e}");
                                }
                                // The text on the clipboard is gone now; whatever comes next is new.
                                last = None;
                            }
                            Err(e) => tracing::debug!("clipboard image: {e:#}"),
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            tick += 1;
                            if let Ok(text) = clip.get_text() {
                                let h = blake3::hash(text.as_bytes());
                                if Some(h) != last {
                                    last = Some(h);
                                    if !text.is_empty() && text.len() <= MAX_CLIPBOARD && on_change.send(text).is_err()
                                    {
                                        return;
                                    }
                                }
                            } else if let Some(tx) = &on_image
                                && images_on.load(Ordering::Acquire)
                                && tick.is_multiple_of(2)
                                && let Ok(img) = clip.get_image()
                            {
                                let h = blake3::hash(&img.bytes);
                                let first = !std::mem::replace(&mut images_were_on, true);
                                if Some(h) != last_image {
                                    last_image = Some(h);
                                    let (w, hgt) = (img.width as u32, img.height as u32);
                                    if !first && w <= MAX_IMAGE_SIDE && hgt <= MAX_IMAGE_SIDE {
                                        match png_encode(w, hgt, &img.bytes) {
                                            Ok(png) if png.len() <= MAX_CLIPBOARD => {
                                                if tx.send(png).is_err() {
                                                    return;
                                                }
                                            }
                                            Ok(_) => tracing::debug!("clipboard image too big to send"),
                                            Err(e) => tracing::debug!("clipboard image: {e:#}"),
                                        }
                                    }
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok()?;
        ready_rx.recv().ok()?.then_some(Self { set_tx, images })
    }

    pub fn set(&self, text: String) {
        if text.len() <= MAX_CLIPBOARD {
            let _ = self.set_tx.send(Incoming::Text(text));
        }
    }

    /// Put a PNG image on the local clipboard.
    pub fn set_image(&self, png: Vec<u8>) {
        if png.len() <= MAX_CLIPBOARD {
            let _ = self.set_tx.send(Incoming::Image(png));
        }
    }

    /// Watch images too (both sides agreed).
    pub fn set_images(&self, on: bool) {
        self.images.store(on, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let rgba: Vec<u8> = (0..7 * 3).flat_map(|i| [i as u8, 255 - i as u8, 7, 200]).collect();
        let png = png_encode(7, 3, &rgba).unwrap();
        assert_eq!(png_decode(&png).unwrap(), (7, 3, rgba));
        assert!(png_decode(b"not a png").is_err());
    }
}
