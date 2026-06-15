use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::framing::{read_message, write_message};
use crate::methods::{
    method_names, AppendEventRequest, AppendEventResponse, HelloRequest, HelloResponse,
};
use crate::wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("shutting down")]
    ShuttingDown,
}

impl RpcError {
    pub fn to_jsonrpc(&self) -> JsonRpcError {
        match self {
            RpcError::InvalidParams(m) => JsonRpcError::invalid_params(m.clone()),
            RpcError::Internal(m) => JsonRpcError::internal(m.clone()),
            RpcError::ShuttingDown => JsonRpcError::toffee(-32100, "daemon shutting down"),
        }
    }
}

#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn hello(&self, req: HelloRequest) -> Result<HelloResponse, RpcError>;
    async fn append_event(
        &self,
        req: AppendEventRequest,
    ) -> Result<AppendEventResponse, RpcError>;
    /// Called when a client invokes `toffee.daemon.shutdown`. The handler
    /// is expected to begin shutdown and return; the server loop will
    /// reply, then the daemon's main loop will see the shutdown signal
    /// and exit.
    async fn daemon_shutdown(&self) -> Result<(), RpcError>;
}

/// Serve clients on `listener` using `handler`. Runs until the shutdown
/// receiver fires or the listener errors fatally.
pub async fn serve<H: Handler>(
    listener: UnixListener,
    handler: Arc<H>,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                tracing::info!("rpc server: shutdown signal received");
                return Ok(());
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let handler = handler.clone();
                        let shutdown = shutdown.resubscribe();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, handler, shutdown).await {
                                tracing::debug!(error = ?e, "rpc connection ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "rpc accept failed");
                    }
                }
            }
        }
    }
}

async fn handle_connection<S, H>(
    stream: S,
    handler: Arc<H>,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    H: Handler,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                return Ok(());
            }
            msg = read_message::<_, JsonRpcRequest>(&mut reader) => {
                let request = match msg {
                    Ok(Some(req)) => req,
                    Ok(None) => return Ok(()), // clean EOF
                    Err(e) => {
                        // Invalid frame: try to reply with parse error if we
                        // can; otherwise close the connection.
                        let resp = JsonRpcResponse::err(None, JsonRpcError::parse_error(e.to_string()));
                        let _ = write_message(&mut write_half, &resp).await;
                        return Ok(());
                    }
                };

                let response = dispatch(handler.as_ref(), request).await;
                if let Some(resp) = response {
                    write_message(&mut write_half, &resp).await?;
                }
            }
        }
    }
}

/// Returns `None` for notifications (no id) where we should not reply.
async fn dispatch<H: Handler>(handler: &H, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    let id = req.id.clone();
    let is_notification = id.is_none();

    if req.jsonrpc != "2.0" {
        if is_notification {
            return None;
        }
        return Some(JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_request("jsonrpc must be \"2.0\""),
        ));
    }

    let result = match req.method.as_str() {
        method_names::HELLO => call_handler(req.params, |r| handler.hello(r)).await,
        method_names::APPEND_EVENT => call_handler(req.params, |r| handler.append_event(r)).await,
        method_names::DAEMON_SHUTDOWN => {
            handler
                .daemon_shutdown()
                .await
                .map(|()| serde_json::Value::Null)
                .map_err(|e| e.to_jsonrpc())
        }
        other => Err(JsonRpcError::method_not_found(other)),
    };

    if is_notification {
        return None;
    }

    match result {
        Ok(value) => Some(JsonRpcResponse::ok(id, value)),
        Err(err) => Some(JsonRpcResponse::err(id, err)),
    }
}

async fn call_handler<P, R, F, Fut>(
    params: serde_json::Value,
    f: F,
) -> Result<serde_json::Value, JsonRpcError>
where
    P: serde::de::DeserializeOwned,
    R: serde::Serialize,
    F: FnOnce(P) -> Fut,
    Fut: std::future::Future<Output = Result<R, RpcError>>,
{
    let parsed: P = serde_json::from_value(params)
        .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
    let value = f(parsed).await.map_err(|e| e.to_jsonrpc())?;
    serde_json::to_value(&value).map_err(|e| JsonRpcError::internal(e.to_string()))
}

// Unused alias kept for crate readers.
#[allow(dead_code)]
type _RequestIdAlias = RequestId;
#[allow(dead_code)]
type _AppendAlias = AppendEventResponse;
