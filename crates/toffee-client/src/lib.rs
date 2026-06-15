//! Async client for `toffeed`.
//!
//! One [`Client`] owns one socket connection. A background reader task
//! demuxes incoming frames into either request responses (matched by
//! JSON-RPC id) or daemon-pushed notifications (no id) — notifications get
//! broadcast to anyone who called [`Client::subscribe_notifications`].

mod connect;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex as ParkingMutex;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex};
use toffee_core::{
    paths, ConflictId, ContextPackage, ContextPackageId, Entity, EntityPage, EventId, EventInput,
    FeedbackKind, Memory, MemoryConflict, MemoryId, MemoryKind, Notification, ProvenanceReport,
    Scope, WhyMemoryReport, WorkerStatus,
};
use toffee_rpc::wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};
use toffee_rpc::{
    framing, methods::method_names, AddMemoryRequest, AddMemoryResponse, AppendEventResponse,
    ForgetMemoryRequest, GetConflictRequest, GetConflictResponse, GetEntityPageRequest,
    GetEntityPageResponse, GetMemoryRequest, GetMemoryResponse, HelloRequest,
    InspectProvenanceRequest, InspectProvenanceResponse, ListConflictsRequest,
    ListConflictsResponse, ListEntitiesRequest, ListEntitiesResponse, ListMemoriesRequest,
    ListMemoriesResponse, ReadContextRequest, ReadContextResponse, RebuildIndexesResponse,
    RecordFeedbackRequest, RecordFeedbackResponse, ResolveConflictAction, ResolveConflictRequest,
    ResolveConflictResponse, SearchMemoryHit, SearchMemoryRequest, SearchMemoryResponse,
    ServerInfo, WhyMemoryRequest, WhyMemoryResponse, WorkerStatusRequest, WorkerStatusResponse,
};

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

type PendingMap = Arc<ParkingMutex<HashMap<i64, oneshot::Sender<JsonRpcResponse>>>>;

/// Async toffee client. One client owns one socket connection.
pub struct Client {
    writer: Mutex<tokio::io::WriteHalf<UnixStream>>,
    next_id: AtomicI64,
    pending: PendingMap,
    notifications_tx: broadcast::Sender<Notification>,
    socket_path: PathBuf,
    _reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // Aborting the reader task lets us avoid resource leaks if the
        // user drops the client without explicitly disconnecting.
        self._reader_task.abort();
    }
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

        // Fast path.
        match UnixStream::connect(&socket).await {
            Ok(s) => return Ok(Self::wrap(s, socket)),
            Err(e) if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
            {
                if !opts.auto_spawn {
                    return Err(e.into());
                }
            }
            Err(e) => return Err(e.into()),
        }

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
        let (read_half, write_half) = tokio::io::split(stream);
        let pending: PendingMap = Arc::new(ParkingMutex::new(HashMap::new()));
        let (notifications_tx, _) = broadcast::channel(256);
        let pending_for_task = pending.clone();
        let tx_for_task = notifications_tx.clone();
        let reader_task = tokio::spawn(async move {
            reader_loop(read_half, pending_for_task, tx_for_task).await
        });
        Client {
            writer: Mutex::new(write_half),
            next_id: AtomicI64::new(1),
            pending,
            notifications_tx,
            socket_path: socket,
            _reader_task: reader_task,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Subscribe to daemon → client notifications (`toffee.memory.promoted`,
    /// `toffee.worker.lag_changed`). Each call returns a fresh receiver.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications_tx.subscribe()
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

    pub async fn record_feedback(
        &self,
        memory_id: MemoryId,
        kind: FeedbackKind,
    ) -> Result<RecordFeedbackResponse, ClientError> {
        self.call(
            method_names::RECORD_FEEDBACK,
            RecordFeedbackRequest { memory_id, kind },
        )
        .await
    }

    pub async fn add_memory(
        &self,
        kind: MemoryKind,
        scope: Scope,
        text: String,
        subject: Option<String>,
        predicate: Option<String>,
        object: Option<String>,
        confidence: Option<f64>,
    ) -> Result<Memory, ClientError> {
        let resp: AddMemoryResponse = self
            .call(
                method_names::ADD_MEMORY,
                AddMemoryRequest {
                    kind,
                    scope,
                    text,
                    subject,
                    predicate,
                    object,
                    confidence,
                },
            )
            .await?;
        Ok(resp.memory)
    }

    pub async fn forget_memory(&self, memory_id: MemoryId) -> Result<(), ClientError> {
        let _: serde_json::Value = self
            .call(method_names::FORGET_MEMORY, ForgetMemoryRequest { memory_id })
            .await?;
        Ok(())
    }

    pub async fn list_memories(
        &self,
        req: ListMemoriesRequest,
    ) -> Result<Vec<Memory>, ClientError> {
        let resp: ListMemoriesResponse = self.call(method_names::LIST_MEMORIES, req).await?;
        Ok(resp.memories)
    }

    pub async fn get_memory(&self, memory_id: MemoryId) -> Result<Memory, ClientError> {
        let resp: GetMemoryResponse = self
            .call(method_names::GET_MEMORY, GetMemoryRequest { memory_id })
            .await?;
        Ok(resp.memory)
    }

    pub async fn list_entities(
        &self,
        req: ListEntitiesRequest,
    ) -> Result<Vec<Entity>, ClientError> {
        let resp: ListEntitiesResponse = self.call(method_names::LIST_ENTITIES, req).await?;
        Ok(resp.entities)
    }

    pub async fn get_entity_page(
        &self,
        identifier: String,
        scope: Option<Vec<String>>,
    ) -> Result<EntityPage, ClientError> {
        let resp: GetEntityPageResponse = self
            .call(
                method_names::GET_ENTITY_PAGE,
                GetEntityPageRequest { identifier, scope },
            )
            .await?;
        Ok(resp.page)
    }

    pub async fn search_memory(
        &self,
        req: SearchMemoryRequest,
    ) -> Result<Vec<SearchMemoryHit>, ClientError> {
        let resp: SearchMemoryResponse = self.call(method_names::SEARCH_MEMORY, req).await?;
        Ok(resp.hits)
    }

    /// Assemble a memory-augmented context package. This is the integrator
    /// entry point — call before LLM completion and render
    /// [`ContextPackage::render_markdown`] into your prompt.
    pub async fn read_context(
        &self,
        scope: Vec<String>,
        query: String,
        token_budget: Option<usize>,
    ) -> Result<ContextPackage, ClientError> {
        let resp: ReadContextResponse = self
            .call(
                method_names::READ_CONTEXT,
                ReadContextRequest {
                    scope,
                    query,
                    lens: "default".into(),
                    custom_lens: None,
                    token_budget,
                },
            )
            .await?;
        Ok(resp.package)
    }

    pub async fn read_context_with(
        &self,
        req: ReadContextRequest,
    ) -> Result<ContextPackage, ClientError> {
        let resp: ReadContextResponse = self.call(method_names::READ_CONTEXT, req).await?;
        Ok(resp.package)
    }

    pub async fn inspect_provenance(
        &self,
        context_package_id: ContextPackageId,
    ) -> Result<ProvenanceReport, ClientError> {
        let resp: InspectProvenanceResponse = self
            .call(
                method_names::INSPECT_PROVENANCE,
                InspectProvenanceRequest { context_package_id },
            )
            .await?;
        Ok(resp.report)
    }

    pub async fn list_conflicts(
        &self,
        include_resolved: bool,
    ) -> Result<Vec<MemoryConflict>, ClientError> {
        let resp: ListConflictsResponse = self
            .call(
                method_names::LIST_CONFLICTS,
                ListConflictsRequest { include_resolved },
            )
            .await?;
        Ok(resp.conflicts)
    }

    pub async fn get_conflict(
        &self,
        conflict_id: ConflictId,
    ) -> Result<MemoryConflict, ClientError> {
        let resp: GetConflictResponse = self
            .call(method_names::GET_CONFLICT, GetConflictRequest { conflict_id })
            .await?;
        Ok(resp.conflict)
    }

    pub async fn resolve_conflict(
        &self,
        conflict_id: ConflictId,
        action: ResolveConflictAction,
    ) -> Result<MemoryConflict, ClientError> {
        let resp: ResolveConflictResponse = self
            .call(
                method_names::RESOLVE_CONFLICT,
                ResolveConflictRequest { conflict_id, action },
            )
            .await?;
        Ok(resp.conflict)
    }

    pub async fn worker_status(&self) -> Result<WorkerStatus, ClientError> {
        let resp: WorkerStatusResponse = self
            .call(method_names::WORKER_STATUS, WorkerStatusRequest::default())
            .await?;
        Ok(resp.status)
    }

    pub async fn why_memory(
        &self,
        memory_id: MemoryId,
    ) -> Result<WhyMemoryReport, ClientError> {
        let resp: WhyMemoryResponse = self
            .call(method_names::WHY_MEMORY, WhyMemoryRequest { memory_id })
            .await?;
        Ok(resp.report)
    }

    pub async fn rebuild_indexes(&self) -> Result<usize, ClientError> {
        let resp: RebuildIndexesResponse = self
            .call(method_names::DAEMON_REBUILD_INDEXES, serde_json::Value::Null)
            .await?;
        Ok(resp.reindexed)
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

        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);

        {
            let mut w = self.writer.lock().await;
            if let Err(e) = framing::write_message(&mut *w, &request).await {
                self.pending.lock().remove(&id);
                return Err(e.into());
            }
            if let Err(e) = w.flush().await {
                self.pending.lock().remove(&id);
                return Err(e.into());
            }
        }

        let resp = rx.await.map_err(|_| ClientError::Closed)?;
        if let Some(err) = resp.error {
            return Err(err.into());
        }
        let result = resp.result.unwrap_or(serde_json::Value::Null);
        Ok(serde_json::from_value(result)?)
    }
}

async fn reader_loop(
    read_half: tokio::io::ReadHalf<UnixStream>,
    pending: PendingMap,
    notifications_tx: broadcast::Sender<Notification>,
) {
    let mut reader = BufReader::new(read_half);
    loop {
        let frame: std::io::Result<Option<serde_json::Value>> =
            framing::read_message(&mut reader).await;
        let frame = match frame {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = ?e, "client reader: frame error, closing");
                break;
            }
        };

        // Frames with a numeric / string id are responses; frames without
        // are notifications.
        if let Some(id) = numeric_id(&frame) {
            let resp: JsonRpcResponse = match serde_json::from_value(frame) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = ?e, "client reader: malformed response");
                    continue;
                }
            };
            if let Some(tx) = pending.lock().remove(&id) {
                let _ = tx.send(resp);
            } else {
                tracing::debug!(id, "client reader: no waiter for response id");
            }
            continue;
        }

        // Notification.
        match serde_json::from_value::<Notification>(frame.clone()) {
            Ok(n) => {
                let _ = notifications_tx.send(n);
            }
            Err(e) => {
                tracing::debug!(error = ?e, "client reader: unrecognised notification, ignoring");
            }
        }
    }

    // Connection closed — fail any outstanding requests.
    let mut guard = pending.lock();
    let drained: Vec<_> = guard.drain().collect();
    drop(guard);
    for (_, tx) in drained {
        // The waiter sees `Closed` via the recv error path.
        drop(tx);
    }
}

fn numeric_id(v: &serde_json::Value) -> Option<i64> {
    v.get("id").and_then(|id| id.as_i64())
}
