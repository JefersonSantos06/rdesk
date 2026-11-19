//! File transfers with the rights of whoever is signed in. The host started by the service
//! runs as SYSTEM, which may read and write anything; files are instead handled by a
//! helper (`rdesk host file-helper`) started as the signed-in user, so a session sees
//! exactly what that person sees and received files belong to them.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rdesk_proto::DirEntry;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::proc::{self, Env, Spawned};

fn helper(args: &[&str], stdin: bool) -> Result<Spawned> {
    let session = proc::console_session().ok_or_else(|| anyhow!("nobody is at this computer's screen"))?;
    let token = proc::user_token(session)?;
    let exe = std::env::current_exe().context("finding rdesk.exe")?;
    let mut all = vec!["host", "file-helper"];
    all.extend_from_slice(args);
    proc::spawn(&token, &exe, &all, Env::OfUser, stdin, true)
}

async fn reap(sp: &Spawned) {
    for _ in 0..50 {
        if let Some(code) = sp.child.try_wait() {
            if code != 0 {
                tracing::warn!("file helper exited with code {code}");
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sp.child.kill();
}

/// [`rdesk_common::files::serve`], run by the helper; this side only relays the stream.
pub async fn serve_as_user<R, W>(r: &mut R, w: &mut W, inbox: Option<&Path>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let inbox = inbox.map(|p| p.to_string_lossy().into_owned());
    let mut args = vec!["serve"];
    if let Some(dir) = &inbox {
        args.extend(["--inbox", dir]);
    }
    relay_as_user(&args, r, w).await
}

/// A terminal ([`crate::terminal::serve`]) in a helper started as the signed-in user.
pub async fn terminal_as_user<R, W>(r: &mut R, w: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    relay_as_user(&["terminal"], r, w).await
}

/// Start `rdesk host file-helper <args>` as the signed-in user and relay one stream to it.
async fn relay_as_user<R, W>(args: &[&str], r: &mut R, w: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut sp = helper(args, true)?;
    let mut to_helper = tokio::fs::File::from_std(sp.stdin.take().expect("piped stdin"));
    let mut from_helper = tokio::fs::File::from_std(sp.stdout.take().expect("piped stdout"));
    let send = async {
        // The client may keep its side open while it waits for the answer: once its data is
        // in, close the helper's input and wait for the other direction to finish.
        let _ = tokio::io::copy(r, &mut to_helper).await;
        let _ = to_helper.flush().await;
        drop(to_helper);
        std::future::pending::<()>().await
    };
    let result = tokio::select! {
        r = tokio::io::copy(&mut from_helper, w) => r.map(|_| ()).map_err(Into::into),
        _ = send => unreachable!(),
    };
    reap(&sp).await;
    result
}

/// [`rdesk_common::files::list_dir`] as the signed-in user.
pub async fn list_as_user(path: &str) -> Result<(String, Vec<DirEntry>), String> {
    let run = async {
        let mut sp = helper(&["list", path], false)?;
        let mut out = Vec::new();
        tokio::fs::File::from_std(sp.stdout.take().expect("piped stdout")).read_to_end(&mut out).await?;
        reap(&sp).await;
        serde_json::from_slice::<Result<(String, Vec<DirEntry>), String>>(&out).context("reading the file helper")
    };
    match tokio::time::timeout(Duration::from_secs(20), run).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err("listing the folder took too long".into()),
    }
}
