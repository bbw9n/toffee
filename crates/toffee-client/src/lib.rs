//! Async client for `toffeed`.
//!
//! Phase 0 surface: `connect`, `hello`, `append_event`, `shutdown_daemon`.
//! Later phases will fill in `read_context`, `search_memory`, etc.

mod connect;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use toffee_core::{paths, EventId, EventInput};
use toffee_rpc::wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};
use toffee_rpc::{framing, methods::method_names, AppendEventResponse, HelloRequest, ServerInfo};

pub use connect::{spawn_daemon_if_needed, ConnectOptions};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rpc error ({code}): {message}")]
    Rpc { code: i64, message: String },
    #[error("decode error: {0}")]
    Decode(String),
    #[error("daemon not running and auto-spawn failed: {0}")]
    SpawnFailed(String),
    #[error("connection closed by peer")]
    Closed,
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Decode(e.to_string())
    }
}

impl From<JsonRpcError> for ClientError {
    fn from(e: JsonRpcError) -> Self {
        ClientError::Rpc {
            code: e.code,
            message: e.message,
        }
    }
}

/// Async toffee client. One client owns one socket connection; cloning is
/// not supported in Phase 0 — share via `Arc<Client>` if you need multiple
/// callers.
pub struct Client {
    reader: Mutex<BufReader<tokio::io::ReadHalf<UnixStream>>>,
    writer: Mutex<tokio::io::WriteHalf<UnixStream>>,
    next_id: AtomicI64,
    socket_path: PathBuf,
}

impl Client {
    /// Connect to the default daemon socket, auto-spawning `toffeed` if it
    /// isn't running and `auto_spawn` is enabled (default).
    pub async fn connect() -> Result<Self, ClientError> {
        Self::connect_with(ConnectOptions::default()).await
    }

    pub async fn connect_with(opts: ConnectOptions) -> Result<Self, ClientError> {
        let socket = opts
            .socket_path
            .clone()
            .unwrap_or_else(paths::socket_path);

        // Fast path: socket already accepting.
        match UnixStream::connect(&socket).await {
            Ok(s) => return Ok(Self::wrap(s, socket)),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                if !opts.auto_spawn {
                    return Err(e.into());
                }
            }
            Err(e) => return Err(e.into()),
        }

        // Slow path: try to spawn the daemon and wait for the socket.
        connect::spawn_daemon_if_needed(&opts)
            .map_err(|e| ClientError::SpawnFailed(e.to_string()))?;

        let deadline = std::time::Instant::now() + opts.connect_timeout;
        let mut delay = Duration::from_millis(25);
        loop {
            match UnixStream::connect(&socket).await {
                Ok(s) => return Ok(Self::wrap(s, socket)),
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(200));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Connect to an explicit socket path. Does not auto-spawn.
    pub async fn connect_to(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&path).await?;
        Ok(Self::wrap(stream, path))
    }

    fn wrap(stream: UnixStream, socket: PathBuf) -> Self {
        let (r, w) = tokio::io::split(stream);
        Client {
            reader: Mutex::new(BufReader::new(r)),
            writer: Mutex::new(w),
            next_id: AtomicI64::new(1),
            socket_path: socket,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn hello(
        &self,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
    ) -> Result<ServerInfo, ClientError> {
        self.call(
            method_names::HELLO,
            HelloRequest {
                client_name: Some(client_name.into()),
                client_version: Some(client_version.into()),
            },
        )
        .await
    }

    pub async fn append_event(&self, input: EventInput) -> Result<EventId, ClientError> {
        let resp: AppendEventResponse = self.call(method_names::APPEND_EVENT, input).await?;
        Ok(resp.event_id)
    }

    pub async fn shutdown_daemon(&self) -> Result<(), ClientError> {
        let _: serde_json::Value = self
            .call(method_names::DAEMON_SHUTDOWN, serde_json::Value::Null)
            .await?;
        Ok(())
    }

    async fn call<P, R>(&self, method: &str, params: P) -> Result<R, ClientError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(RequestId::Number(id)),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        // Lock writer to send; release before awaiting the read so that the
        // critical section is short. For Phase 0 the client is effectively
        // single-flight per `Client` instance anyway.
        {
            let mut w = self.writer.lock().await;
            framing::write_message(&mut *w, &request).await?;
            w.flush().await?;
        }

        let mut r = self.reader.lock().await;
        let resp: Option<JsonRpcResponse> = framing::read_message(&mut *r).await?;
        let resp = resp.ok_or(ClientError::Closed)?;

        if let Some(err) = resp.error {
            return Err(err.into());
        }
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        Ok(serde_json::from_value(result)?)
    }
}
