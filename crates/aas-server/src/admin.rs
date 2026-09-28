//! Admin API for the local CLI (`agent-app-server pair|devices|status|stop|harness refresh`)
//! and the watchdog's liveness endpoint.
//!
//! These routes exist only on the admin listener, which [`crate::Server::serve_admin`] accepts
//! only on a loopback address. `tailscale serve` publishes the public listener (a different
//! port), so nothing relayed from the tailnet can reach these routes; `doctor` warns if
//! `tailscale serve` is pointed at the admin port. Every admin request must present the admin
//! token stored in the user's config folder (compared in constant time). The liveness
//! endpoint reveals nothing and needs no token.

use std::sync::Arc;

use aas_core::CoreError;
use aas_protocol::http::*;
use aas_protocol::methods::{HarnessListResult, HarnessRefreshParams};
use aas_protocol::{DeviceId, ErrorKind, WORKSPACE_STREAM};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::errors::{Api, ApiError, error};
use crate::http::bearer;

/// A stop requested through the admin API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopRequest {
    pub drain: bool,
}

fn check(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    match bearer(headers) {
        Some(token) if aas_core::auth::secrets_equal(token, &state.options.admin_token) => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid admin token",
        )),
    }
}

/// `GET /v1/liveness`: the liveness contract with the watchdog (design.md §18.2). Answers
/// 200 only after a round trip through the engine — this task being scheduled by the runtime
/// and a read of the database through the engine's reader pool — completes within
/// `policy.liveness_deadline`; 503 otherwise.
pub(crate) async fn liveness(State(state): State<Arc<AppState>>) -> Response {
    let deadline = state.policy.liveness_deadline;
    match tokio::time::timeout(deadline, state.engine.stream_head(WORKSPACE_STREAM)).await {
        Ok(Ok(Some(_))) => Json(Health { ok: true }).into_response(),
        Ok(Ok(None)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the workspace stream is missing",
        ),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            format!("the engine could not read its database: {e}"),
        ),
        Err(_) => {
            tracing::warn!(?deadline, "the liveness round trip missed its deadline");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                format!("the engine did not answer within {deadline:?}"),
            )
        }
    }
}

pub(crate) async fn pairing_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    let Some(url) = state.options.public_url.clone() else {
        return error(
            StatusCode::BAD_REQUEST,
            "notConfigured",
            "set server.public_url in config.toml (e.g. wss://<pc>.<tailnet>.ts.net/v1/ws) before pairing",
        );
    };
    match state.engine.create_pairing_code().await {
        Ok((code, expires_at)) => {
            let pair_url = pair_url(&url, &code, &state.engine.config().server_name);
            Json(AdminPairingCodeResponse {
                code,
                expires_at,
                pair_url,
            })
            .into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub(crate) async fn devices(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    match state.engine.list_devices(None).await {
        Ok(devices) => Json(AdminDevicesResponse { devices }).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub(crate) async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Api(Path(id)): Api<Path<String>>,
) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    match state.engine.revoke_device(&DeviceId::from(id)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "notFound", "no such device"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub(crate) async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    let e = &state.engine;
    Json(AdminStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        epoch: e.epoch().to_owned(),
        uptime_ms: e.uptime_ms(),
        listen: state.options.listen.to_string(),
        public_url: state.options.public_url.clone(),
        running_processes: e.running_processes() as u32,
        running_turns: e.running_turns() as u32,
        running_background_tasks: e.running_background_tasks() as u32,
        connected_devices: state.connections.lock().len() as u32,
        draining: e.is_draining(),
    })
    .into_response()
}

/// `POST /v1/admin/harnesses/refresh` (`agent-app-server harness refresh [id]`): probes one
/// harness (`harnessId`) or all of them again, publishes the results to the clients
/// (`harness/updated`) and answers with every harness (`{harnesses}`, as `harness/refresh`).
/// 404 for an unknown harness.
pub(crate) async fn refresh_harnesses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Api(body): Api<Option<Json<HarnessRefreshParams>>>,
) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    let id = body.and_then(|Json(p)| p.harness_id);
    match state.engine.refresh_harnesses(id.as_deref()).await {
        Ok(harnesses) => Json(HarnessListResult { harnesses }).into_response(),
        Err(CoreError::Rpc(e)) if e.kind() == Some(ErrorKind::NotFound) => {
            error(StatusCode::NOT_FOUND, "notFound", e.message)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

pub(crate) async fn stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Api(body): Api<Option<Json<AdminStopRequest>>>,
) -> Response {
    if let Err(e) = check(&state, &headers) {
        return e.into_response();
    }
    let drain = body.map(|Json(b)| b.drain).unwrap_or(false);
    if state.stop_tx.receiver_count() == 0 {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "this server does not accept stop requests",
        );
    }
    // Every request is delivered, also after an earlier one: the daemon keeps listening so
    // that a plain `stop` can end a drain that waits for a turn that never finishes.
    state.stop_tx.send_replace(Some(StopRequest { drain }));
    StatusCode::ACCEPTED.into_response()
}
