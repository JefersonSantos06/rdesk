//! The host's sound: an `Audio` stream of Opus packets, decoded and played on this
//! computer's default output with a small buffer against network jitter.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rdesk_common::audio::{Resampler, from_stereo};
use rdesk_common::transport::Connection;
use rdesk_proto::{AUDIO_FRAME, AUDIO_RATE, AudioMsg, MAX_SMALL_MSG, StreamOpen, expect_msg, read_msg, write_msg};

/// Played sound waits this long in the buffer before it starts (and after it ran dry)…
const PREFILL_MS: usize = 60;
/// …and never more than this (a burst after a stall is cut back to the prefill).
const MAX_MS: usize = 300;

/// Sound playing; dropping it stops the sound and closes the stream.
pub struct AudioLink {
    task: tokio::task::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl Drop for AudioLink {
    fn drop(&mut self) {
        self.task.abort();
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Ask the host for its sound. `status` hears where it comes from, or why there is none.
pub fn start(conn: Connection, status: impl Fn(Result<String, String>) + Send + Sync + 'static) -> AudioLink {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(conn, &status, stop2).await {
            status(Err(format!("{e:#}")));
        }
    });
    AudioLink { task, stop }
}

async fn run(
    conn: Connection,
    status: &(dyn Fn(Result<String, String>) + Send + Sync),
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let (mut w, mut r) = conn.open_bi().await?;
    write_msg(&mut w, &StreamOpen::Audio).await?;
    let device = match expect_msg::<_, AudioMsg>(&mut r, MAX_SMALL_MSG).await {
        Ok(AudioMsg::Started { device }) => device,
        Ok(AudioMsg::Error(e)) => bail!("{e}"),
        Ok(AudioMsg::Packet(_)) => bail!("the host sent sound before saying so"),
        Err(_) => bail!("that computer's rdesk does not send sound (an older version)"),
    };
    let out = Output::start(stop)?;
    status(Ok(device));

    let mut dec = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo)?;
    // Up to 120 ms per packet, stereo.
    let mut pcm = vec![0f32; AUDIO_FRAME * 6 * 2];
    let mut resampler = Resampler::new(AUDIO_RATE, out.rate);
    let (mut resampled, mut mapped) = (Vec::new(), Vec::new());
    while let Some(m) = read_msg::<_, AudioMsg>(&mut r, MAX_SMALL_MSG).await? {
        let AudioMsg::Packet(p) = m else { continue };
        let n = dec.decode_float(&p, &mut pcm, false)?;
        resampled.clear();
        resampler.process(&pcm[..n * 2], &mut resampled);
        mapped.clear();
        from_stereo(&resampled, out.channels, &mut mapped);
        out.queue.lock().unwrap().push(&mapped);
    }
    // Keep our side open until here: closing it tells the host to stop.
    drop(w);
    Ok(())
}

/// Samples waiting to be played, interleaved in the output's format.
struct Queue {
    buf: VecDeque<f32>,
    playing: bool,
    prefill: usize,
    max: usize,
}

impl Queue {
    fn new(rate: u32, channels: usize) -> Self {
        let per_ms = rate as usize * channels / 1000;
        Self { buf: VecDeque::new(), playing: false, prefill: per_ms * PREFILL_MS, max: per_ms * MAX_MS }
    }

    fn push(&mut self, s: &[f32]) {
        self.buf.extend(s.iter().copied());
        if self.buf.len() > self.max {
            let cut = self.buf.len() - self.prefill;
            self.buf.drain(..cut);
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        if !self.playing && self.buf.len() >= self.prefill {
            self.playing = true;
        }
        if !self.playing {
            out.fill(0.0);
            return;
        }
        for s in out.iter_mut() {
            *s = self.buf.pop_front().unwrap_or(0.0);
        }
        if self.buf.is_empty() {
            // Ran dry: wait for a little sound again before going on.
            self.playing = false;
        }
    }
}

/// The default output, played from a [`Queue`] on its own thread.
struct Output {
    queue: Arc<Mutex<Queue>>,
    rate: u32,
    channels: usize,
}

impl Output {
    fn start(stop: Arc<AtomicBool>) -> Result<Output> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Output>>();
        std::thread::Builder::new().name("rdesk-sound".into()).spawn(move || {
            let (stream, out) = match open() {
                Ok(x) => x,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let _ = tx.send(Ok(out));
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            drop(stream);
        })?;
        rx.recv().map_err(|_| anyhow!("the sound output stopped"))?
    }
}

fn open() -> Result<(cpal::Stream, Output)> {
    let device = cpal::default_host().default_output_device().context("this computer has no sound output")?;
    let supported = device.default_output_config().context("reading the sound output's format")?;
    let config = supported.config();
    let queue = Arc::new(Mutex::new(Queue::new(config.sample_rate, config.channels as usize)));
    let out = Output { queue: queue.clone(), rate: config.sample_rate, channels: config.channels as usize };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, queue)?,
        SampleFormat::F64 => build::<f64>(&device, config, queue)?,
        SampleFormat::I16 => build::<i16>(&device, config, queue)?,
        SampleFormat::I32 => build::<i32>(&device, config, queue)?,
        SampleFormat::U16 => build::<u16>(&device, config, queue)?,
        SampleFormat::I8 => build::<i8>(&device, config, queue)?,
        SampleFormat::U8 => build::<u8>(&device, config, queue)?,
        other => bail!("the sound output uses samples rdesk does not write ({other})"),
    };
    stream.play().context("starting the sound output")?;
    Ok((stream, out))
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, queue: Arc<Mutex<Queue>>) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut buf: Vec<f32> = Vec::new();
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                buf.resize(data.len(), 0.0);
                queue.lock().unwrap().fill(&mut buf);
                for (d, &s) in data.iter_mut().zip(&buf) {
                    *d = <T as FromSample<f32>>::from_sample_(s);
                }
            },
            |e| tracing::warn!("sound output: {e}"),
            Some(Duration::from_secs(5)),
        )
        .context("opening the sound output")?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trip() {
        // 100 ms of a 440 Hz tone, encoded as the host does and decoded as here.
        let tone: Vec<f32> = (0..AUDIO_FRAME * 5)
            .flat_map(|i| [(i as f32 * 440.0 * std::f32::consts::TAU / AUDIO_RATE as f32).sin() * 0.5; 2])
            .collect();
        let mut enc = opus::Encoder::new(AUDIO_RATE, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(96_000)).unwrap();
        let mut dec = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo).unwrap();
        let (mut packet, mut pcm, mut out) = (vec![0u8; 4000], vec![0f32; AUDIO_FRAME * 2], Vec::new());
        for frame in tone.chunks(AUDIO_FRAME * 2) {
            let n = enc.encode_float(frame, &mut packet).unwrap();
            assert!(n < 400, "a 20 ms packet at 96 kbit/s is about 240 bytes, not {n}");
            let got = dec.decode_float(&packet[..n], &mut pcm, false).unwrap();
            assert_eq!(got, AUDIO_FRAME);
            out.extend_from_slice(&pcm);
        }
        // Past the codec's start-up delay, the tone's energy comes through.
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        let (a, b) = (rms(&tone[AUDIO_FRAME * 4..]), rms(&out[AUDIO_FRAME * 4..]));
        assert!((b / a - 1.0).abs() < 0.2, "{a} vs {b}");
    }

    #[test]
    fn the_queue_waits_for_a_little_sound_and_catches_up_after_a_burst() {
        // 1 kHz mono: 1 sample per ms.
        let mut q = Queue::new(1000, 1);
        let mut out = [1.0f32; 10];
        q.push(&[0.5; 30]);
        q.fill(&mut out);
        assert_eq!(out, [0.0; 10], "not enough yet: silence");
        q.push(&[0.5; 30]);
        q.fill(&mut out);
        assert_eq!(out, [0.5; 10]);
        // A burst beyond the maximum is cut back to the prefill.
        q.push(&[0.25; 400]);
        assert_eq!(q.buf.len(), PREFILL_MS);
        // Running dry pads with silence and waits again.
        let mut big = [1.0f32; 100];
        q.fill(&mut big);
        assert_eq!(big[..60], [0.25; 60]);
        assert_eq!(big[60..], [0.0; 40]);
        assert!(!q.playing);
    }
}
