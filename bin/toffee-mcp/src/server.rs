//! `ToffeeMcp` — the rmcp [`ServerHandler`] that fronts `toffeed`.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use toffee_client::{Client, ClientError, ConnectOptions};
use toffee_core::{FeedbackKind, MemoryId};
use toffee_rpc::{ListMemoriesRequest, SearchMemoryRequest};
use tokio::sync::Mutex;

use crate::tools::{
    AddMemoryArgs, AppendEventArgs, ForgetMemoryArgs, GetMemoryArgs, ListMemoriesArgs,
    ReadContextArgs, RecordFeedbackArgs, SearchMemoryArgs,
};
use crate::Args;

#[derive(Clone)]
pub struct ToffeeMcp {
    inner: Arc<Inner>,
    // Read by the #[tool_handler] macro expansion; dead-code lint can't see
    // that.
    #[allow(dead_code)]
    tool_router: ToolRouter<ToffeeMcp>,
}

struct Inner {
    args: Arc<Args>,
    // Held inside a Mutex so a dropped/broken client can be transparently
    // reconnected on the next call without racing other requests.
    client: Mutex<Option<Arc<Client>>>,
}

#[tool_router]
impl ToffeeMcp {
    pub fn new(args: Arc<Args>) -> Self {
        Self {
            inner: Arc::new(Inner {
                args,
                client: Mutex::new(None),
            }),
            tool_router: Self::tool_router(),
        }
    }

    /// Get a live client, reconnecting if the previous one died.
    async fn client(&self) -> Result<Arc<Client>, McpError> {
        let mut guard = self.inner.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let opts = ConnectOptions {
            socket_path: self.inner.args.socket.clone(),
            auto_spawn: !self.inner.args.no_autospawn,
            ..ConnectOptions::default()
        };
        let client = Client::connect_with(opts).await.map_err(map_client_err)?;
        let client = Arc::new(client);
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Drop the cached client; the next call will reconnect.
    async fn invalidate_client(&self) {
        self.inner.client.lock().await.take();
    }

    fn resolve_scope(&self, override_scope: Option<Vec<String>>) -> Result<Vec<String>, McpError> {
        if let Some(s) = override_scope {
            if !s.is_empty() {
                return Ok(s);
            }
        }
        if !self.inner.args.default_scope.is_empty() {
            return Ok(self.inner.args.default_scope.clone());
        }
        Err(McpError::invalid_params(
            "no scope provided and no --default-scope configured",
            None,
        ))
    }

    // -------- tools --------

    #[tool(
        description = "Assemble a memory-augmented context block for the given query. \
                       Returns markdown the agent can prepend to its prompt. \
                       This is the primary read entry point — call it before generating."
    )]
    async fn read_context(
        &self,
        Parameters(args): Parameters<ReadContextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope = self.resolve_scope(args.scope)?;
        let client = self.client().await?;
        let pkg = call_with_retry(
            self,
            |c| {
                let scope = scope.clone();
                let query = args.query.clone();
                let budget = args.token_budget;
                async move { c.read_context(scope, query, budget).await }
            },
            client,
        )
        .await?;

        let markdown = pkg.render_markdown();
        let summary = serde_json::json!({
            "context_package_id": pkg.id,
            "counts": {
                "decisions": pkg.decisions.len(),
                "preferences": pkg.preferences.len(),
                "claims": pkg.claims.len(),
                "episodes": pkg.episodes.len(),
                "conflicts": pkg.conflicts.len(),
            },
            "token_estimate": pkg.token_estimate,
        });
        Ok(CallToolResult::success(vec![
            Content::text(markdown),
            Content::text(summary.to_string()),
        ]))
    }

    #[tool(
        description = "Vector-ranked memory search. Returns the top hits as JSON with \
                       memory id, kind, text, and similarity. Prefer `read_context` for \
                       prompt assembly; use this when you want raw matches without the \
                       lens / budget / bucketing of a context package."
    )]
    async fn search_memory(
        &self,
        Parameters(args): Parameters<SearchMemoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope = match args.scope {
            Some(s) if !s.is_empty() => Some(s),
            _ if !self.inner.args.default_scope.is_empty() => {
                Some(self.inner.args.default_scope.clone())
            }
            _ => None,
        };
        let req = SearchMemoryRequest {
            query: args.query,
            scope_any_of: scope,
            kind: args.kind.and_then(parse_kind),
            limit: args.limit,
            min_similarity: args.min_similarity,
        };
        let client = self.client().await?;
        let hits = call_with_retry(
            self,
            |c| {
                let req = req.clone();
                async move { c.search_memory(req).await }
            },
            client,
        )
        .await?;

        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| McpError::internal_error(format!("serialize hits: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Append a raw event to the log (a user message, agent reply, tool call, \
                       etc.). The daemon's background worker derives typed memories from the \
                       event stream. Returns the assigned event id. Returns immediately — \
                       extraction happens asynchronously."
    )]
    async fn append_event(
        &self,
        Parameters(args): Parameters<AppendEventArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope_vec = self.resolve_scope(args.scope)?;
        let actor = args
            .actor
            .as_deref()
            .map(parse_actor)
            .transpose()?
            .unwrap_or(toffee_core::Actor::Agent);
        let input = toffee_core::EventInput {
            scope: toffee_core::Scope::new(scope_vec),
            actor,
            event_type: args.event_type,
            payload: args.payload,
            session_id: None,
            run_id: None,
        };
        let client = self.client().await?;
        let event_id = call_with_retry(
            self,
            |c| {
                let input = input.clone();
                async move { c.append_event(input).await }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::json!({ "event_id": event_id }).to_string(),
        )]))
    }

    #[tool(
        description = "Explicitly add a typed memory (claim | decision | preference | episode). \
                       Use this when you want to record a fact directly instead of letting the \
                       extractor infer it from an event."
    )]
    async fn add_memory(
        &self,
        Parameters(args): Parameters<AddMemoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope_vec = self.resolve_scope(args.scope)?;
        let kind = parse_kind(args.kind.clone()).ok_or_else(|| {
            McpError::invalid_params(format!("unknown memory kind: {}", args.kind), None)
        })?;
        let scope = toffee_core::Scope::new(scope_vec);
        let client = self.client().await?;
        let mem = call_with_retry(
            self,
            |c| {
                let scope = scope.clone();
                let text = args.text.clone();
                let subject = args.subject.clone();
                let predicate = args.predicate.clone();
                let object = args.object.clone();
                let confidence = args.confidence;
                async move {
                    c.add_memory(kind, scope, text, subject, predicate, object, confidence)
                        .await
                }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&mem)
                .map_err(|e| McpError::internal_error(format!("serialize memory: {e}"), None))?,
        )]))
    }

    #[tool(
        description = "Record feedback on a memory: positive | negative | correction. \
                       Adjusts the memory's confidence."
    )]
    async fn record_feedback(
        &self,
        Parameters(args): Parameters<RecordFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let kind = parse_feedback_kind(&args.kind)?;
        let memory_id = MemoryId(args.memory_id);
        let client = self.client().await?;
        let resp = call_with_retry(
            self,
            |c| {
                let memory_id = memory_id.clone();
                async move { c.record_feedback(memory_id, kind).await }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::json!({
                "memory_id": resp.memory_id,
                "new_confidence": resp.new_confidence,
            })
            .to_string(),
        )]))
    }

    #[tool(
        description = "List stored memories with simple filters (scope, kind, substring query, \
                       limit). For ranked retrieval use `search_memory`."
    )]
    async fn list_memories(
        &self,
        Parameters(args): Parameters<ListMemoriesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope = match args.scope {
            Some(s) if !s.is_empty() => Some(s),
            _ if !self.inner.args.default_scope.is_empty() => {
                Some(self.inner.args.default_scope.clone())
            }
            _ => None,
        };
        let req = ListMemoriesRequest {
            scope_any_of: scope,
            kind: args.kind.and_then(parse_kind),
            limit: args.limit,
            query: args.query,
        };
        let client = self.client().await?;
        let mems = call_with_retry(
            self,
            |c| {
                let req = req.clone();
                async move { c.list_memories(req).await }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&mems)
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?,
        )]))
    }

    #[tool(description = "Fetch one memory by id.")]
    async fn get_memory(
        &self,
        Parameters(args): Parameters<GetMemoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = MemoryId(args.memory_id);
        let client = self.client().await?;
        let mem = call_with_retry(
            self,
            |c| {
                let id = id.clone();
                async move { c.get_memory(id).await }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&mem)
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?,
        )]))
    }

    #[tool(description = "Soft-delete a memory. Irreversible.")]
    async fn forget_memory(
        &self,
        Parameters(args): Parameters<ForgetMemoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = MemoryId(args.memory_id);
        let client = self.client().await?;
        call_with_retry(
            self,
            |c| {
                let id = id.clone();
                async move { c.forget_memory(id).await }
            },
            client,
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }
}

#[tool_handler]
impl ServerHandler for ToffeeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Toffee is a local memory layer. Before generating, call `read_context` with the \
             user's request to fetch relevant decisions, preferences, and claims. After the \
             turn, call `append_event` to record both the user message and your reply so the \
             memory grows. Use `search_memory` for ad-hoc lookups and `add_memory` to record a \
             specific fact directly."
                    .to_string(),
            )
    }
}

// ---------------- helpers ----------------

fn map_client_err(e: ClientError) -> McpError {
    match e {
        ClientError::Rpc { code, message } => {
            McpError::internal_error(format!("toffeed rpc error ({code}): {message}"), None)
        }
        ClientError::Io(io) => McpError::internal_error(format!("toffeed io error: {io}"), None),
        ClientError::Decode(s) => McpError::internal_error(format!("toffeed decode: {s}"), None),
        ClientError::SpawnFailed(s) => {
            McpError::internal_error(format!("could not spawn toffeed: {s}"), None)
        }
        ClientError::Closed => {
            McpError::internal_error("toffeed connection closed".to_string(), None)
        }
    }
}

fn parse_kind(s: String) -> Option<toffee_core::MemoryKind> {
    toffee_core::MemoryKind::parse(&s)
}

fn parse_actor(s: &str) -> Result<toffee_core::Actor, McpError> {
    match s.to_ascii_lowercase().as_str() {
        "user" => Ok(toffee_core::Actor::User),
        "agent" | "assistant" => Ok(toffee_core::Actor::Agent),
        "tool" => Ok(toffee_core::Actor::Tool),
        "system" => Ok(toffee_core::Actor::System),
        other => Err(McpError::invalid_params(
            format!("unknown actor: {other} (expected user|agent|tool|system)"),
            None,
        )),
    }
}

fn parse_feedback_kind(s: &str) -> Result<FeedbackKind, McpError> {
    // Accept the canonical names plus a few aliases agents are likely to
    // emit. Canonical set: helpful | wrong | stale | correct.
    match s.to_ascii_lowercase().as_str() {
        "helpful" | "useful" | "good" | "positive" | "up" => Ok(FeedbackKind::Helpful),
        "wrong" | "incorrect" | "bad" | "negative" | "down" => Ok(FeedbackKind::Wrong),
        "stale" | "outdated" => Ok(FeedbackKind::Stale),
        "correct" | "confirmed" | "right" => Ok(FeedbackKind::Correct),
        other => Err(McpError::invalid_params(
            format!("unknown feedback kind: {other} (expected helpful|wrong|stale|correct)"),
            None,
        )),
    }
}

/// Call a client method, transparently reconnecting once if the cached
/// connection has gone away (daemon restart between calls is the common
/// case).
async fn call_with_retry<T, F, Fut>(
    server: &ToffeeMcp,
    mut f: F,
    mut client: Arc<Client>,
) -> Result<T, McpError>
where
    F: FnMut(Arc<Client>) -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    match f(client.clone()).await {
        Ok(v) => Ok(v),
        Err(ClientError::Closed) | Err(ClientError::Io(_)) => {
            tracing::warn!("toffeed connection lost; reconnecting once");
            server.invalidate_client().await;
            client = server.client().await?;
            f(client).await.map_err(map_client_err)
        }
        Err(e) => Err(map_client_err(e)),
    }
}
