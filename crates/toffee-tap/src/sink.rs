//! Thin wrapper around `toffee-client::Client` that the runner uses to
//! ship events. Kept as its own type so tests can inject a fake sink
//! without spawning the daemon.

use async_trait::async_trait;
use toffee_client::Client;
use toffee_core::{EventId, EventInput};

#[async_trait]
pub trait Sink: Send + Sync {
    async fn append_event(&self, input: EventInput) -> Result<EventId, SinkError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("client error: {0}")]
    Client(String),
}

pub struct ClientSink {
    client: Client,
}

impl ClientSink {
    pub fn new(client: Client) -> Self {
        ClientSink { client }
    }
}

#[async_trait]
impl Sink for ClientSink {
    async fn append_event(&self, input: EventInput) -> Result<EventId, SinkError> {
        self.client
            .append_event(input)
            .await
            .map_err(|e| SinkError::Client(e.to_string()))
    }
}

/// In-memory sink for tests.
#[cfg(any(test, feature = "test-support"))]
pub struct MockSink {
    events: parking_lot::Mutex<Vec<EventInput>>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for MockSink {
    fn default() -> Self {
        Self {
            events: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MockSink {
    pub fn captured(&self) -> Vec<EventInput> {
        self.events.lock().clone()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl Sink for MockSink {
    async fn append_event(&self, input: EventInput) -> Result<EventId, SinkError> {
        let id = EventId::generate();
        self.events.lock().push(input);
        Ok(id)
    }
}
