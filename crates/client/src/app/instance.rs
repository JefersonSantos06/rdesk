//! One app per user: a second launch (double-click, autostart) shows the running
//! window instead of starting another host on the same port.
//!
//! The running app listens on a random loopback port written, with a random token, to
//! `instance` in the config folder; a newcomer sends `<token> show` (or `quit` when it
//! takes over, e.g. after "Restart as administrator").

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rdesk_common::config;

pub enum Msg {
    Show,
    Quit,
}

/// Held by the running app; removes the instance file on drop.
pub struct Guard {
    path: PathBuf,
    contents: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).is_ok_and(|c| c == self.contents) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn path() -> Result<PathBuf> {
    Ok(config::ensure_config_dir()?.join("instance"))
}

fn send(addr: SocketAddr, token: &str, what: &str) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else { return false };
    let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
    s.write_all(format!("{token} {what}\n").as_bytes()).is_ok()
}

fn running() -> Option<(SocketAddr, String)> {
    let text = std::fs::read_to_string(path().ok()?).ok()?;
    let (port, token) = text.trim().split_once(' ')?;
    Some((SocketAddr::from(([127, 0, 0, 1], port.parse().ok()?)), token.to_string()))
}

/// `Ok(None)`: another instance is running and was asked to show itself; exit.
pub fn acquire(takeover: bool, on_msg: impl Fn(Msg) + Send + 'static) -> Result<Option<Guard>> {
    if let Some((addr, token)) = running() {
        if !takeover {
            if send(addr, &token, "show") {
                return Ok(None);
            }
        } else if send(addr, &token, "quit") {
            // Wait for it to release the host port and the instance socket.
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                std::thread::sleep(Duration::from_millis(200));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    };
    let contents = format!("{port} {token}");
    let path = path()?;
    config::write_private(&path, contents.as_bytes())?;

    std::thread::Builder::new().name("rdesk-instance".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_err() {
                continue;
            }
            match line.trim().split_once(' ') {
                Some((t, "show")) if t == token => on_msg(Msg::Show),
                Some((t, "quit")) if t == token => on_msg(Msg::Quit),
                _ => {}
            }
        }
    })?;
    Ok(Some(Guard { path, contents }))
}
