use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::framing::{read_message, write_message};
use crate::methods::{
    method_names, AddMemoryRequest, AddMemoryResponse, AppendEventRequest, AppendEventResponse,
    ForgetMemoryRequest, GetConflictRequest, GetConflictResponse, GetEntityPageRequest,
    GetEntityPageResponse, GetMemoryRequest, GetMemoryResponse, HelloRequest, HelloResponse,
    InspectProvenanceRequest, InspectProvenanceResponse, ListConflictsRequest,
    ListConflictsResponse, ListEntitiesRequest, ListEntitiesResponse, ListMemoriesRequest,
    ListMemoriesResponse, ReadContextRequest, ReadContextResponse, RebuildIndexesResponse,
    RecordFeedbackRequest, RecordFeedbackResponse, ResolveConflictRequest, ResolveConflictResponse,
    SearchMemoryRequest, SearchMemoryResponse, WhyMemoryRequest, WhyMemoryResponse,
    WorkerStatusRequest, WorkerStatusResponse,
};
use crate::wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};
use toffee_core::Notification;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("shutting down")]
    ShuttingDown,
}

impl RpcError {
    pub fn to_jsonrpc(&self) -> JsonRpcError {
        match self {
            RpcError::InvalidParams(m) => JsonRpcError::invalid_params(m.clone()),
            RpcError::NotFound(m) => JsonRpcError::toffee(-32101, format!("not found: {m}")),
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
    async fn record_feedback(
        &self,
        req: RecordFeedbackRequest,
    ) -> Result<RecordFeedbackResponse, RpcError>;
    async fn add_memory(&self, req: AddMemoryRequest) -> Result<AddMemoryResponse, RpcError>;
    async fn forget_memory(&self, req: ForgetMemoryRequest) -> Result<(), RpcError>;
    async fn list_memories(
        &self,
        req: ListMemoriesRequest,
    ) -> Result<ListMemoriesResponse, RpcError>;
    async fn get_memory(&self, req: GetMemoryRequest) -> Result<GetMemoryResponse, RpcError>;
    async fn list_entities(
        &self,
        req: ListEntitiesRequest,
    ) -> Result<ListEntitiesResponse, RpcError>;
    async fn get_entity_page(
        &self,
        req: GetEntityPageRequest,
    ) -> Result<GetEntityPageResponse, RpcError>;
    async fn search_memory(
        &self,
        req: SearchMemoryRequest,
    ) -> Result<SearchMemoryResponse, RpcError>;
    async fn read_context(
        &self,
        req: ReadContextRequest,
    ) -> Result<ReadContextResponse, RpcError>;
    async fn inspect_provenance(
        &self,
        req: InspectProvenanceRequest,
    ) -> Result<InspectProvenanceResponse, RpcError>;
    async fn list_conflicts(
        &self,
        req: ListConflictsRequest,
    ) -> Result<ListConflictsResponse, RpcError>;
    async fn get_conflict(
        &self,
        req: GetConflictRequest,
    ) -> Result<GetConflictResponse, RpcError>;
    async fn resolve_conflict(
        &self,
        req: ResolveConflictRequest,
    ) -> Result<ResolveConflictResponse, RpcError>;
    async fn worker_status(
        &self,
        req: WorkerStatusRequest,
    ) -> Result<WorkerStatusResponse, RpcError>;
    async fn why_memory(
        &self,
        req: WhyMemoryRequest,
    ) -> Result<WhyMemoryResponse, RpcError>;
    /// Subscribe to the broadcast notification channel. Returning `None`
    /// disables server-side notification fan-out for this handler.
    fn notification_subscriber(&self) -> Option<broadcast::Receiver<Notification>> {
        None
    }
    async fn daemon_rebuild_indexes(&self) -> Result<RebuildIndexesResponse, RpcError>;
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
    let mut notifications = handler.notification_subscriber();

    loop {
        // Branch shape changes when the handler doesn't expose
        // notifications — `select!` requires the same return type per branch
        // so we encode "no notifications" by feeding a pending future.
        tokio::select! {
            _ = shutdown.recv() => {
                return Ok(());
            }
            msg = read_message::<_, JsonRpcRequest>(&mut reader) => {
                let request = match msg {
                    Ok(Some(req)) => req,
                    Ok(None) => return Ok(()), // clean EOF
                    Err(e) => {
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
            notification = recv_notification(&mut notifications) => {
                if let Some(n) = notification {
                    let frame = notification_to_request(&n);
                    if write_message(&mut write_half, &frame).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Helper that returns a pending future when there's no subscriber, so
/// `select!` has something to wait on. Receivers that lag (overflow) skip
/// the missed entries and resync.
async fn recv_notification(
    rx: &mut Option<broadcast::Receiver<Notification>>,
) -> Option<Notification> {
    let Some(receiver) = rx.as_mut() else {
        // No subscriber attached. Park forever — the other select branches
        // drive forward progress.
        std::future::pending::<()>().await;
        return None;
    };
    loop {
        match receiver.recv().await {
            Ok(n) => return Some(n),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "notification subscriber lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

fn notification_to_request(n: &Notification) -> JsonRpcRequest {
    // The Notification enum already carries the method and params via its
    // serde tags. Re-encode it into a JsonRpcRequest with id=None so it
    // lands on the wire as a JSON-RPC notification.
    let value = serde_json::to_value(n).unwrap_or(serde_json::Value::Null);
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or(n.method_name())
        .to_string();
    let params = value
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: None,
        method,
        params,
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
        method_names::RECORD_FEEDBACK => {
            call_handler(req.params, |r| handler.record_feedback(r)).await
        }
        method_names::ADD_MEMORY => call_handler(req.params, |r| handler.add_memory(r)).await,
        method_names::FORGET_MEMORY => {
            call_handler_unit(req.params, |r| handler.forget_memory(r)).await
        }
        method_names::LIST_MEMORIES => {
            call_handler(req.params, |r| handler.list_memories(r)).await
        }
        method_names::GET_MEMORY => call_handler(req.params, |r| handler.get_memory(r)).await,
        method_names::LIST_ENTITIES => {
            call_handler(req.params, |r| handler.list_entities(r)).await
        }
        method_names::GET_ENTITY_PAGE => {
            call_handler(req.params, |r| handler.get_entity_page(r)).await
        }
        method_names::SEARCH_MEMORY => {
            call_handler(req.params, |r| handler.search_memory(r)).await
        }
        method_names::READ_CONTEXT => {
            call_handler(req.params, |r| handler.read_context(r)).await
        }
        method_names::INSPECT_PROVENANCE => {
            call_handler(req.params, |r| handler.inspect_provenance(r)).await
        }
        method_names::LIST_CONFLICTS => {
            call_handler(req.params, |r| handler.list_conflicts(r)).await
        }
        method_names::GET_CONFLICT => {
            call_handler(req.params, |r| handler.get_conflict(r)).await
        }
        method_names::RESOLVE_CONFLICT => {
            call_handler(req.params, |r| handler.resolve_conflict(r)).await
        }
        method_names::WORKER_STATUS => {
            call_handler(req.params, |r| handler.worker_status(r)).await
        }
        method_names::WHY_MEMORY => {
            call_handler(req.params, |r| handler.why_memory(r)).await
        }
        method_names::DAEMON_REBUILD_INDEXES => handler
            .daemon_rebuild_indexes()
            .await
            .map(|r| serde_json::to_value(&r).unwrap_or(serde_json::Value::Null))
            .map_err(|e| e.to_jsonrpc()),
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

async fn call_handler_unit<P, F, Fut>(
    params: serde_json::Value,
    f: F,
) -> Result<serde_json::Value, JsonRpcError>
where
    P: serde::de::DeserializeOwned,
    F: FnOnce(P) -> Fut,
    Fut: std::future::Future<Output = Result<(), RpcError>>,
{
    let parsed: P = serde_json::from_value(params)
        .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
    f(parsed).await.map_err(|e| e.to_jsonrpc())?;
    Ok(serde_json::Value::Null)
}

// Unused alias kept for crate readers.
#[allow(dead_code)]
type _RequestIdAlias = RequestId;
#[allow(dead_code)]
type _AppendAlias = AppendEventResponse;
