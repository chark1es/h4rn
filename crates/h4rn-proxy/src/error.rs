//! Errors surfaced by the proxy integration.

use h4rn_protocol::{ProtoError, ProtoErrorCode};

/// A failure class, mapped to a protocol error at the daemon boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyErrorKind {
    /// The request was not valid.
    InvalidRequest,
    /// The profile does not exist.
    NotFound,
    /// A stored secret could not be read or written.
    Secret,
    /// The proxy could not be reached or returned an unexpected response.
    Upstream,
    /// The proxy rejected the supplied key.
    Unauthorized,
    /// The operation needs a management key that is not configured.
    ManagementUnavailable,
}

/// A proxy failure with a user-facing hint.
///
/// Messages and hints never contain key material: callers must pass only
/// descriptions, never header values.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProxyError {
    /// Failure class.
    pub kind: ProxyErrorKind,
    /// What happened.
    pub message: String,
    /// What to do next.
    pub hint: Option<String>,
}

impl ProxyError {
    /// Builds an error.
    pub fn new(kind: ProxyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
        }
    }

    /// Adds a user-facing hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Maps this failure onto the wire error type.
    pub fn into_proto(self) -> ProtoError {
        let code = match self.kind {
            ProxyErrorKind::InvalidRequest => ProtoErrorCode::InvalidRequest,
            ProxyErrorKind::NotFound => ProtoErrorCode::NotFound,
            ProxyErrorKind::Secret | ProxyErrorKind::Upstream => ProtoErrorCode::Internal,
            ProxyErrorKind::Unauthorized => ProtoErrorCode::Unauthorized,
            ProxyErrorKind::ManagementUnavailable => ProtoErrorCode::Conflict,
        };
        ProtoError {
            code,
            message: self.message,
            hint: self.hint,
        }
    }
}
