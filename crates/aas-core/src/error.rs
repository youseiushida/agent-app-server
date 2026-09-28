use aas_protocol::rpc::{ErrorKind, RpcError};

/// Engine error. Protocol-level failures are carried as [`RpcError`]; everything else maps to
/// `internal`.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("event log error: {0}")]
    Log(#[from] aas_eventlog::LogError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
    /// The database was closed ([`crate::Engine::close`]): the engine has stopped for good.
    #[error("the database is closed")]
    Closed,
    #[error("{0}")]
    Internal(String),
}

pub type CoreResult<T> = Result<T, CoreError>;

impl CoreError {
    /// Stored data that cannot be decoded: reading it again gives the same error.
    pub fn is_corrupt_data(&self) -> bool {
        matches!(
            self,
            CoreError::Corrupt(_) | CoreError::Log(aas_eventlog::LogError::Corrupt { .. })
        )
    }

    /// Whether a write transaction failed because of the storage (the database, the disk, the
    /// data it holds), as opposed to a rejection of the request itself (`Rpc`) or a database
    /// that was closed on purpose. Only these failures are retried and lead to the fail-stop.
    pub fn is_storage_failure(&self) -> bool {
        match self {
            CoreError::Rpc(_) | CoreError::Closed => false,
            CoreError::Db(_)
            | CoreError::Log(_)
            | CoreError::Io(_)
            | CoreError::Corrupt(_)
            | CoreError::Internal(_) => true,
        }
    }
}

impl From<CoreError> for RpcError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Rpc(r) => r,
            other => {
                tracing::error!(error = %other, "internal error");
                RpcError::new(ErrorKind::Internal, other.to_string())
            }
        }
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Corrupt(e.to_string())
    }
}

/// Shorthand constructors.
pub fn not_found(entity: &str, id: impl std::fmt::Display) -> CoreError {
    CoreError::Rpc(RpcError::not_found(entity, id))
}

pub fn invalid_params(message: impl Into<String>) -> CoreError {
    CoreError::Rpc(RpcError::invalid_params(message))
}

pub fn invalid_state(message: impl Into<String>) -> CoreError {
    CoreError::Rpc(RpcError::invalid_state(message))
}

pub fn rpc(kind: ErrorKind, message: impl Into<String>) -> CoreError {
    CoreError::Rpc(RpcError::new(kind, message))
}
