//! The remote terminal: a shell in a pseudo-terminal (ConPTY on Windows, a pty elsewhere)
//! wired to one stream. [`TermClient`] messages come in (keys, size), [`TermHost`] messages
//! go out (output, exit). The host started by the Windows service runs this in a helper
//! started as the signed-in user, so the shell is theirs, not SYSTEM's.

use std::io::{Read, Write};

use anyhow::{Result, anyhow};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rdesk_proto::{MAX_SMALL_MSG, TermClient, TermHost, expect_msg, read_msg, write_msg};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize { rows: rows.clamp(2, 500), cols: cols.clamp(10, 1000), pixel_width: 0, pixel_height: 0 }
}

/// The shell to start, and its name.
fn shell() -> (String, CommandBuilder) {
    if cfg!(windows) {
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.arg("-NoLogo");
        if let Some(home) = std::env::var_os("USERPROFILE") {
            cmd.cwd(home);
        }
        ("PowerShell".into(), cmd)
    } else {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("TERM", "xterm-256color");
        if let Some(home) = std::env::var_os("HOME") {
            cmd.cwd(home);
        }
        let name = std::env::var("SHELL").ok().and_then(|s| s.rsplit('/').next().map(str::to_string));
        (name.unwrap_or_else(|| "shell".into()), cmd)
    }
}

fn user() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default()
}

/// Run one terminal session on `r`/`w` (after the `StreamOpen::Terminal` tag).
pub async fn serve<R, W>(r: &mut R, w: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let TermClient::Start { cols, rows } = expect_msg::<_, TermClient>(r, MAX_SMALL_MSG).await? else {
        return Err(anyhow!("a terminal starts with its size"));
    };
    let started = (|| {
        let pair = native_pty_system().openpty(size(cols, rows)).map_err(|e| anyhow!("opening a terminal: {e}"))?;
        let (name, cmd) = shell();
        let child = pair.slave.spawn_command(cmd).map_err(|e| anyhow!("starting {name}: {e}"))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| anyhow!("{e}"))?;
        let writer = pair.master.take_writer().map_err(|e| anyhow!("{e}"))?;
        anyhow::Ok((name, pair.master, child, reader, writer))
    })();
    let (name, master, mut child, mut reader, mut writer) = match started {
        Ok(s) => s,
        Err(e) => {
            write_msg(w, &TermHost::Error(format!("{e:#}"))).await?;
            return Ok(());
        }
    };
    let mut killer = child.clone_killer();
    // Resizing happens between awaits; the lock is never held across one.
    let master = std::sync::Mutex::new(master);
    write_msg(w, &TermHost::Started { shell: name, user: user() }).await?;

    // The pty's blocking ends, on their own threads.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let (exit_tx, mut exit_rx) = oneshot::channel::<u32>();
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
        let _ = exit_tx.send(code);
    });
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });

    // Keys and sizes in; output and the exit out. Whichever side ends first ends it.
    let keys = async {
        while let Some(m) = read_msg::<_, TermClient>(r, MAX_SMALL_MSG).await? {
            match m {
                TermClient::Input(bytes) => {
                    let _ = in_tx.send(bytes);
                }
                TermClient::Resize { cols, rows } => {
                    let _ = master.lock().unwrap().resize(size(cols, rows));
                }
                TermClient::Start { .. } => {}
            }
        }
        anyhow::Ok(())
    };
    let output = async {
        let mut reading = true;
        loop {
            tokio::select! {
                out = out_rx.recv(), if reading => match out {
                    Some(bytes) => write_msg(w, &TermHost::Output(bytes)).await?,
                    None => reading = false,
                },
                code = &mut exit_rx => {
                    // What the shell printed last.
                    while let Ok(bytes) = out_rx.try_recv() {
                        write_msg(w, &TermHost::Output(bytes)).await?;
                    }
                    write_msg(w, &TermHost::Exited(code.unwrap_or(1))).await?;
                    w.flush().await?;
                    return anyhow::Ok(());
                }
            }
        }
    };
    let result = tokio::select! {
        r = keys => r,
        r = output => r,
    };
    // The client closed the window (or the stream broke): end the shell.
    let _ = killer.kill();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The pty and the shell on their own, over an in-memory pipe.
    #[tokio::test(flavor = "multi_thread")]
    async fn shell_starts_and_answers() {
        let (client, host) = tokio::io::duplex(1 << 16);
        let (mut hr, mut hw) = tokio::io::split(host);
        let server = tokio::spawn(async move { serve(&mut hr, &mut hw).await });
        let (mut cr, mut cw) = tokio::io::split(client);
        write_msg(&mut cw, &TermClient::Start { cols: 100, rows: 30 }).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(15), read_msg::<_, TermHost>(&mut cr, MAX_SMALL_MSG))
            .await
            .expect("nothing from the terminal")
            .unwrap()
            .unwrap();
        assert!(matches!(first, TermHost::Started { .. }), "{first:?}");
        write_msg(&mut cw, &TermClient::Input(b"exit\r".to_vec())).await.unwrap();
        let mut code = None;
        while code.is_none() {
            match tokio::time::timeout(Duration::from_secs(20), read_msg::<_, TermHost>(&mut cr, MAX_SMALL_MSG))
                .await
                .expect("the shell did not exit")
                .unwrap()
            {
                Some(TermHost::Exited(c)) => code = Some(c),
                // ConPTY asks where the cursor is before it goes on: answer like a terminal.
                Some(TermHost::Output(b)) => {
                    if b.windows(4).any(|w| w == b"\x1b[6n") {
                        write_msg(&mut cw, &TermClient::Input(b"\x1b[1;1R".to_vec())).await.unwrap();
                    }
                }
                Some(_) => {}
                None => panic!("closed without an exit code"),
            }
        }
        assert_eq!(code, Some(0));
        drop(cw);
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }
}
