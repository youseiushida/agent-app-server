//! Wire protocol v1 of agent-app-server.
//!
//! This crate is the single source of truth for every message exchanged between the daemon
//! and its clients. `docs/protocol.md` describes it in prose; `fixtures/protocol/*.json`
//! contains one golden example per message type, generated from [`examples`] and verified
//! by this crate's tests (the Android client parses the same files in its unit tests).

pub mod events;
pub mod examples;
pub mod http;
pub mod ids;
pub mod methods;
pub mod notifications;
pub mod rpc;
pub mod types;

pub use events::{Event, EventEnvelope};
pub use ids::*;
pub use methods::{ClientRequest, METHOD_NAMES};
pub use notifications::ServerNotification;
pub use rpc::{ErrorKind, RequestId, RpcError, RpcMessage};
pub use types::*;

/// The only protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the workspace-wide stream.
pub const WORKSPACE_STREAM: &str = "workspace";

/// Builds the stream name of a thread (`thread:<threadId>`).
pub fn thread_stream(thread_id: &ThreadId) -> String {
    format!("thread:{}", thread_id.as_str())
}

/// Parses a stream name into its kind.
pub fn parse_stream(name: &str) -> Option<StreamRef> {
    if name == WORKSPACE_STREAM {
        return Some(StreamRef::Workspace);
    }
    name.strip_prefix("thread:")
        .filter(|id| id.starts_with(ThreadId::PREFIX))
        .map(|id| StreamRef::Thread(ThreadId::from(id.to_owned())))
}

/// A parsed stream name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamRef {
    Workspace,
    Thread(ThreadId),
}

impl StreamRef {
    pub fn name(&self) -> String {
        match self {
            StreamRef::Workspace => WORKSPACE_STREAM.to_owned(),
            StreamRef::Thread(id) => thread_stream(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_names_round_trip() {
        let id = ThreadId::generate();
        let name = thread_stream(&id);
        assert_eq!(parse_stream(&name), Some(StreamRef::Thread(id.clone())));
        assert_eq!(StreamRef::Thread(id).name(), name);
        assert_eq!(parse_stream("workspace"), Some(StreamRef::Workspace));
        assert_eq!(parse_stream("thread:prj_1"), None);
        assert_eq!(parse_stream("bogus"), None);
    }
}
