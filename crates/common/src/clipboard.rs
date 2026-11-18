//! Text clipboard sync on a dedicated thread (used by both host and client).
//!
//! The thread polls the local clipboard every 500 ms and reports changes; text
//! received from the peer is set locally and remembered so it is not echoed back.

use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

pub const MAX_CLIPBOARD: usize = 10 * 1024 * 1024;

pub struct ClipboardSync {
    set_tx: mpsc::Sender<String>,
}

impl ClipboardSync {
    /// Start watching. Local changes are sent on `on_change`. Returns `None` if
    /// the platform clipboard is unavailable.
    pub fn spawn(on_change: UnboundedSender<String>) -> Option<Self> {
        let (set_tx, set_rx) = mpsc::channel::<String>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
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
                loop {
                    match set_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(text) => {
                            last = Some(blake3::hash(text.as_bytes()));
                            if let Err(e) = clip.set_text(text) {
                                tracing::debug!("setting clipboard failed: {e}");
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Ok(text) = clip.get_text() {
                                let h = blake3::hash(text.as_bytes());
                                if Some(h) != last {
                                    last = Some(h);
                                    if !text.is_empty() && text.len() <= MAX_CLIPBOARD && on_change.send(text).is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok()?;
        ready_rx.recv().ok()?.then_some(Self { set_tx })
    }

    pub fn set(&self, text: String) {
        if text.len() <= MAX_CLIPBOARD {
            let _ = self.set_tx.send(text);
        }
    }
}
