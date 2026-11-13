//! One error envelope for every connect endpoint: `{code, message, hint}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use h4rn_protocol::ConnectErrorBody;

/// Handler result alias.
pub type Result<T> = std::result::Result<T, AppError>;

/// An API error with a stable code and an actionable hint.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    code: String,
    message: String,
    hint: Option<String>,
}

impl AppError {
    /// Builds an error.
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            hint,
        }
    }

    /// A configuration failure, surfaced at startup.
    pub fn config(
        code: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, message, hint)
    }

    /// `400`.
    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, None)
    }

    /// `400` with a hint.
    pub fn bad_request_hint(
        code: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, Some(hint.into()))
    }

    /// `401`.
    pub fn unauthorized(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message, None)
    }

    /// `403`.
    pub fn forbidden(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message, None)
    }

    /// `404`.
    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message, None)
    }

    /// `409`.
    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message, None)
    }

    /// `429` with a retry hint.
    pub fn rate_limited(retry_after: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests",
            Some(format!("Retry after {retry_after} seconds.")),
        )
    }

    /// `502` for an upstream OIDC failure.
    pub fn upstream(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, code, message, None)
    }

    /// `500`. The real cause is logged, never returned.
    pub fn internal(context: impl Into<String>) -> Self {
        tracing::error!(context = %context.into(), "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the relay could not complete the request",
            Some("Retry once; if it persists, check the relay logs.".to_string()),
        )
    }

    /// `503` when a dependency is not ready.
    pub fn unavailable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message, None)
    }

    /// The wire body.
    pub fn body(&self) -> ConnectErrorBody {
        ConnectErrorBody::new(self.code.clone(), self.message.clone(), self.hint.clone())
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::internal(format!("database error: {error}"))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(format!("internal error: {error}"))
    }
}
