use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use toffee_core::{
    paths, Config, EventInput, ExtractionProvenance, FeedbackKind, Lens, MemoryCandidate, MemoryId,
    MemoryKind, Scope, ScoringConfig,
};
use toffee_rpc::methods::{
    method_names, AddMemoryRequest, AddMemoryResponse, AppendEventRequest, AppendEventResponse,
    ForgetMemoryRequest, GetConflictRequest, GetConflictResponse, GetEntityPageRequest,
    GetEntityPageResponse, GetMemoryRequest, GetMemoryResponse, HelloRequest, HelloResponse,
    InspectProvenanceRequest, InspectProvenanceResponse, ListConflictsRequest,
    ListConflictsResponse, ListEntitiesRequest, ListEntitiesResponse, ListMemoriesRequest,
    ListMemoriesResponse, ReadContextRequest, ReadContextResponse, RebuildIndexesResponse,
    RecordFeedbackRequest, RecordFeedbackResponse, ResolveConflictAction, ResolveConflictRequest,
    ResolveConflictResponse, SearchMemoryHit, SearchMemoryRequest, SearchMemoryResponse,
    ServerInfo, WhyMemoryRequest, WhyMemoryResponse, WorkerStatusRequest, WorkerStatusResponse,
};
use toffee_rpc::server::{serve, Handler, RpcError};
use toffee_runtime::read_path::ReadContextRequest as RuntimeReadContext;
use toffee_runtime::{ResolutionAction, Runtime, SearchParams};
use toffee_store::{EntityListFilter, MemoryListFilter, Store};
use toffee_vector::{BgeEmbedder, Embedder, HashEmbedder, VectorIndex};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum EmbedderChoice {
    /// BGE-small-en-v1.5 via candle. Lazy-downloads weights (~130MB) on
    /// first start into `$XDG_DATA_HOME/toffee/models/`.
    Bge,
    /// Feature-hashed bag-of-tokens. Offline, deterministic, lexical only.
    Hash,
}

#[derive(Parser, Debug)]
#[command(name = "toffeed", version, about = "Toffee memory daemon")]
struct Args {
    /// Run in the foreground, logging to stderr.
    #[arg(long)]
    foreground: bool,

    /// Sentinel used by clients that auto-spawn; behaves the same as default
    /// background launch (logs go to a file).
    #[arg(long)]
    detached: bool,

    /// Override the Unix socket path.
    #[arg(long, env = "TOFFEE_SOCKET")]
    socket: Option<PathBuf>,

    /// Override the database file path.
    #[arg(long, env = "TOFFEE_DB")]
    db: Option<PathBuf>,

    /// Embedding backend. `bge` (default) loads a real sentence-transformer
    /// and downloads weights on first use. `hash` keeps everything offline
    /// but is lexical-only.
    #[arg(long, value_enum, env = "TOFFEE_EMBEDDER", default_value_t = EmbedderChoice::Bge)]
    embedder: EmbedderChoice,

    /// Override the model cache directory. Defaults to
    /// `$XDG_DATA_HOME/toffee/models/`.
    #[arg(long, env = "TOFFEE_MODELS_DIR")]
    models_dir: Option<PathBuf>,

    /// Override the config file path. Defaults to
    /// `$XDG_CONFIG_HOME/toffee/config.toml`. Hot-reloaded while running.
    #[arg(long, env = "TOFFEE_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.foreground)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move { run(args).await })
}

async fn run(args: Args) -> Result<()> {
    let runtime_dir = paths::runtime_dir();
    std::fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("create runtime dir {:?}", runtime_dir))?;

    let pid_path = paths::pid_path();
    let _pid_lock =
        acquire_pid_lock(&pid_path).context("another toffeed instance appears to be running")?;

    let socket_path = args.socket.unwrap_or_else(paths::socket_path);
    // Stale socket from a previous crash — remove before binding.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).ok();
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind unix socket {:?}", socket_path))?;
    set_socket_mode(&socket_path, 0o600)?;
    tracing::info!(?socket_path, "listening");

    let db_path = args.db.unwrap_or_else(paths::db_path);
    let store =
        Arc::new(Store::open(&db_path).with_context(|| format!("open store {:?}", db_path))?);
    tracing::info!(?db_path, "store ready");

    let embedder: Arc<dyn Embedder> =
        build_embedder(args.embedder, args.models_dir).context("build embedder")?;
    tracing::info!(
        model = embedder.model(),
        dim = embedder.dim(),
        "embedder ready"
    );
    let vector = Arc::new(VectorIndex::new(embedder.dim()));
    let runtime = Runtime::with_components(store.clone(), embedder, vector);

    // Rebuild the in-memory HNSW index from the persisted embeddings table.
    // Embeddings stored under a different model id are skipped silently;
    // run `toffee daemon rebuild-indexes` after switching embedders to
    // re-embed under the new model.
    let n_indexed = runtime
        .load_index_from_store()
        .context("rehydrate vector index from store")?;
    tracing::info!(n_indexed, "vector index rehydrated");

    // Load the read-path scoring config and start watching it for live edits
    // (e.g. from `toffee-eval tune --write`).
    let config_path = args.config.unwrap_or_else(paths::config_file);
    if let Some(scoring) = load_scoring_config(&config_path) {
        runtime.set_scoring_config(scoring);
        tracing::info!(?config_path, ?scoring, "scoring config loaded");
    } else {
        tracing::info!(
            ?config_path,
            "no config file; using default scoring weights"
        );
    }

    let (shutdown_tx, _) = broadcast::channel::<()>(8);
    let handler = Arc::new(DaemonHandler {
        runtime: runtime.clone(),
        store: store.clone(),
        started_at: Instant::now(),
        shutdown: shutdown_tx.clone(),
    });

    // Spawn the worker task.
    let worker_handle = {
        let runtime = runtime.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = runtime.run_worker(shutdown_rx).await {
                tracing::error!(error = ?e, "worker task ended with error");
            }
        })
    };

    // Spawn the config watcher: poll the file's mtime and hot-swap the
    // runtime's scoring weights when it changes. Polling (vs inotify) keeps
    // the dependency footprint flat and is plenty prompt for a human- or
    // tuner-edited config.
    let config_handle = {
        let runtime = runtime.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            watch_config(runtime, config_path, shutdown_rx).await;
        })
    };

    // Spawn the server task.
    let server_handle = {
        let handler = handler.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = serve(listener, handler, shutdown_rx).await {
                tracing::error!(error = ?e, "server task ended with error");
            }
        })
    };

    wait_for_shutdown_signal(shutdown_tx.clone()).await;
    tracing::info!("shutting down");

    // Drop the worker + server tasks and clean up.
    let _ = server_handle.await;
    let _ = worker_handle.await;
    let _ = config_handle.await;
    std::fs::remove_file(&socket_path).ok();
    // pid file is unlinked when the lock guard drops.
    Ok(())
}

/// Read and parse the scoring config from `path`. Returns `None` if the file
/// is absent; logs and returns `None` on a read/parse error so a malformed
/// edit never takes the daemon down — it just keeps the previous weights.
fn load_scoring_config(path: &std::path::Path) -> Option<ScoringConfig> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(?path, error = ?e, "could not read config; keeping current weights");
            return None;
        }
    };
    match toml::from_str::<Config>(&body) {
        Ok(cfg) => Some(cfg.scoring),
        Err(e) => {
            tracing::warn!(?path, error = %e, "malformed config; keeping current weights");
            None
        }
    }
}

/// Poll `path`'s mtime every 2s and hot-swap the runtime's scoring weights when
/// it changes. Exits on shutdown.
async fn watch_config(runtime: Runtime, path: PathBuf, mut shutdown_rx: broadcast::Receiver<()>) {
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut last = mtime(&path);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tick.tick() => {
                let now = mtime(&path);
                if now != last {
                    last = now;
                    if let Some(scoring) = load_scoring_config(&path) {
                        runtime.set_scoring_config(scoring);
                        tracing::info!(?path, ?scoring, "scoring config hot-reloaded");
                    }
                }
            }
        }
    }
}

async fn wait_for_shutdown_signal(shutdown_tx: broadcast::Sender<()>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "failed to install SIGTERM handler");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "failed to install SIGINT handler");
            return;
        }
    };
    let mut shutdown_rx = shutdown_tx.subscribe();

    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
        _ = shutdown_rx.recv() => tracing::info!("received in-process shutdown"),
    }
    let _ = shutdown_tx.send(());
}

struct DaemonHandler {
    runtime: Runtime,
    store: Arc<Store>,
    started_at: Instant,
    shutdown: broadcast::Sender<()>,
}

#[async_trait]
impl Handler for DaemonHandler {
    async fn hello(&self, _req: HelloRequest) -> Result<HelloResponse, RpcError> {
        let store = self.store.clone();
        let event_count = tokio::task::spawn_blocking(move || store.event_count())
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .unwrap_or(0);
        Ok(ServerInfo {
            server_name: "toffeed".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            supported_methods: method_names::all(),
            db_path: self.store.path().display().to_string(),
            event_count,
        })
    }

    async fn append_event(&self, req: AppendEventRequest) -> Result<AppendEventResponse, RpcError> {
        let input: EventInput = req;
        let runtime = self.runtime.clone();
        let event = tokio::task::spawn_blocking(move || runtime.append_event(input))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(AppendEventResponse { event_id: event.id })
    }

    async fn record_feedback(
        &self,
        req: RecordFeedbackRequest,
    ) -> Result<RecordFeedbackResponse, RpcError> {
        let RecordFeedbackRequest { memory_id, kind } = req;
        let runtime = self.runtime.clone();
        let mid = memory_id.clone();
        let kind_owned: FeedbackKind = kind;
        let new_confidence =
            tokio::task::spawn_blocking(move || runtime.record_feedback(&mid, kind_owned))
                .await
                .map_err(|e| RpcError::Internal(e.to_string()))?
                .map_err(map_runtime_err)?;
        Ok(RecordFeedbackResponse {
            memory_id,
            new_confidence,
        })
    }

    async fn add_memory(&self, req: AddMemoryRequest) -> Result<AddMemoryResponse, RpcError> {
        let AddMemoryRequest {
            kind,
            scope,
            text,
            subject,
            predicate,
            object,
            confidence,
        } = req;

        validate_add_memory(kind, &subject, &predicate, &object)?;

        let candidate = MemoryCandidate {
            kind,
            scope,
            text,
            subject,
            predicate,
            object,
            entities: vec![],
            confidence,
            source_event_ids: vec![],
            provenance: ExtractionProvenance::Manual,
        };
        let conf = toffee_core::scalars::compute_confidence(&candidate);
        let memory = candidate_to_memory(candidate, conf);

        let runtime = self.runtime.clone();
        let memory =
            tokio::task::spawn_blocking(move || runtime.insert_memory_and_link_entities(memory))
                .await
                .map_err(|e| RpcError::Internal(e.to_string()))?
                .map_err(map_runtime_err)?;

        Ok(AddMemoryResponse { memory })
    }

    async fn forget_memory(&self, req: ForgetMemoryRequest) -> Result<(), RpcError> {
        let runtime = self.runtime.clone();
        let id = req.memory_id;
        tokio::task::spawn_blocking(move || runtime.forget_memory(&id))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(())
    }

    async fn list_memories(
        &self,
        req: ListMemoriesRequest,
    ) -> Result<ListMemoriesResponse, RpcError> {
        let filter = MemoryListFilter {
            scope_any_of: req.scope_any_of,
            kind: req.kind,
            include_deleted: false,
            include_superseded: false,
            limit: req.limit.or(Some(200)),
        };
        let q = req.query.map(|s| s.to_lowercase());
        let store = self.store.clone();
        let memories = tokio::task::spawn_blocking(move || store.list_memories(&filter))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        let filtered = if let Some(q) = q {
            memories
                .into_iter()
                .filter(|m| m.text.to_lowercase().contains(&q))
                .collect()
        } else {
            memories
        };
        Ok(ListMemoriesResponse { memories: filtered })
    }

    async fn get_memory(&self, req: GetMemoryRequest) -> Result<GetMemoryResponse, RpcError> {
        let store = self.store.clone();
        let id = req.memory_id.clone();
        let memory = tokio::task::spawn_blocking(move || store.get_memory(&id))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        let memory =
            memory.ok_or_else(|| RpcError::NotFound(format!("memory {}", req.memory_id)))?;
        Ok(GetMemoryResponse { memory })
    }

    async fn list_entities(
        &self,
        req: ListEntitiesRequest,
    ) -> Result<ListEntitiesResponse, RpcError> {
        let filter = EntityListFilter {
            entity_type: req.entity_type,
            name_prefix: req.name_prefix,
            limit: req.limit.or(Some(200)),
        };
        let store = self.store.clone();
        let entities = tokio::task::spawn_blocking(move || store.list_entities(&filter))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(|e| RpcError::Internal(e.to_string()))?;
        Ok(ListEntitiesResponse { entities })
    }

    async fn get_entity_page(
        &self,
        req: GetEntityPageRequest,
    ) -> Result<GetEntityPageResponse, RpcError> {
        let runtime = self.runtime.clone();
        let ident = req.identifier;
        let scope = req.scope;
        let page = tokio::task::spawn_blocking(move || runtime.get_entity_page(&ident, scope))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(GetEntityPageResponse { page })
    }

    async fn search_memory(
        &self,
        req: SearchMemoryRequest,
    ) -> Result<SearchMemoryResponse, RpcError> {
        let runtime = self.runtime.clone();
        let params = SearchParams {
            query: req.query,
            scope_any_of: req.scope_any_of,
            kind: req.kind,
            limit: req.limit.unwrap_or(20),
            min_similarity: req.min_similarity.unwrap_or(0.0),
        };
        let hits = tokio::task::spawn_blocking(move || runtime.search_memory(params))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(SearchMemoryResponse {
            hits: hits
                .into_iter()
                .map(|h| SearchMemoryHit {
                    memory: h.memory,
                    similarity: h.similarity,
                })
                .collect(),
        })
    }

    async fn read_context(&self, req: ReadContextRequest) -> Result<ReadContextResponse, RpcError> {
        let lens = req
            .custom_lens
            .clone()
            .unwrap_or_else(|| match req.lens.as_str() {
                "" | "default" => Lens::default_lens(),
                _ => {
                    tracing::warn!(lens = %req.lens, "unknown lens — falling back to default");
                    Lens::default_lens()
                }
            });
        let token_budget = req.token_budget.unwrap_or(3000);
        let runtime = self.runtime.clone();
        let runtime_req = RuntimeReadContext {
            scope: req.scope,
            query: req.query,
            lens,
            token_budget,
        };
        let package = tokio::task::spawn_blocking(move || runtime.read_context(runtime_req))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(ReadContextResponse { package })
    }

    async fn inspect_provenance(
        &self,
        req: InspectProvenanceRequest,
    ) -> Result<InspectProvenanceResponse, RpcError> {
        let report = self
            .runtime
            .inspect_provenance(&req.context_package_id)
            .ok_or_else(|| {
                RpcError::NotFound(format!(
                    "context package {} not in cache (it may have aged out)",
                    req.context_package_id
                ))
            })?;
        Ok(InspectProvenanceResponse { report })
    }

    async fn list_conflicts(
        &self,
        req: ListConflictsRequest,
    ) -> Result<ListConflictsResponse, RpcError> {
        let runtime = self.runtime.clone();
        let conflicts =
            tokio::task::spawn_blocking(move || runtime.list_conflicts(req.include_resolved))
                .await
                .map_err(|e| RpcError::Internal(e.to_string()))?
                .map_err(map_runtime_err)?;
        Ok(ListConflictsResponse { conflicts })
    }

    async fn get_conflict(&self, req: GetConflictRequest) -> Result<GetConflictResponse, RpcError> {
        let runtime = self.runtime.clone();
        let id = req.conflict_id.clone();
        let conflict = tokio::task::spawn_blocking(move || runtime.get_conflict(&id))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        let conflict =
            conflict.ok_or_else(|| RpcError::NotFound(format!("conflict {}", req.conflict_id)))?;
        Ok(GetConflictResponse { conflict })
    }

    async fn resolve_conflict(
        &self,
        req: ResolveConflictRequest,
    ) -> Result<ResolveConflictResponse, RpcError> {
        let action = match req.action {
            ResolveConflictAction::Pick { winner } => ResolutionAction::Pick { winner },
            ResolveConflictAction::Merge {
                text,
                subject,
                predicate,
                object,
                confidence,
            } => ResolutionAction::Merge {
                text,
                subject,
                predicate,
                object,
                confidence,
            },
            ResolveConflictAction::RejectAll => ResolutionAction::RejectAll,
        };
        let runtime = self.runtime.clone();
        let id = req.conflict_id;
        let conflict = tokio::task::spawn_blocking(move || runtime.resolve_conflict(&id, action))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(ResolveConflictResponse { conflict })
    }

    async fn daemon_rebuild_indexes(&self) -> Result<RebuildIndexesResponse, RpcError> {
        let runtime = self.runtime.clone();
        let reindexed = tokio::task::spawn_blocking(move || runtime.rebuild_indexes())
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(RebuildIndexesResponse { reindexed })
    }

    async fn worker_status(
        &self,
        _req: WorkerStatusRequest,
    ) -> Result<WorkerStatusResponse, RpcError> {
        let runtime = self.runtime.clone();
        let status = tokio::task::spawn_blocking(move || runtime.worker_status())
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(WorkerStatusResponse { status })
    }

    async fn why_memory(&self, req: WhyMemoryRequest) -> Result<WhyMemoryResponse, RpcError> {
        let runtime = self.runtime.clone();
        let id = req.memory_id;
        let report = tokio::task::spawn_blocking(move || runtime.why_memory(&id))
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?
            .map_err(map_runtime_err)?;
        Ok(WhyMemoryResponse { report })
    }

    fn notification_subscriber(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<toffee_core::Notification>> {
        Some(self.runtime.subscribe_notifications())
    }

    async fn daemon_shutdown(&self) -> Result<(), RpcError> {
        let _ = self.shutdown.send(());
        Ok(())
    }
}

fn map_runtime_err(e: toffee_runtime::RuntimeError) -> RpcError {
    match e {
        toffee_runtime::RuntimeError::NotFound(m) => RpcError::NotFound(m),
        toffee_runtime::RuntimeError::Store(s) => RpcError::Internal(s.to_string()),
        toffee_runtime::RuntimeError::Vector(v) => RpcError::Internal(v.to_string()),
        toffee_runtime::RuntimeError::InvalidInput(m) => RpcError::InvalidParams(m),
        toffee_runtime::RuntimeError::AlreadyResolved(id) => {
            RpcError::InvalidParams(format!("conflict {id} is already resolved"))
        }
    }
}

fn validate_add_memory(
    kind: MemoryKind,
    subject: &Option<String>,
    predicate: &Option<String>,
    object: &Option<String>,
) -> Result<(), RpcError> {
    if kind.requires_spo() && (subject.is_none() || predicate.is_none() || object.is_none()) {
        return Err(RpcError::InvalidParams(format!(
            "kind={} requires subject, predicate, and object",
            kind.as_str()
        )));
    }
    Ok(())
}

fn candidate_to_memory(c: MemoryCandidate, confidence: f64) -> toffee_core::Memory {
    let now = chrono::Utc::now();
    toffee_core::Memory {
        id: MemoryId::generate(),
        kind: c.kind,
        scope: c.scope,
        text: c.text,
        subject: c.subject,
        predicate: c.predicate,
        object: c.object,
        entities: c.entities,
        confidence,
        source_event_ids: c.source_event_ids,
        created_at: now,
        updated_at: now,
        superseded_by: None,
    }
}

// Quiet warnings about unused imports at the top of file.
#[allow(dead_code)]
fn _refs(_: Scope) {}

/// Owns the pid file and its flock. Drop unlinks the file.
struct PidLock {
    path: PathBuf,
    _file: File,
}

impl Drop for PidLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

fn acquire_pid_lock(pid_path: &PathBuf) -> Result<PidLock> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(pid_path)
        .with_context(|| format!("open pid file {:?}", pid_path))?;
    // Non-blocking exclusive lock. If another daemon holds it, we bail.
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("flock on pid file failed: {err}");
    }
    // Truncate then write our pid.
    let pid = std::process::id();
    // Truncate by re-opening with truncate true — easier than the unsafe ftruncate dance.
    let mut writer = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(pid_path)
        .with_context(|| format!("truncate pid file {:?}", pid_path))?;
    writeln!(writer, "{pid}")?;
    writer.sync_all().ok();
    drop(writer);
    Ok(PidLock {
        path: pid_path.clone(),
        _file: file,
    })
}

fn build_embedder(
    choice: EmbedderChoice,
    models_dir_override: Option<PathBuf>,
) -> Result<Arc<dyn Embedder>> {
    match choice {
        EmbedderChoice::Bge => {
            let cache = models_dir_override.unwrap_or_else(paths::models_dir);
            tracing::info!(
                cache = ?cache,
                "loading BGE-small-en-v1.5 (downloads ~130MB on first start)"
            );
            let bge = BgeEmbedder::new(&cache).with_context(|| {
                format!(
                    "load BGE-small embedder from {:?}. \
                     If you have no network access, restart with --embedder hash.",
                    cache
                )
            })?;
            Ok(Arc::new(bge))
        }
        EmbedderChoice::Hash => Ok(Arc::new(HashEmbedder::default())),
    }
}

fn set_socket_mode(path: &PathBuf, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {:?}", path))?;
    let mut perms = metadata.permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn init_tracing(foreground: bool) -> Result<()> {
    let filter = EnvFilter::try_from_env("TOFFEE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,toffee=debug"));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    if foreground {
        builder.with_writer(std::io::stderr).try_init().ok();
    } else {
        let log_dir = paths::log_dir();
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log dir {:?}", log_dir))?;
        let log_path = log_dir.join("toffeed.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {:?}", log_path))?;
        builder
            .with_writer(move || file.try_clone().unwrap())
            .try_init()
            .ok();
    }
    Ok(())
}
