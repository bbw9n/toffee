use serde::{Deserialize, Serialize};
use toffee_core::{EventId, EventInput};

/// `toffee.hello` — optional handshake. Clients may identify themselves and
/// learn what the server supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_version: Option<String>,
}

pub type HelloResponse = ServerInfo;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_name: String,
    pub server_version: String,
    /// Seconds the daemon has been running.
    pub uptime_seconds: u64,
    /// Method names this server understands.
    pub supported_methods: Vec<String>,
    /// Path to the SQLite database (informational).
    pub db_path: String,
    /// Number of events currently stored.
    pub event_count: i64,
}

/// `toffee.append_event` — record one event.
pub type AppendEventRequest = EventInput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEventResponse {
    pub event_id: EventId,
}

/// Method names we dispatch on. Keeping these as `&'static str` lets the
/// server side use a plain `match`.
pub mod method_names {
    pub const HELLO: &str = "toffee.hello";
    pub const APPEND_EVENT: &str = "toffee.append_event";
    pub const DAEMON_SHUTDOWN: &str = "toffee.daemon.shutdown";

    pub fn all() -> Vec<String> {
        vec![
            HELLO.to_string(),
            APPEND_EVENT.to_string(),
            DAEMON_SHUTDOWN.to_string(),
        ]
    }
}
