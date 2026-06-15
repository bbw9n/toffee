//! Input types for the MCP tools.
//!
//! Each struct derives [`schemars::JsonSchema`] so rmcp's `#[tool]` macro can
//! generate the JSON schema the MCP client uses to validate calls and
//! present the tool's signature to the model.

use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReadContextArgs {
    /// Scope to read against (e.g. `["project:foo", "user:me"]`). If omitted
    /// the server's `--default-scope` is used; if neither is set the call
    /// fails with `invalid_params`.
    #[serde(default)]
    pub scope: Option<Vec<String>>,

    /// The user's current request or the topic to retrieve memory for.
    pub query: String,

    /// Total token budget across all memory buckets. Daemon default is 3000.
    #[serde(default)]
    pub token_budget: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SearchMemoryArgs {
    pub query: String,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// Restrict to one memory kind: `claim`, `decision`, `preference`,
    /// `episode`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Drop hits below this cosine similarity (0.0 – 1.0). Optional.
    #[serde(default)]
    pub min_similarity: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AppendEventArgs {
    /// Scope to attribute the event to. Falls back to `--default-scope`.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// Free-form event type — e.g. `user_message`, `agent_message`,
    /// `tool_call`. The extractor keys off well-known types but accepts any.
    pub event_type: String,
    /// Arbitrary JSON payload. Convention: `{"text": "..."}` for messages.
    pub payload: Value,
    /// Who produced the event: `user` | `agent` | `tool` | `system`.
    /// Defaults to `agent` when called from an MCP client.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AddMemoryArgs {
    /// `claim` | `decision` | `preference` | `episode`.
    pub kind: String,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// The memory text — the line a human would read.
    pub text: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    /// Initial confidence in [0.0, 1.0]. Daemon picks a sensible default
    /// when omitted.
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RecordFeedbackArgs {
    pub memory_id: String,
    /// `positive` | `negative` | `correction`.
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ListMemoriesArgs {
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional case-insensitive substring filter on text.
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct GetMemoryArgs {
    pub memory_id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ForgetMemoryArgs {
    pub memory_id: String,
}
