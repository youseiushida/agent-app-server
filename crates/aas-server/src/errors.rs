//! Error bodies of the HTTP endpoints (protocol.md §6).
//!
//! Every failure is answered as `{kind, message}` — also the ones axum produces before a
//! handler runs (a body above the limit, malformed JSON, a bad path, a failed WebSocket
//! upgrade, an unknown route). Handlers take their inputs through [`Api`], which turns the
//! rejection of the wrapped extractor into that body; the router's fallbacks cover unknown
//! paths and methods.

use aas_protocol::http::HttpError;
use axum::Json;
use axum::extract::rejection::{BytesRejection, ExtensionRejection, JsonRejection, PathRejection};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

/// An error response with the protocol's body.
pub(crate) fn error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(HttpError {
            kind: kind.to_owned(),
            message: message.into(),
        }),
    )
        .into_response()
}

/// An HTTP error before it becomes a response (small enough to travel in a `Result`).
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error(self.status, self.kind, self.message)
    }
}

/// The protocol's view of an extractor rejection.
pub(crate) trait Rejection {
    fn status(&self) -> StatusCode;
    fn message(&self) -> String;
    /// `kind` for a client error other than 413 (which is always `payloadTooLarge`).
    fn client_kind(&self) -> &'static str;

    fn into_error(self) -> Response
    where
        Self: Sized,
    {
        let status = self.status();
        let kind = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "payloadTooLarge"
        } else if status.is_server_error() {
            "internal"
        } else {
            self.client_kind()
        };
        error(status, kind, self.message())
    }
}

impl Rejection for JsonRejection {
    fn status(&self) -> StatusCode {
        match self {
            JsonRejection::MissingJsonContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            // Syntax errors (400) and data errors (422) are both invalid parameters; the body
            // limit (413) and failures to read the body keep their status.
            JsonRejection::JsonSyntaxError(_) | JsonRejection::JsonDataError(_) => {
                StatusCode::BAD_REQUEST
            }
            other => other.status(),
        }
    }

    fn message(&self) -> String {
        self.body_text()
    }

    fn client_kind(&self) -> &'static str {
        "invalidParams"
    }
}

impl Rejection for BytesRejection {
    fn status(&self) -> StatusCode {
        BytesRejection::status(self)
    }

    fn message(&self) -> String {
        self.body_text()
    }

    fn client_kind(&self) -> &'static str {
        "invalidParams"
    }
}

impl Rejection for PathRejection {
    fn status(&self) -> StatusCode {
        PathRejection::status(self)
    }

    fn message(&self) -> String {
        self.body_text()
    }

    fn client_kind(&self) -> &'static str {
        "invalidParams"
    }
}

impl Rejection for WebSocketUpgradeRejection {
    fn status(&self) -> StatusCode {
        WebSocketUpgradeRejection::status(self)
    }

    fn message(&self) -> String {
        self.body_text()
    }

    fn client_kind(&self) -> &'static str {
        "invalidRequest"
    }
}

impl Rejection for ExtensionRejection {
    fn status(&self) -> StatusCode {
        ExtensionRejection::status(self)
    }

    fn message(&self) -> String {
        self.body_text()
    }

    fn client_kind(&self) -> &'static str {
        "invalidRequest"
    }
}

/// Wraps an axum extractor so that its rejection is answered with the protocol's error body.
pub(crate) struct Api<E>(pub E);

impl<S, E> FromRequestParts<S> for Api<E>
where
    S: Send + Sync,
    E: FromRequestParts<S>,
    E::Rejection: Rejection,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        E::from_request_parts(parts, state)
            .await
            .map(Api)
            .map_err(Rejection::into_error)
    }
}

impl<S, E> FromRequest<S> for Api<E>
where
    S: Send + Sync,
    E: FromRequest<S>,
    E::Rejection: Rejection,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Response> {
        E::from_request(req, state)
            .await
            .map(Api)
            .map_err(Rejection::into_error)
    }
}

/// Router fallback: no route has this path.
pub(crate) async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "notFound", "no such endpoint")
}

/// Router fallback: the path exists, the method does not.
pub(crate) async fn method_not_allowed() -> Response {
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalidRequest",
        "this endpoint does not accept that method",
    )
}
