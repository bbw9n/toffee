//! JSON-RPC 2.0 wire types and server for toffee.
//!
//! Newline-delimited framing: one JSON object per line, over any
//! `tokio::io::AsyncRead + AsyncWrite` transport (the daemon uses
//! `tokio::net::UnixStream`).

pub mod framing;
pub mod methods;
pub mod server;
pub mod wire;

pub use framing::{read_message, write_message};
pub use methods::{
    AddMemoryRequest, AddMemoryResponse, AppendEventRequest, AppendEventResponse,
    ForgetMemoryRequest, GetConflictRequest, GetConflictResponse, GetEntityPageRequest,
    GetEntityPageResponse, GetMemoryRequest, GetMemoryResponse, HelloRequest, HelloResponse,
    InspectProvenanceRequest, InspectProvenanceResponse, ListConflictsRequest,
    ListConflictsResponse, ListEntitiesRequest, ListEntitiesResponse, ListMemoriesRequest,
    ListMemoriesResponse, ReadContextRequest, ReadContextResponse, RebuildIndexesResponse,
    RecordFeedbackRequest, RecordFeedbackResponse, ResolveConflictAction, ResolveConflictRequest,
    ResolveConflictResponse, SearchMemoryHit, SearchMemoryRequest, SearchMemoryResponse,
    ServerInfo, WhyMemoryRequest, WhyMemoryResponse, WorkerStatusRequest, WorkerStatusResponse,
};
pub use server::{serve, Handler, RpcError};
pub use wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};
