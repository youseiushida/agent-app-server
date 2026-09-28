//! Line-delimited JSON over async byte streams, plus a JSON-RPC 2.0 peer.
//!
//! Every harness speaks newline-delimited JSON on the child's stdin/stdout:
//! Claude Code (`stream-json`) and pi (`--mode rpc`) use plain JSON Lines, while Codex
//! (`app-server`) and ACP agents use JSON-RPC 2.0 framed as JSON Lines. Codex omits the
//! `"jsonrpc"` member, so the peer accepts messages with or without it and can be told
//! whether to emit it.

mod lines;
mod rpc;

pub use lines::{
    JsonLinesReader, JsonLinesWriter, LineError, ReadLine, SharedJsonLinesWriter, WriteError,
};
pub use rpc::{Incoming, IncomingRequest, RpcCallError, RpcPeer, RpcPeerConfig, RpcWireError};
