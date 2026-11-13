//! Errors raised by the forge layer.
//!
//! The daemon maps these onto [`h4rn_protocol::ProtoErrorCode`] at the
//! protocol boundary. Messages are written for the user; a token, an
//! `Authorization` header or a response body that could contain one never
//! appears in an error.

use h4rn_protocol::ForgeKind;

/// A forge failure.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// No account or host mapping matches the request.
    #[error("{0}")]
    NotConfigured(String),
    /// The token is missing, expired or rejected.
    #[error("the {} token was rejected: {message}", forge.display_name())]
    Unauthorized {
        /// Forge that rejected it.
        forge: ForgeKind,
        /// What the forge said, with secrets removed.
        message: String,
    },
    /// The token is valid but lacks a scope.
    #[error("the {} token lacks a required scope: {message}", forge.display_name())]
    Forbidden {
        /// Forge that rejected it.
        forge: ForgeKind,
        /// What the forge said.
        message: String,
    },
    /// The repository, PR or comment does not exist (or is not visible).
    #[error("{0}")]
    NotFound(String),
    /// The forge reports a conflicting state.
    #[error("{0}")]
    Conflict(String),
    /// The request is malformed.
    #[error("{0}")]
    InvalidRequest(String),
    /// The forge is rate limiting this account.
    #[error("the {} API is rate limited{}", forge.display_name(), retry_suffix(*retry_after_secs))]
    RateLimited {
        /// Forge that rate limited the request.
        forge: ForgeKind,
        /// Seconds until the documented reset, when known.
        retry_after_secs: Option<u64>,
        /// When the documented reset happens, when known.
        retry_at: Option<chrono::DateTime<chrono::Utc>>,
        /// The forge's message.
        message: String,
    },
    /// The forge does not support the operation.
    #[error("{0}")]
    Unsupported(String),
    /// The network request failed.
    #[error("could not reach the forge: {0}")]
    Network(String),
    /// The forge answered with an unexpected status.
    #[error("the forge returned HTTP {status}: {message}")]
    Http {
        /// HTTP status.
        status: u16,
        /// Short, redacted message.
        message: String,
    },
    /// The response shape did not match what the client expected.
    #[error("could not parse the forge response: {0}")]
    Parse(String),
    /// A local filesystem or git operation failed.
    #[error("{0}")]
    Local(String),
}

fn retry_suffix(retry_after_secs: Option<u64>) -> String {
    match retry_after_secs {
        Some(seconds) => format!("; retry in {seconds}s"),
        None => String::new(),
    }
}

/// A forge result.
pub type Result<T> = std::result::Result<T, ForgeError>;

impl ForgeError {
    /// Whether a retry later could succeed without changing the request.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Network(_))
    }
}
