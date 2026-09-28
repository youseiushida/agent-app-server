//! Plain HTTP endpoints: health, pairing, blobs.

use std::sync::Arc;

use aas_protocol::BlobId;
use aas_protocol::http::{Health, PairRequest};
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::errors::{Api, ApiError, error};

/// The bearer token of a request.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

pub(crate) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<aas_core::AuthenticatedDevice, ApiError> {
    let Some(token) = bearer(headers) else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing bearer token",
        ));
    };
    match state.engine.authenticate(token).await {
        Ok(Some(device)) => Ok(device),
        Ok(None) => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or revoked token",
        )),
        Err(e) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        )),
    }
}

pub(crate) async fn health() -> Json<Health> {
    Json(Health { ok: true })
}

pub(crate) async fn pair(
    State(state): State<Arc<AppState>>,
    Api(Json(req)): Api<Json<PairRequest>>,
) -> Response {
    if req.device_name.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalidParams",
            "deviceName must not be empty",
        );
    }
    match state
        .engine
        .pair(&req.code, &req.device_name, &req.platform)
        .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(aas_core::PairError::InvalidCode) => error(
            StatusCode::BAD_REQUEST,
            "invalidCode",
            "the pairing code is invalid, used or expired",
        ),
        Err(aas_core::PairError::RateLimited) => error(
            StatusCode::TOO_MANY_REQUESTS,
            "rateLimited",
            format!(
                "too many attempts; try again in {:?}",
                state.engine.policy().pairing_rate_window
            ),
        ),
        Err(aas_core::PairError::Internal(e)) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e)
        }
    }
}

/// `Cache-Control` of a blob download. A blob's id is the hash of its content, so a cached copy
/// can never be stale: one year is HTTP's conventional "forever" (the longest freshness
/// RFC 2616 §14.21 let servers announce), and `immutable` spares revalidation.
const BLOB_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// The answer to an upload above `policy.max_blob_bytes`.
fn blob_too_large(max: u64) -> Response {
    error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payloadTooLarge",
        format!("the upload is larger than maxBlobBytes ({max} bytes)"),
    )
}

/// `POST /v1/blobs`. An upload whose `Content-Length` already exceeds the limit is refused
/// before its body is read (a client sending `Expect: 100-continue` then never transfers it);
/// a body without a length is cut off by the router's body limit, whose rejection is answered
/// the same way.
pub(crate) async fn upload_blob(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Response {
    let max = state.engine.policy().max_blob_bytes;
    if let Err(e) = authenticate(&state, &headers).await {
        return e.into_response();
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max) {
        return blob_too_large(max);
    }
    let body =
        match <Api<Bytes> as axum::extract::FromRequest<()>>::from_request(request, &()).await {
            Ok(Api(body)) => body,
            Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => return blob_too_large(max),
            Err(r) => return r,
        };
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    match state.engine.put_blob(body.to_vec(), &mime).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => {
            let rpc = aas_protocol::RpcError::from(e);
            let status = match rpc.kind() {
                Some(aas_protocol::ErrorKind::PayloadTooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
                Some(aas_protocol::ErrorKind::InvalidParams) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            error(
                status,
                rpc.kind().map(|k| k.as_str()).unwrap_or("internal"),
                rpc.message,
            )
        }
    }
}

pub(crate) async fn download_blob(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Api(Path(id)): Api<Path<String>>,
) -> Response {
    if let Err(e) = authenticate(&state, &headers).await {
        return e.into_response();
    }
    let id = BlobId::from(id);
    match state.engine.blob(&id).await {
        Ok(Some((path, mime))) => match tokio::fs::File::open(&path).await {
            Ok(file) => {
                let len = file.metadata().await.map(|m| m.len()).ok();
                let stream = tokio_util::io::ReaderStream::new(file);
                let mut resp = Response::new(Body::from_stream(stream));
                let h = resp.headers_mut();
                if let Ok(v) = mime.parse() {
                    h.insert(header::CONTENT_TYPE, v);
                }
                if let Some(len) = len {
                    h.insert(header::CONTENT_LENGTH, len.into());
                }
                // Blobs are content-addressed: safe to cache forever.
                h.insert(
                    header::CACHE_CONTROL,
                    BLOB_CACHE_CONTROL.parse().expect("static header"),
                );
                resp
            }
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
        },
        Ok(None) => error(StatusCode::NOT_FOUND, "notFound", "no such blob"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
