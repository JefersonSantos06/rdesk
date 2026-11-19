//! File transfer over a dedicated bidirectional QUIC stream per file.
//!
//! Push (client → host inbox) and pull (host path → client) both resume from
//! an existing `.part` file and finish with a BLAKE3 check over the whole file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rdesk_proto::{
    DirEntry, FileData, FileReq, FileResp, MAX_CONTROL_MSG, MAX_FILE_MSG, StreamOpen, expect_msg, read_msg, write_msg,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

pub const CHUNK: usize = 64 * 1024;

/// Reduce a client-supplied name to a safe bare file name, or `None` if unusable.
pub fn sanitize_name(name: &str) -> Option<String> {
    let base = name.replace('\\', "/");
    let base = base.rsplit('/').next()?.trim();
    let mut s: String = base.chars().map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c }).collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() || s == "." || s == ".." || s.len() > 255 {
        return None;
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    let numbered =
        (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit();
    if RESERVED.contains(&stem.as_str()) || numbered {
        s.insert(0, '_');
    }
    Some(s)
}

/// `dir/name`, or `dir/name (1).ext`, `(2)`... if taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1.. {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

async fn hash_prefix(file: &mut tokio::fs::File, len: u64) -> Result<blake3::Hasher> {
    let mut h = blake3::Hasher::new();
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let mut remaining = len;
    let mut buf = vec![0u8; CHUNK];
    while remaining > 0 {
        let n = file.read(&mut buf[..CHUNK.min(remaining as usize)]).await?;
        if n == 0 {
            bail!("file shorter than expected while hashing");
        }
        h.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(h)
}

/// Stream `file` from its current position to the end as chunks, then `Done`.
async fn send_body<W: AsyncWrite + Unpin>(
    w: &mut W,
    file: &mut tokio::fs::File,
    mut hasher: blake3::Hasher,
    mut done: u64,
    total: u64,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        write_msg(w, &FileData::Chunk(buf[..n].to_vec())).await?;
        done += n as u64;
        progress(done, total);
    }
    if done != total {
        bail!("file changed size during transfer ({done} of {total} bytes)");
    }
    write_msg(w, &FileData::Done { blake3: *hasher.finalize().as_bytes() }).await?;
    Ok(())
}

/// Receive chunks into `file` (opened for append at `offset`), verify, return total size.
async fn recv_body<R: AsyncRead + Unpin>(
    r: &mut R,
    file: &mut tokio::fs::File,
    mut hasher: blake3::Hasher,
    mut done: u64,
    total: u64,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<()> {
    loop {
        match expect_msg::<_, FileData>(r, MAX_FILE_MSG).await? {
            FileData::Chunk(c) => {
                done += c.len() as u64;
                if done > total {
                    bail!("peer sent more data than announced");
                }
                hasher.update(&c);
                file.write_all(&c).await?;
                progress(done, total);
            }
            FileData::Done { blake3 } => {
                file.flush().await?;
                file.sync_all().await?;
                if done != total {
                    bail!("transfer ended at {done} of {total} bytes");
                }
                if hasher.finalize().as_bytes() != &blake3 {
                    bail!("checksum mismatch");
                }
                return Ok(());
            }
        }
    }
}

async fn open_part(path: &Path, max_len: u64) -> Result<(tokio::fs::File, u64)> {
    let mut f = tokio::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).await?;
    let mut len = f.metadata().await?.len();
    if len > max_len {
        f.set_len(0).await?;
        len = 0;
    }
    f.seek(std::io::SeekFrom::Start(len)).await?;
    Ok((f, len))
}

fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------

/// Serve one file stream (after the `StreamOpen::File` tag has been read).
pub async fn serve<R, W>(r: &mut R, w: &mut W, inbox: &Path) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match expect_msg::<_, FileReq>(r, MAX_CONTROL_MSG).await? {
        FileReq::Push { name, size } => {
            let Some(name) = sanitize_name(&name) else {
                write_msg(w, &FileResp::Reject { reason: "invalid file name".into() }).await?;
                return Ok(());
            };
            tokio::fs::create_dir_all(inbox).await.with_context(|| format!("creating {}", inbox.display()))?;
            // The partial file is keyed by name+size so a different file with the same name restarts.
            let part = inbox.join(format!(".{name}.{size}.part"));
            let (mut file, offset) = open_part(&part, size).await?;
            let hasher = hash_prefix(&mut file, offset).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            write_msg(w, &FileResp::Accept { offset }).await?;
            let res = recv_body(r, &mut file, hasher, offset, size, &mut |_, _| {}).await;
            drop(file);
            let result = match res {
                Ok(()) => {
                    let dest = unique_path(inbox, &name);
                    tokio::fs::rename(&part, &dest).await?;
                    tracing::info!("received {}", dest.display());
                    Ok(dest.file_name().unwrap_or_default().to_string_lossy().into_owned())
                }
                Err(e) => {
                    if e.to_string().contains("checksum") {
                        let _ = tokio::fs::remove_file(&part).await;
                    }
                    Err(format!("{e:#}"))
                }
            };
            write_msg(w, &FileResp::Complete { result }).await?;
        }
        FileReq::Pull { path, offset } => {
            let p = expand_home(&path);
            let mut file = match tokio::fs::File::open(&p).await {
                Ok(f) => f,
                Err(e) => {
                    write_msg(w, &FileResp::Reject { reason: format!("{}: {e}", p.display()) }).await?;
                    return Ok(());
                }
            };
            let meta = file.metadata().await?;
            if !meta.is_file() {
                write_msg(w, &FileResp::Reject { reason: "not a regular file".into() }).await?;
                return Ok(());
            }
            let size = meta.len();
            if offset > size {
                write_msg(w, &FileResp::Reject { reason: "offset beyond end of file".into() }).await?;
                return Ok(());
            }
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "download".into());
            write_msg(w, &FileResp::PullMeta { name, size }).await?;
            let hasher = hash_prefix(&mut file, offset).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            send_body(w, &mut file, hasher, offset, size, &mut |_, _| {}).await?;
        }
    }
    w.flush().await?;
    Ok(())
}

pub fn expand_home(path: &str) -> PathBuf {
    let home = || directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    if path.is_empty() || path == "~" {
        return home().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(h) = home()
    {
        let rest = if cfg!(windows) { rest.replace('/', "\\") } else { rest.to_string() };
        return h.join(rest);
    }
    PathBuf::from(path)
}

pub async fn list_dir(path: &str) -> Result<(String, Vec<DirEntry>), String> {
    let p = expand_home(path);
    let mut rd = tokio::fs::read_dir(&p).await.map_err(|e| format!("{}: {e}", p.display()))?;
    let mut out = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let Ok(meta) = e.metadata().await else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(DirEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            mtime,
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((p.to_string_lossy().into_owned(), out))
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Upload `local` into the host's inbox. Returns the name it was saved as.
pub async fn push(
    conn: &crate::transport::Connection,
    local: &Path,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<String> {
    let mut file = tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
    let size = file.metadata().await?.len();
    let name = local.file_name().ok_or_else(|| anyhow!("not a file: {}", local.display()))?.to_string_lossy();
    let (mut w, mut r) = conn.open_bi().await?;
    let _ = w.set_priority(-10);
    write_msg(&mut w, &StreamOpen::File).await?;
    write_msg(&mut w, &FileReq::Push { name: name.into_owned(), size }).await?;
    let offset = match expect_msg::<_, FileResp>(&mut r, MAX_CONTROL_MSG).await? {
        FileResp::Accept { offset } if offset <= size => offset,
        FileResp::Reject { reason } => bail!("host rejected upload: {reason}"),
        other => bail!("unexpected reply {other:?}"),
    };
    let hasher = hash_prefix(&mut file, offset).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    progress(offset, size);
    send_body(&mut w, &mut file, hasher, offset, size, progress).await?;
    let result = match read_msg::<_, FileResp>(&mut r, MAX_CONTROL_MSG).await? {
        Some(FileResp::Complete { result }) => result,
        other => bail!("unexpected reply {other:?}"),
    };
    let _ = w.finish();
    result.map_err(|e| anyhow!("host: {e}"))
}

/// Download `remote` from the host into `local_dir`. Returns the final path.
pub async fn pull(
    conn: &crate::transport::Connection,
    remote: &str,
    local_dir: &Path,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(local_dir).await?;
    let guess = sanitize_name(remote).ok_or_else(|| anyhow!("invalid remote path"))?;
    let part = part_path(&local_dir.join(&guess));
    for attempt in 0..2 {
        let (mut file, offset) = open_part(&part, u64::MAX).await?;
        let (mut w, mut r) = conn.open_bi().await?;
        let _ = w.set_priority(-10);
        write_msg(&mut w, &StreamOpen::File).await?;
        write_msg(&mut w, &FileReq::Pull { path: remote.to_string(), offset }).await?;
        let _ = w.finish();
        let (name, size) = match expect_msg::<_, FileResp>(&mut r, MAX_CONTROL_MSG).await? {
            FileResp::PullMeta { name, size } => (name, size),
            FileResp::Reject { reason } if reason.contains("offset beyond") && attempt == 0 => {
                drop(file);
                tokio::fs::remove_file(&part).await?;
                continue;
            }
            FileResp::Reject { reason } => bail!("host refused download: {reason}"),
            other => bail!("unexpected reply {other:?}"),
        };
        let hasher = hash_prefix(&mut file, offset).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        progress(offset, size);
        let res = recv_body(&mut r, &mut file, hasher, offset, size, progress).await;
        drop(file);
        if let Err(e) = res {
            if e.to_string().contains("checksum") {
                let _ = tokio::fs::remove_file(&part).await;
            }
            return Err(e);
        }
        let name = sanitize_name(&name).unwrap_or(guess);
        let dest = unique_path(local_dir, &name);
        tokio::fs::rename(&part, &dest).await?;
        return Ok(dest);
    }
    bail!("download failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(sanitize_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_name("C:\\Windows\\win.ini").as_deref(), Some("win.ini"));
        assert_eq!(sanitize_name("..").as_deref(), None);
        assert_eq!(sanitize_name("").as_deref(), None);
        assert_eq!(sanitize_name("a/b/").as_deref(), None);
        assert_eq!(sanitize_name("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(sanitize_name("com1").as_deref(), Some("_com1"));
        assert_eq!(sanitize_name("what?.txt. ").as_deref(), Some("what_.txt"));
    }

    #[test]
    fn unique_paths() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a.txt"));
        std::fs::write(d.path().join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (1).txt"));
        std::fs::write(d.path().join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(d.path(), "a.txt"), d.path().join("a (2).txt"));
    }
}
