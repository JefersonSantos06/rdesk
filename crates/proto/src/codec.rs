//! Length-prefixed postcard framing.
//!
//! Every message on every stream is `u32 little-endian length` + `postcard bytes`.
//! Readers enforce a per-stream maximum so a misbehaving peer cannot make us
//! allocate unbounded memory.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum size for control/auth messages (clipboard text is the largest).
pub const MAX_CONTROL_MSG: usize = 16 * 1024 * 1024;
/// Maximum size for a single video message (one tile, or a frame header).
pub const MAX_VIDEO_MSG: usize = 4 * 1024 * 1024;
/// Maximum size for input events and acks.
pub const MAX_SMALL_MSG: usize = 64 * 1024;
/// Maximum size for a file-transfer message (chunks are 64 KiB of payload).
pub const MAX_FILE_MSG: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message of {len} bytes exceeds limit of {max}")]
    TooLarge { len: usize, max: usize },
    #[error("stream ended in the middle of a message")]
    Truncated,
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
}

/// Serialize `msg` into a framed buffer (length prefix included).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::new();
    encode_into(msg, &mut buf)?;
    Ok(buf)
}

/// Append a framed message to an existing buffer (to batch several messages in one write).
pub fn encode_into<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let taken = std::mem::take(out);
    *out = postcard::to_extend(msg, taken)?;
    let len = out.len() - start - 4;
    let len32 = u32::try_from(len).map_err(|_| CodecError::TooLarge { len, max: u32::MAX as usize })?;
    out[start..start + 4].copy_from_slice(&len32.to_le_bytes());
    Ok(())
}

/// Decode one message from a complete payload (without length prefix).
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(payload)?)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let buf = encode(msg)?;
    w.write_all(&buf).await?;
    Ok(())
}

/// Read one message. Returns `Ok(None)` on a clean end-of-stream at a message boundary.
pub async fn read_msg<R, T>(r: &mut R, max: usize) -> Result<Option<T>, CodecError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut len_buf[got..]).await?;
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(CodecError::Truncated) };
        }
        got += n;
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max {
        return Err(CodecError::TooLarge { len, max });
    }
    let mut payload = vec![0u8; len];
    match r.read_exact(&mut payload).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(CodecError::Truncated);
        }
        Err(e) => return Err(e.into()),
    }
    Ok(Some(decode(&payload)?))
}

/// Like [`read_msg`] but treats end-of-stream as an error.
pub async fn expect_msg<R, T>(r: &mut R, max: usize) -> Result<T, CodecError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    read_msg(r, max).await?.ok_or(CodecError::Truncated)
}
