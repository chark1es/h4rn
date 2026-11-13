//! Minimal JSON-RPC 2.0 framing for MCP.
//!
//! MCP is JSON-RPC 2.0 with a small, fixed error vocabulary. This module keeps
//! the wire handling in one place so the HTTP server, the stdio child transport,
//! the streamable-HTTP client and the legacy SSE client cannot disagree about
//! ids, notifications or error shapes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The only JSON-RPC version MCP uses.
pub const JSONRPC_VERSION: &str = "2.0";

/// Invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// The JSON was valid but not a valid request object.
pub const INVALID_REQUEST: i64 = -32600;
/// The method does not exist or is not available to this session.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The params are wrong for the method.
pub const INVALID_PARAMS: i64 = -32602;
/// An internal failure.
pub const INTERNAL_ERROR: i64 = -32603;
/// The request was cancelled with `notifications/cancelled`.
pub const REQUEST_CANCELLED: i64 = -32800;

/// One JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorObject {
    /// Numeric error class.
    pub code: i64,
    /// Human-readable message. Never contains a secret.
    pub message: String,
    /// Optional structured detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcErrorObject {
    /// Builds an error without structured data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Builds an error with structured data.
    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    /// Serialises as a JSON-RPC error response body for `id`.
    pub fn response(&self, id: Value) -> Value {
        json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "error": self,
        })
    }
}

impl std::fmt::Display for RpcErrorObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcErrorObject {}

/// One parsed incoming JSON-RPC message.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// A request: an id and a method.
    Request {
        /// Client-chosen id, echoed in the response.
        id: Value,
        /// Method name.
        method: String,
        /// Method parameters.
        params: Value,
    },
    /// A notification: a method and no id.
    Notification {
        /// Method name.
        method: String,
        /// Method parameters.
        params: Value,
    },
    /// A response to an earlier request.
    Response {
        /// Id of the request being answered.
        id: Value,
        /// Result on success.
        result: Option<Value>,
        /// Error on failure.
        error: Option<RpcErrorObject>,
    },
}

/// Why a message could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The bytes were not JSON.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// The JSON was not an object.
    #[error("a JSON-RPC message must be an object")]
    NotAnObject,
    /// A response carried neither `result` nor `error`.
    #[error("a JSON-RPC response must carry result or error")]
    EmptyResponse,
}

/// Parses one JSON value into an [`Incoming`].
pub fn parse_message(value: Value) -> Result<Incoming, ParseError> {
    let Value::Object(mut object) = value else {
        return Err(ParseError::NotAnObject);
    };
    if let Some(version) = object.get("jsonrpc")
        && version != JSONRPC_VERSION
    {
        return Err(ParseError::Json(format!(
            "unsupported jsonrpc version {version}"
        )));
    }
    let id = object.remove("id");
    if let Some(method) = object.remove("method") {
        let params = object.remove("params").unwrap_or(Value::Null);
        let Some(id) = id.filter(|id| !id.is_null()) else {
            return Ok(Incoming::Notification {
                method: method.as_str().unwrap_or_default().to_owned(),
                params,
            });
        };
        let method = method.as_str().ok_or(ParseError::NotAnObject)?.to_owned();
        return Ok(Incoming::Request { id, method, params });
    }
    let Some(id) = id else {
        return Err(ParseError::NotAnObject);
    };
    let result = object.remove("result");
    let error = object
        .remove("error")
        .and_then(|error| serde_json::from_value(error).ok());
    if result.is_none() && error.is_none() {
        return Err(ParseError::EmptyResponse);
    }
    Ok(Incoming::Response { id, result, error })
}

/// Builds a success response.
pub fn success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

/// Builds a request object.
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Builds a notification object.
pub fn notification(method: &str, params: Value) -> Value {
    let mut value = json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
    });
    if !params.is_null() {
        value["params"] = params;
    }
    value
}

/// Extracts an `i64` id for pending-request maps, treating anything else as
/// absent (MCP clients use numeric or string ids; the hub only mints integers,
/// but upstreams may answer either, so matching falls back to the exact JSON
/// value where needed).
pub fn numeric_id(id: &Value) -> Option<i64> {
    id.as_i64()
        .or_else(|| id.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_notifications_are_distinguished_by_id() {
        let request = parse_message(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        }))
        .unwrap();
        assert!(matches!(request, Incoming::Request { .. }));
        let notification = parse_message(json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .unwrap();
        assert!(matches!(notification, Incoming::Notification { .. }));
    }

    #[test]
    fn responses_need_result_or_error() {
        assert_eq!(
            parse_message(json!({"jsonrpc": "2.0", "id": 3})),
            Err(ParseError::EmptyResponse)
        );
        let response = parse_message(json!({
            "jsonrpc": "2.0", "id": 3, "result": {"tools": []}
        }))
        .unwrap();
        assert!(matches!(response, Incoming::Response { .. }));
    }

    #[test]
    fn error_responses_serialise_the_standard_shape() {
        let error = RpcErrorObject::new(REQUEST_CANCELLED, "cancelled");
        let value = error.response(json!(7));
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["error"]["code"], REQUEST_CANCELLED);
    }
}
