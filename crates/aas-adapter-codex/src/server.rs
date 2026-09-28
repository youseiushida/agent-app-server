//! Spawning `codex app-server` and its `initialize` handshake.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use aas_harness::{AdapterContext, AdapterError, HarnessConfig, StartGuard};
use aas_stdio::{Incoming, RpcPeer, RpcPeerConfig, RpcWireError};
use aas_supervisor::{ChildHandle, SpawnSpec, StopReason};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::rpc_err;

pub(crate) struct Spawned {
    pub peer: RpcPeer,
    pub incoming: mpsc::UnboundedReceiver<Incoming>,
    pub handle: ChildHandle,
}

/// Spawns `<command> <args…> app-server` through the supervisor.
pub(crate) async fn spawn_app_server(
    config: &HarnessConfig,
    ctx: &AdapterContext,
    program: &Path,
    cwd: &Path,
    label: String,
    owner: Option<String>,
) -> Result<Spawned, AdapterError> {
    let mut spec = SpawnSpec::new(label.clone(), program, cwd)
        .args(config.args.iter())
        .arg("app-server");
    for (key, value) in &config.env {
        spec = spec.env(key, value);
    }
    if let Some(owner) = owner {
        spec = spec.owner(owner);
    }
    let mut child = ctx
        .supervisor
        .spawn(spec)
        .await
        .map_err(|e| AdapterError::Spawn(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        child.handle.kill(StopReason::Abandoned);
        return Err(AdapterError::Spawn(
            "codex app-server started without stdio pipes".into(),
        ));
    };
    let (peer, incoming) = RpcPeer::start(
        stdout,
        stdin,
        RpcPeerConfig {
            emit_jsonrpc_field: false,
            max_line_bytes: ctx.policy.max_line_bytes,
            label,
        },
    );
    Ok(Spawned {
        peer,
        incoming,
        handle: child.handle,
    })
}

/// `initialize` request followed by the `initialized` notification.
pub(crate) async fn initialize(peer: &RpcPeer, timeout: Duration) -> Result<(), AdapterError> {
    let params = json!({
        "clientInfo": {
            "name": "agent-app-server",
            "title": "agent-app-server",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": { "experimentalApi": false, "requestAttestation": false },
    });
    peer.request_timeout::<_, Value>("initialize", params, timeout)
        .await
        .map_err(|e| rpc_err("initialize", e))?;
    peer.notify("initialized", json!({}))
        .await
        .map_err(|e| rpc_err("initialized", e))
}

/// Adds the child's stderr tail to a start failure.
pub(crate) fn with_stderr(error: AdapterError, handle: &ChildHandle) -> AdapterError {
    let tail = handle.stderr_tail();
    let tail = tail.trim();
    if tail.is_empty() {
        return error;
    }
    let message = format!("{error}; stderr: {tail}");
    match error {
        AdapterError::Unavailable(_) => AdapterError::Unavailable(message),
        AdapterError::Protocol(_) => AdapterError::Protocol(message),
        AdapterError::Harness(_) => AdapterError::Harness(message),
        _ => AdapterError::Spawn(message),
    }
}

/// Runs `f` against a temporary app-server (for probes and listings) and stops it afterwards.
pub(crate) async fn with_short_lived<T, F, Fut>(
    config: &HarnessConfig,
    ctx: &AdapterContext,
    program: &Path,
    f: F,
) -> Result<T, AdapterError>
where
    F: FnOnce(RpcPeer) -> Fut,
    Fut: Future<Output = Result<T, AdapterError>>,
{
    std::fs::create_dir_all(&ctx.state_dir)
        .map_err(|e| AdapterError::Spawn(format!("{}: {e}", ctx.state_dir.display())))?;
    let Spawned {
        peer,
        mut incoming,
        handle,
    } = spawn_app_server(
        config,
        ctx,
        program,
        &ctx.state_dir,
        format!("{}[short-lived]", config.id),
        None,
    )
    .await?;
    // Also stops the process with the staged stop when the caller drops this future (a
    // cancelled probe or listing request).
    let guard = start_guard(&peer, &handle, ctx.policy.stop_grace);
    // Nothing is expected from a short-lived server; refuse requests so it never blocks.
    let drain_peer = peer.clone();
    tokio::spawn(async move {
        while let Some(message) = incoming.recv().await {
            if let Incoming::Request(req) = message {
                let _ = drain_peer
                    .respond_error(req.id, RpcWireError::method_not_found(&req.method))
                    .await;
            }
        }
    });
    let result = async {
        initialize(&peer, ctx.policy.handshake_timeout).await?;
        f(peer.clone()).await
    }
    .await;
    guard.stop(StopReason::Shutdown).await;
    result.map_err(|e| with_stderr(e, &handle))
}

/// Guards a freshly spawned app-server until its session is handed over: the staged stop of
/// design §4.3 (close stdin, wait `grace`, terminate the tree), run in its own task when the
/// guard is dropped while armed.
pub(crate) fn start_guard(peer: &RpcPeer, handle: &ChildHandle, grace: Duration) -> StartGuard {
    let (peer, handle) = (peer.clone(), handle.clone());
    StartGuard::new(move |reason| async move {
        peer.close_writer().await;
        handle.shutdown(grace, reason).await
    })
}
