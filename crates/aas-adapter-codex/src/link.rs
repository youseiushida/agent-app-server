//! Abstraction over the supervised process so the protocol core can run over in-memory pipes
//! in tests.

use std::time::Duration;

use aas_supervisor::{ChildHandle, ExitInfo, StopReason};
use async_trait::async_trait;

/// Lifetime control of the process behind a session.
#[async_trait]
pub trait ProcessLink: Send + Sync + 'static {
    /// Resolves when the process tree has ended.
    async fn wait(&self) -> ExitInfo;
    /// Waits up to `grace` for a natural exit, then terminates the tree.
    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo;
    /// Terminates the tree now.
    fn kill(&self, reason: StopReason);
}

#[async_trait]
impl ProcessLink for ChildHandle {
    async fn wait(&self) -> ExitInfo {
        ChildHandle::wait(self).await
    }

    async fn shutdown(&self, grace: Duration, reason: StopReason) -> ExitInfo {
        ChildHandle::shutdown(self, grace, reason).await
    }

    fn kill(&self, reason: StopReason) {
        ChildHandle::kill(self, reason)
    }
}
