use std::io;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::wire::{JsonRpcRequest, JsonRpcResponse};

const MAX_LINE_BYTES: usize = 4 * 1024 * 1024; // 4 MiB — generous for Phase 0.

/// Read one JSON-RPC frame (newline-delimited). Returns `Ok(None)` on clean
/// EOF.
pub async fn read_message<R, T>(reader: &mut BufReader<R>) -> io::Result<Option<T>>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rpc frame exceeds max line size",
            ));
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<T>(trimmed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(Some(value));
    }
}

pub async fn write_message<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut s = serde_json::to_string(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    s.push('\n');
    writer.write_all(s.as_bytes()).await?;
    writer.flush().await
}

/// Convenience aliases used by the rest of the crate.
pub type ReadRequest<R> = fn(&mut BufReader<R>) -> Option<JsonRpcRequest>;
pub type WriteResponse<W> = fn(&mut W, &JsonRpcResponse) -> io::Result<()>;
