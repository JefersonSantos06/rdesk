//! The remote terminal's stream to the host's shell, without any drawing: the app draws
//! the screen itself (the desktop's egui widget, the phone app's own view).

use anyhow::Result;
use rdesk_common::transport::Connection;
use rdesk_proto::{MAX_SMALL_MSG, StreamOpen, TermClient, TermHost, read_msg, write_msg};
use tokio::sync::mpsc;

/// One open terminal: keys and sizes go out on `tx`, the host's output comes in on `rx`.
pub struct TerminalLink {
    pub tx: mpsc::UnboundedSender<TermClient>,
    pub rx: mpsc::UnboundedReceiver<TermHost>,
}

/// Input is sent in pieces the host accepts (a big paste becomes several messages).
const INPUT_CHUNK: usize = 32 * 1024;

/// A [`TerminalLink`] handed from the connection to the window (events must be `Clone`).
#[derive(Clone)]
pub struct TermSlot(pub std::sync::Arc<std::sync::Mutex<Option<TerminalLink>>>);

impl std::fmt::Debug for TermSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TermSlot")
    }
}

/// Open a terminal on an authenticated connection (`admin`: with administrator rights, on
/// Windows); `wake` runs when output arrives.
pub async fn open(
    conn: Connection,
    cols: u16,
    rows: u16,
    admin: bool,
    wake: impl Fn() + Send + 'static,
) -> Result<TerminalLink> {
    let (mut w, mut r) = conn.open_bi().await?;
    write_msg(&mut w, &StreamOpen::Terminal).await?;
    let start = if admin { TermClient::StartAdmin { cols, rows } } else { TermClient::Start { cols, rows } };
    write_msg(&mut w, &start).await?;
    let (tx, mut out) = mpsc::unbounded_channel::<TermClient>();
    let (inc, rx) = mpsc::unbounded_channel::<TermHost>();
    tokio::spawn(async move {
        while let Some(m) = out.recv().await {
            let parts = match m {
                TermClient::Input(bytes) if bytes.len() > INPUT_CHUNK => {
                    bytes.chunks(INPUT_CHUNK).map(|c| TermClient::Input(c.to_vec())).collect()
                }
                m => vec![m],
            };
            for p in parts {
                if write_msg(&mut w, &p).await.is_err() {
                    return;
                }
            }
        }
        let _ = w.finish();
    });
    tokio::spawn(async move {
        while let Ok(Some(m)) = read_msg::<_, TermHost>(&mut r, MAX_SMALL_MSG).await {
            if inc.send(m).is_err() {
                return;
            }
            wake();
        }
        wake();
    });
    Ok(TerminalLink { tx, rx })
}

/// What a terminal answers to the questions programs print: where the cursor is
/// (`ESC[6n`, which Windows' ConPTY asks before anything else), its status (`ESC[5n`) and
/// what kind of terminal it is (`ESC[c`).
pub fn answers(output: &[u8], cursor: (u16, u16)) -> Vec<u8> {
    let mut out = Vec::new();
    let count = |q: &[u8]| output.windows(q.len()).filter(|w| *w == q).count();
    for _ in 0..count(b"\x1b[6n") {
        out.extend_from_slice(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes());
    }
    for _ in 0..count(b"\x1b[5n") {
        out.extend_from_slice(b"\x1b[0n");
    }
    for _ in 0..(count(b"\x1b[c") + count(b"\x1b[0c")) {
        out.extend_from_slice(b"\x1b[?1;2c");
    }
    out
}
