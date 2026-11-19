//! `rdesk connect --headless`: connect without a window, report stats, and
//! optionally dump the final picture. Used for testing links and the host.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::conn::{self, Cmd, ConnectOpts, UiEvent};

pub struct HeadlessOpts {
    pub frames: u64,
    pub timeout: Duration,
    pub dump: Option<PathBuf>,
}

pub fn run(connect: ConnectOpts, opts: HeadlessOpts) -> Result<()> {
    let (tx, rx) = mpsc::channel::<UiEvent>();
    let handle = conn::spawn(connect, move |ev| {
        let _ = tx.send(ev);
    });
    let start = Instant::now();
    let mut first_frame: Option<Instant> = None;
    let mut result = Ok(());
    loop {
        let left = opts.timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            result = Err(anyhow::anyhow!("timed out"));
            break;
        }
        match rx.recv_timeout(left.min(Duration::from_millis(500))) {
            Ok(UiEvent::Status(s)) => eprintln!("status: {s}"),
            Ok(UiEvent::Notice(s)) => eprintln!("host: {s}"),
            Ok(UiEvent::PasswordNeeded { reply, .. }) => reply.send(None),
            Ok(UiEvent::Connected(h)) => {
                eprintln!("connected to {} ({}), {} monitor(s)", h.hostname, h.os, h.monitors.len());
            }
            Ok(UiEvent::Stats { rtt_ms, fps, kbps }) => eprintln!("rtt {rtt_ms} ms, {fps:.1} fps, {kbps:.0} kbit/s"),
            Ok(UiEvent::Fatal(m)) => {
                result = Err(anyhow::anyhow!(m));
                break;
            }
            Ok(UiEvent::Quit) => break,
            Ok(UiEvent::FrameUpdated) => {
                first_frame.get_or_insert_with(Instant::now);
                let _ = conn::take_updates(&handle.view);
                if handle.view.lock().unwrap().frames >= opts.frames {
                    break;
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let frames = handle.view.lock().unwrap().frames;
    if let Some(t) = first_frame {
        eprintln!(
            "received {frames} frames; first after {:.0} ms, {:.1} fps average",
            (t - start).as_secs_f64() * 1000.0,
            frames as f64 / t.elapsed().as_secs_f64().max(0.001)
        );
    }
    if let Some(path) = &opts.dump {
        let v = handle.view.lock().unwrap();
        write_ppm(path, v.frame.width, v.frame.height, &v.frame.data)?;
        eprintln!("wrote {}", path.display());
    }
    let _ = handle.cmd.send(Cmd::Quit);
    let _ = handle.thread.join();
    result?;
    if frames == 0 {
        bail!("no frames received");
    }
    Ok(())
}

/// Binary PPM: viewable everywhere, no image-codec dependency.
fn write_ppm(path: &PathBuf, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.reserve(w as usize * h as usize * 3);
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, out)?;
    Ok(())
}
