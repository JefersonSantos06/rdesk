//! Playing the computer's sound: Opus packets decoded on the network thread into a small
//! jitter buffer that the speakers' callback drains. A buffer that grows past
//! [`MAX_BUFFER_MS`] (the speakers fell behind, or a burst after a stall) is cut back, so the
//! sound never drifts behind the picture.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rdesk_core::conn::{AudioEvent, AudioSink};

/// Sound buffered before playing starts (and again after running dry).
const START_MS: u32 = 60;
/// More than this is cut back to [`START_MS`].
const MAX_BUFFER_MS: u32 = 250;

/// Samples waiting for the speakers (interleaved at the speakers' rate and channel count).
struct Jitter {
    samples: VecDeque<f32>,
    /// Waiting for [`START_MS`] before playing.
    priming: bool,
}

struct Playing {
    decoder: opus::Decoder,
    in_rate: u32,
    in_channels: usize,
    out_rate: u32,
    out_channels: usize,
    resampler: Resampler,
    buf: Arc<Mutex<Jitter>>,
    pcm: Vec<f32>,
    /// Dropping it ends the thread that holds the speakers' stream.
    _stop: std::sync::mpsc::Sender<()>,
}

/// The sound player of a session window.
#[derive(Default)]
pub struct Player {
    playing: Mutex<Option<Playing>>,
}

impl Player {
    pub fn sink(self: &Arc<Self>) -> AudioSink {
        let me = self.clone();
        Arc::new(move |e| me.event(e))
    }

    fn event(&self, e: AudioEvent) {
        match e {
            AudioEvent::Start { sample_rate, channels } => match Playing::start(sample_rate, channels as usize) {
                Ok(p) => *self.playing.lock().unwrap() = Some(p),
                Err(e) => tracing::warn!("cannot play the computer's sound: {e:#}"),
            },
            AudioEvent::Packet(p) => {
                if let Some(pl) = self.playing.lock().unwrap().as_mut()
                    && let Err(e) = pl.packet(&p)
                {
                    tracing::debug!("sound packet: {e:#}");
                }
            }
            AudioEvent::Stop => *self.playing.lock().unwrap() = None,
        }
    }
}

impl Playing {
    fn start(in_rate: u32, in_channels: usize) -> Result<Self> {
        let channels = match in_channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            n => return Err(anyhow!("{n} channels")),
        };
        let decoder = opus::Decoder::new(in_rate, channels)?;
        let buf = Arc::new(Mutex::new(Jitter { samples: VecDeque::new(), priming: true }));
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(u32, usize)>>();
        let b = buf.clone();
        // The speakers' stream lives on its own thread (it may not move between threads).
        std::thread::Builder::new().name("rdesk-sound".into()).spawn(move || {
            match open_speakers(b) {
                Ok((stream, rate, ch)) => {
                    let _ = ready_tx.send(Ok((rate, ch)));
                    // Until the sender is dropped.
                    let _ = stopped.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        let (out_rate, out_channels) = ready_rx.recv().map_err(|_| anyhow!("sound thread ended"))??;
        Ok(Self {
            decoder,
            in_rate,
            in_channels,
            out_rate,
            out_channels,
            resampler: Resampler::new(in_rate, out_rate, out_channels),
            buf,
            pcm: vec![0.0; 5760 * 2],
            _stop: stop,
        })
    }

    fn packet(&mut self, p: &[u8]) -> Result<()> {
        let n = self.decoder.decode_float(p, &mut self.pcm, false)?;
        // To the speakers' channels: stereo (or mono) in, as many as they have out.
        let mut frames = Vec::with_capacity(n * self.out_channels);
        for i in 0..n {
            let (l, r) =
                if self.in_channels == 2 { (self.pcm[i * 2], self.pcm[i * 2 + 1]) } else { (self.pcm[i], self.pcm[i]) };
            match self.out_channels {
                1 => frames.push((l + r) / 2.0),
                c => {
                    frames.extend_from_slice(&[l, r]);
                    frames.extend(std::iter::repeat_n(0.0, c - 2));
                }
            }
        }
        let mut out = Vec::with_capacity(frames.len() * self.out_rate as usize / self.in_rate as usize + 16);
        self.resampler.run(&frames, &mut out);
        let per_ms = (self.out_rate as usize * self.out_channels) / 1000;
        let mut j = self.buf.lock().unwrap();
        j.samples.extend(out);
        if j.samples.len() > MAX_BUFFER_MS as usize * per_ms {
            let keep = START_MS as usize * per_ms;
            let drop = j.samples.len() - keep;
            j.samples.drain(..drop - drop % self.out_channels);
        }
        Ok(())
    }
}

/// Open the default speakers; their callback plays from `buf`.
fn open_speakers(buf: Arc<Mutex<Jitter>>) -> Result<(cpal::Stream, u32, usize)> {
    let device = cpal::default_host().default_output_device().context("no speakers")?;
    let config = device.default_output_config().context("speaker format")?;
    let (rate, channels, format) = (config.sample_rate(), config.channels() as usize, config.sample_format());
    let start = START_MS as usize * rate as usize * channels / 1000;
    let fill = move |out: &mut dyn FnMut(usize, f32), len: usize| {
        let mut j = buf.lock().unwrap();
        if j.priming && j.samples.len() >= start {
            j.priming = false;
        }
        for i in 0..len {
            let s = if j.priming { None } else { j.samples.pop_front() };
            if s.is_none() && !j.priming {
                // Ran dry: wait for a little to build up again.
                j.priming = true;
            }
            out(i, s.unwrap_or(0.0));
        }
    };
    let err = |e| tracing::debug!("speakers: {e}");
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config.into(),
            move |d: &mut [f32], _: &_| {
                let len = d.len();
                fill(&mut |i, s| d[i] = s, len)
            },
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config.into(),
            move |d: &mut [i16], _: &_| {
                let len = d.len();
                fill(&mut |i, s| d[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16, len)
            },
            err,
            None,
        ),
        f => anyhow::bail!("speaker sample format {f:?}"),
    }
    .context("opening the speakers")?;
    stream.play().context("starting the speakers")?;
    Ok((stream, rate, channels))
}

/// Linear resampling of interleaved samples, keeping its place across calls.
struct Resampler {
    step: f64,
    channels: usize,
    pos: f64,
    last: Vec<f32>,
}

impl Resampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        Self { step: from as f64 / to as f64, channels, pos: 0.0, last: vec![0.0; channels] }
    }

    fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let c = self.channels;
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / c;
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for ch in 0..c {
                let a = if i < 1 { self.last[ch] } else { input[(i as usize - 1) * c + ch] };
                let b = if i < 0 { self.last[ch] } else { input[i as usize * c + ch] };
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last.copy_from_slice(&input[(frames - 1) * c..frames * c]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_to_44_1_khz_keeps_the_duration() {
        let mut r = Resampler::new(48_000, 44_100, 2);
        let mut out = Vec::new();
        for _ in 0..50 {
            r.run(&vec![0.5; 960 * 2], &mut out);
        }
        let frames = out.len() / 2;
        assert!((44_090..=44_110).contains(&frames), "{frames}");
    }
}
