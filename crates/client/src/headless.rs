//! `rdesk connect --headless`: connect without a window, report stats, and
//! optionally dump the final picture. Used for testing links and the host.
//!
//! `--input` plays mouse and keyboard steps once the first picture arrives (see [`Step`]),
//! to try a host's input without anyone at a window.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rdesk_proto::{InputEvent, Key, MouseButton};

use crate::conn::{self, Cmd, ConnectOpts, UiEvent};

pub struct HeadlessOpts {
    pub frames: u64,
    pub timeout: Duration,
    pub dump: Option<PathBuf>,
    /// Played once the first picture arrives.
    pub input: Vec<Step>,
}

/// One step of `--input` (steps are separated by `;`, coordinates are 0..1 across the
/// picture):
/// `move X Y`, `down`, `up`, `click X Y`, `drag X1 Y1 X2 Y2`, `right`, `scroll DY`,
/// `key NAME` (a `Key` name: `Escape`, `KeyA`…; `+` joins held keys: `ControlLeft+KeyV`),
/// `text WORDS`, `wait MS`.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Event(InputEvent),
    Wait(Duration),
}

/// Parse an `--input` script.
pub fn parse_input(script: &str) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let pos = |v: &str| -> Result<u16> {
        let f: f64 = v.parse().with_context(|| format!("{v}: expected a number from 0 to 1"))?;
        Ok((f.clamp(0.0, 1.0) * 65535.0).round() as u16)
    };
    let ev = Step::Event;
    let button = |down| ev(InputEvent::MouseButton { button: MouseButton::Left, down });
    for part in script.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (word, rest) = part.split_once(' ').unwrap_or((part, ""));
        let args: Vec<&str> = rest.split_whitespace().collect();
        let arg = |i: usize| args.get(i).copied().with_context(|| format!("{part}: missing a value"));
        match word {
            "move" => out.push(ev(InputEvent::MouseMove { x: pos(arg(0)?)?, y: pos(arg(1)?)? })),
            "down" => out.push(button(true)),
            "up" => out.push(button(false)),
            "click" => {
                out.push(ev(InputEvent::MouseMove { x: pos(arg(0)?)?, y: pos(arg(1)?)? }));
                out.extend([button(true), Step::Wait(Duration::from_millis(50)), button(false)]);
            }
            "drag" => {
                let (x1, y1, x2, y2) = (pos(arg(0)?)?, pos(arg(1)?)?, pos(arg(2)?)?, pos(arg(3)?)?);
                out.extend([ev(InputEvent::MouseMove { x: x1, y: y1 }), button(true)]);
                for i in 1..=10i64 {
                    let at = |a: u16, b: u16| (a as i64 + (b as i64 - a as i64) * i / 10) as u16;
                    out.push(Step::Wait(Duration::from_millis(30)));
                    out.push(ev(InputEvent::MouseMove { x: at(x1, x2), y: at(y1, y2) }));
                }
                out.extend([Step::Wait(Duration::from_millis(100)), button(false)]);
            }
            "right" => {
                for down in [true, false] {
                    out.push(ev(InputEvent::MouseButton { button: MouseButton::Right, down }));
                }
            }
            "scroll" => {
                let dy: f32 = arg(0)?.parse().with_context(|| format!("{part}: expected notches"))?;
                out.push(ev(InputEvent::Scroll { dx: 0.0, dy }));
            }
            "key" => {
                let keys = arg(0)?
                    .split('+')
                    .map(|n| Key::ALL.iter().copied().find(|k| k.name() == n).with_context(|| format!("no key {n}")))
                    .collect::<Result<Vec<_>>>()?;
                out.extend(keys.iter().map(|&key| ev(InputEvent::Key { key, down: true })));
                out.extend(keys.iter().rev().map(|&key| ev(InputEvent::Key { key, down: false })));
            }
            "text" => out.push(ev(InputEvent::Text { text: rest.to_string() })),
            "wait" => {
                let ms: u64 = arg(0)?.parse().with_context(|| format!("{part}: expected milliseconds"))?;
                out.push(Step::Wait(Duration::from_millis(ms)));
            }
            _ => bail!("{part}: unknown step (move, down, up, click, drag, right, scroll, key, text, wait)"),
        }
    }
    Ok(out)
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
            Ok(UiEvent::Access(a)) => eprintln!("access: {}", a.label()),
            Ok(UiEvent::Connected(h)) => {
                eprintln!("connected to {} ({}), {} monitor(s)", h.hostname, h.os, h.monitors.len());
            }
            Ok(UiEvent::Stats { rtt_ms, fps, kbps, route }) => {
                eprintln!("rtt {rtt_ms} ms ({route}), {fps:.1} fps, {kbps:.0} kbit/s")
            }
            Ok(UiEvent::Fatal(m)) => {
                result = Err(anyhow::anyhow!(m));
                break;
            }
            Ok(UiEvent::Quit) => break,
            Ok(UiEvent::FrameUpdated) => {
                if first_frame.is_none() && !opts.input.is_empty() {
                    let (cmd, steps) = (handle.cmd.clone(), opts.input.clone());
                    std::thread::spawn(move || {
                        for step in steps {
                            match step {
                                Step::Event(ev) => {
                                    let _ = cmd.send(Cmd::Input(ev));
                                }
                                Step::Wait(d) => std::thread::sleep(d),
                            }
                        }
                        eprintln!("input played");
                    });
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_scripts_become_events() {
        let steps = parse_input("click 0.5 1; key ControlLeft+KeyV; text a b; wait 20; scroll -2").unwrap();
        assert_eq!(steps[0], Step::Event(InputEvent::MouseMove { x: 32768, y: 65535 }));
        assert!(matches!(steps[1], Step::Event(InputEvent::MouseButton { button: MouseButton::Left, down: true })));
        assert_eq!(
            &steps[4..],
            [
                Step::Event(InputEvent::Key { key: Key::ControlLeft, down: true }),
                Step::Event(InputEvent::Key { key: Key::KeyV, down: true }),
                Step::Event(InputEvent::Key { key: Key::KeyV, down: false }),
                Step::Event(InputEvent::Key { key: Key::ControlLeft, down: false }),
                Step::Event(InputEvent::Text { text: "a b".into() }),
                Step::Wait(Duration::from_millis(20)),
                Step::Event(InputEvent::Scroll { dx: 0.0, dy: -2.0 }),
            ]
        );
        assert!(parse_input("key Nope").is_err());
        assert!(parse_input("fly 1 2").is_err());
    }
}
