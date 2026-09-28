//! The agent-app-server engine.
//!
//! [`Engine`] owns projects, threads, turns, items and interactions, persists everything in
//! SQLite together with the event log, and drives agent sessions through the harness ports
//! (`aas-harness`). Transport concerns (WebSocket, subscriptions, heartbeats, auth headers)
//! live in `aas-server`.

mod actor;
pub mod auth;
mod background;
mod blobs;
mod capacity;
pub mod config;
mod db;
mod emit;
mod engine;
pub mod error;
mod fs;
mod git;
pub mod heuristics;
mod operations;
mod progress;
mod registry;
mod retention;
mod shared;
mod store;

#[cfg(test)]
mod fail_stop_tests;

pub use aas_eventlog::Batch;
pub use config::{EngineConfig, HeuristicsConfig, Policy};
pub use engine::{AuthenticatedDevice, Engine, PairError, RequestCtx};
pub use error::{CoreError, CoreResult};
pub use registry::HarnessRegistry;
pub use retention::MaintenanceReport;
pub use shared::StorageFailure;
