//! The computer's sound for a session: what it plays (not its microphone). Captured by
//! loopback (WASAPI on Windows; a CoreAudio tap on a Mac, macOS 14.6+, which asks for
//! permission the first time; the PulseAudio/PipeWire monitor of the default output on
//! Linux), turned into 48 kHz stereo and sent as Opus packets ([`AudioMsg`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rdesk_common::audio::{Resampler, to_stereo};
use rdesk_proto::{AUDIO_FRAME, AUDIO_RATE, AudioMsg, MAX_SMALL_MSG, read_msg, write_msg};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

/// Opus at this rate is close to transparent for music and cheap for speech.
const BITRATE: i32 = 96_000;

/// Stream the sound on `w` until the client closes `r` (or the capture fails).
pub async fn serve<R, W>(r: &mut R, w: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (tx, mut rx) = mpsc::channel::<Vec<f32>>(64);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = StopOnDrop(stop.clone());
    std::thread::Builder::new().name("rdesk-audio".into()).spawn(move || capture_thread(tx, ready_tx, stop))?;
    // A Mac asks its user for permission the first time; nobody may be there to answer.
    let device = match tokio::time::timeout(Duration::from_secs(15), ready_rx).await {
        Ok(Ok(Ok(d))) => d,
        Ok(Ok(Err(e))) => {
            write_msg(w, &AudioMsg::Error(format!("{e:#}"))).await?;
            return Ok(());
        }
        Ok(Err(_)) => {
            write_msg(w, &AudioMsg::Error("the sound capture stopped".into())).await?;
            return Ok(());
        }
        Err(_) => {
            let why = "the sound output did not answer (on a Mac: allow rdesk to record the screen and audio)";
            write_msg(w, &AudioMsg::Error(why.into())).await?;
            return Ok(());
        }
    };
    write_msg(w, &AudioMsg::Started { device }).await?;

    let mut enc = opus::Encoder::new(AUDIO_RATE, opus::Channels::Stereo, opus::Application::Audio)?;
    enc.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
    let mut pending: Vec<f32> = Vec::new();
    let mut packet = vec![0u8; 4000];
    let frame = AUDIO_FRAME * 2;
    // The client sends nothing more: its side ending means it no longer listens.
    let closed = async { while let Ok(Some(_)) = read_msg::<_, AudioMsg>(r, MAX_SMALL_MSG).await {} };
    tokio::pin!(closed);
    loop {
        tokio::select! {
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { break };
                pending.extend_from_slice(&chunk);
                while pending.len() >= frame {
                    let n = enc.encode_float(&pending[..frame], &mut packet)?;
                    pending.drain(..frame);
                    if write_msg(w, &AudioMsg::Packet(packet[..n].to_vec())).await.is_err() {
                        return Ok(());
                    }
                }
            }
            _ = &mut closed => break,
        }
    }
    Ok(())
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The capture lives on its own thread (a sound stream may not move between threads).
fn capture_thread(tx: mpsc::Sender<Vec<f32>>, ready: oneshot::Sender<Result<String>>, stop: Arc<AtomicBool>) {
    let stream = match open(tx.clone()) {
        Ok((stream, name)) => {
            let _ = ready.send(Ok(name));
            stream
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(stream);
}

fn open(tx: mpsc::Sender<Vec<f32>>) -> Result<(cpal::Stream, String)> {
    let (device, supported, name) = loopback_device()?;
    let config = supported.config();
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(config.sample_rate, AUDIO_RATE);
    let mut stereo = Vec::new();
    let push = move |samples: &[f32]| {
        stereo.clear();
        to_stereo(samples, channels, &mut stereo);
        let mut out = Vec::with_capacity(stereo.len() * 2);
        resampler.process(&stereo, &mut out);
        // Behind (a slow network): drop sound rather than fall further behind.
        let _ = tx.try_send(out);
    };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, config, push)?,
        SampleFormat::F64 => build::<f64>(&device, config, push)?,
        SampleFormat::I16 => build::<i16>(&device, config, push)?,
        SampleFormat::I32 => build::<i32>(&device, config, push)?,
        SampleFormat::U16 => build::<u16>(&device, config, push)?,
        SampleFormat::I8 => build::<i8>(&device, config, push)?,
        SampleFormat::U8 => build::<u8>(&device, config, push)?,
        other => bail!("the sound output uses samples rdesk does not read ({other})"),
    };
    stream.play().context("starting the sound capture")?;
    Ok((stream, name))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut push: impl FnMut(&[f32]) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut buf: Vec<f32> = Vec::new();
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                buf.clear();
                buf.extend(data.iter().map(|&s| <f32 as FromSample<T>>::from_sample_(s)));
                push(&buf);
            },
            |e| tracing::warn!("sound capture: {e}"),
            Some(Duration::from_secs(5)),
        )
        .context("capturing the sound output")?;
    Ok(stream)
}

/// What to capture: the default output itself (loopback), or on Linux its monitor.
#[cfg(not(target_os = "linux"))]
fn loopback_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig, String)> {
    let device = cpal::default_host().default_output_device().context("this computer has no sound output")?;
    let config = device.default_output_config().context("reading the sound output's format")?;
    Ok((device.clone(), config, name(&device)))
}

#[cfg(target_os = "linux")]
fn loopback_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig, String)> {
    let host = cpal::host_from_id(cpal::HostId::PulseAudio).context("no PulseAudio or PipeWire sound server")?;
    let sink = host.default_output_device().context("this computer has no sound output")?;
    let monitor = format!("{}.monitor", sink.id()?.id());
    let device = host
        .input_devices()?
        .find(|d| d.id().is_ok_and(|id| id.id() == monitor))
        .with_context(|| format!("the sound output has no monitor ({monitor})"))?;
    let config = device.default_input_config().context("reading the sound output's format")?;
    Ok((device, config, name(&sink)))
}

fn name(d: &cpal::Device) -> String {
    d.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "sound output".into())
}
