//! The computer's sound for the client ([`rdesk_proto::StreamOpen::Audio`]): what the speakers
//! play, captured on Windows with WASAPI loopback, encoded as Opus (20 ms packets at 48 kHz,
//! stereo), like every other remote desktop. A phone app brings its own ([`set_source`]: what
//! the phone plays, already Opus). Elsewhere the feature is not offered yet.

use std::sync::OnceLock;

use anyhow::Result;
use tokio::sync::mpsc;

/// Sound from the program the host runs in: whether it can send it now, and its packets.
#[derive(Clone, Copy)]
pub struct Source {
    pub available: fn() -> bool,
    pub start: fn() -> Result<mpsc::Receiver<Vec<u8>>>,
}

static SOURCE: OnceLock<Source> = OnceLock::new();

/// Take the sound from the program the host runs in (the phone app). Set once.
pub fn set_source(source: Source) {
    let _ = SOURCE.set(source);
}

/// Opus at 48 kHz, stereo.
pub const RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
/// Samples per channel in a 20 ms packet.
pub const FRAME: usize = (RATE / 1000 * rdesk_proto::AudioHost::FRAME_MS) as usize;
/// Packets waiting to be sent before the oldest are dropped (latency over completeness).
const QUEUE: usize = 10;

/// Whether this computer can send its sound.
pub fn supported() -> bool {
    match SOURCE.get() {
        Some(s) => (s.available)(),
        None => cfg!(windows),
    }
}

/// Encoded packets of what the computer plays, until the receiver is dropped.
pub fn start() -> Result<mpsc::Receiver<Vec<u8>>> {
    if let Some(s) = SOURCE.get() {
        return (s.start)();
    }
    let (tx, rx) = mpsc::channel(QUEUE);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    std::thread::Builder::new().name("rdesk-audio".into()).spawn(move || {
        if let Err(e) = capture(tx, &ready_tx) {
            let _ = ready_tx.send(Err(e));
        }
    })?;
    ready_rx.recv().map_err(|_| anyhow::anyhow!("audio capture thread ended"))??;
    Ok(rx)
}

/// Turn interleaved samples of `channels` channels into stereo.
pub fn to_stereo(data: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => {
            for &s in data {
                out.extend_from_slice(&[s, s]);
            }
        }
        2 => out.extend_from_slice(data),
        n => {
            for f in data.chunks_exact(n) {
                out.extend_from_slice(&[f[0], f[1]]);
            }
        }
    }
}

/// Linear resampling of stereo samples from `from` Hz to `to` Hz, keeping the position across
/// calls in `pos` (the fractional index into the next input).
pub struct Resampler {
    from: u32,
    to: u32,
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { from, to, pos: 0.0, last: [0.0; 2] }
    }

    pub fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
        let step = self.from as f64 / self.to as f64;
        // Frame -1 is the last one of the previous call.
        let at = |i: isize, c: usize| if i < 0 { self.last[c] } else { input[i as usize * 2 + c] };
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..2 {
                let a = at(i - 1, c);
                let b = at(i, c);
                out.push(a + (b - a) * t);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
        }
    }
}

/// Opus encoding of 48 kHz stereo samples into 20 ms packets.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub struct Encoder {
    enc: opus::Encoder,
    pending: Vec<f32>,
    out: Vec<u8>,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl Encoder {
    pub fn new() -> Result<Self> {
        let mut enc = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)?;
        enc.set_bitrate(opus::Bitrate::Bits(96_000))?;
        Ok(Self { enc, pending: Vec::with_capacity(FRAME * 4), out: vec![0; 4000] })
    }

    /// Add samples; returns the packets they complete.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(samples);
        let mut packets = Vec::new();
        let n = FRAME * CHANNELS as usize;
        while self.pending.len() >= n {
            let len = self.enc.encode_float(&self.pending[..n], &mut self.out)?;
            packets.push(self.out[..len].to_vec());
            self.pending.drain(..n);
        }
        Ok(packets)
    }
}

#[cfg(windows)]
fn capture(tx: mpsc::Sender<Vec<u8>>, ready: &std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_output_device().context("no speakers to listen to")?;
    let config = device.default_output_config().context("speaker format")?;
    let (rate, channels, format) = (config.sample_rate(), config.channels() as usize, config.sample_format());
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let err = |e| tracing::debug!("audio capture: {e}");
    // A speaker used as an input is captured in loopback mode by WASAPI.
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config.into(),
            move |d: &[f32], _: &_| {
                let _ = raw_tx.send(d.to_vec());
            },
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config.into(),
            move |d: &[i16], _: &_| {
                let _ = raw_tx.send(d.iter().map(|&s| s as f32 / 32768.0).collect());
            },
            err,
            None,
        ),
        f => anyhow::bail!("speaker sample format {f:?}"),
    }
    .context("listening to the speakers")?;
    stream.play().context("starting audio capture")?;
    let _ = ready.send(Ok(()));
    tracing::info!("sending sound ({rate} Hz, {channels} channels)");

    let mut enc = Encoder::new()?;
    let mut resampler = Resampler::new(rate, RATE);
    let (mut stereo, mut resampled) = (Vec::new(), Vec::new());
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        let data = match raw_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(d) => d,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return Ok(()),
        };
        stereo.clear();
        to_stereo(&data, channels, &mut stereo);
        resampled.clear();
        resampler.run(&stereo, &mut resampled);
        for p in enc.push(&resampled)? {
            // Full: the client is behind; this packet is dropped rather than delayed.
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(p) {
                return Ok(());
            }
        }
    }
}

#[cfg(not(windows))]
fn capture(_: mpsc::Sender<Vec<u8>>, _: &std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    anyhow::bail!("sound is only sent from Windows computers for now")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_become_stereo() {
        let mut out = Vec::new();
        to_stereo(&[0.5, -0.5], 1, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3, &mut out);
        assert_eq!(out, [1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn resampling_keeps_the_duration() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        // One second in chunks of 441 frames.
        for _ in 0..100 {
            r.run(&vec![0.25; 441 * 2], &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames} frames");
        assert!(out[100..].iter().all(|&s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn encodes_twenty_millisecond_packets() {
        let mut e = Encoder::new().unwrap();
        let tone: Vec<f32> = (0..FRAME * 3)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.3;
                [s, s]
            })
            .collect();
        let packets = e.push(&tone).unwrap();
        assert_eq!(packets.len(), 3);
        let mut d = opus::Decoder::new(RATE, opus::Channels::Stereo).unwrap();
        let mut pcm = vec![0f32; FRAME * 2];
        assert_eq!(d.decode_float(&packets[1], &mut pcm, false).unwrap(), FRAME);
    }
}
