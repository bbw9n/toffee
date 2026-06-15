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
    AppendEventRequest, AppendEventResponse, HelloRequest, HelloResponse, ServerInfo,
};
pub use server::{serve, Handler, RpcError};
pub use wire::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, RequestId};
