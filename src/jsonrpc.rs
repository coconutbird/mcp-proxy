//! Shared JSON-RPC 2.0 types and helpers.
//!
//! Provides the canonical request/response/error structures used across
//! backend communication, the Hub dispatch layer, and both transports.
//! Centralises what was previously ad-hoc `serde_json::json!` construction
//! scattered across multiple modules.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Wire types (serialized over stdin/stdout or HTTP)
// ---------------------------------------------------------------------------

/// An outgoing JSON-RPC 2.0 request (sent to backends).
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Request id; response must echo it back.
    pub id: u64,
    /// Method name (e.g. `"tools/list"`, `"tools/call"`).
    pub method: &'a str,
    /// Method parameters (JSON object or array).
    pub params: Value,
}

impl<'a> Request<'a> {
    /// Construct a request with `jsonrpc: "2.0"` pre-filled.
    pub fn new(id: u64, method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// An outgoing JSON-RPC 2.0 notification (no `id`, no response expected).
#[derive(Debug, Serialize)]
pub struct Notification<'a> {
    /// Protocol version — always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Method name (e.g. `"notifications/initialized"`).
    pub method: &'a str,
    /// Method parameters.
    pub params: Value,
}

impl<'a> Notification<'a> {
    /// Construct a notification with `jsonrpc: "2.0"` pre-filled.
    pub fn new(method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

/// An incoming JSON-RPC 2.0 response from a backend. Exactly one of
/// `result` or `error` should be set.
#[derive(Debug, Deserialize)]
pub struct Response {
    /// Echoes the request's id (missing only on malformed backends).
    pub id: Option<u64>,
    /// Success payload.
    pub result: Option<Value>,
    /// Error payload.
    pub error: Option<WireError>,
}

/// The `error` object inside a JSON-RPC 2.0 wire response.
///
/// Named `WireError` (not `RpcError`) to avoid confusion with
/// [`crate::server::RpcError`], which is the domain-level error enum.
#[derive(Debug, Deserialize)]
pub struct WireError {
    #[allow(dead_code)]
    pub code: i64,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Response builders (produce `serde_json::Value` for transport layers)
// ---------------------------------------------------------------------------

/// Build a JSON-RPC 2.0 success response.
pub fn ok(id: &Value, result: Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC 2.0 error response.
pub fn err(id: &Value, code: i64, message: &str) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

// ---------------------------------------------------------------------------
// Standard error codes
// ---------------------------------------------------------------------------

/// Standard and server-defined JSON-RPC error codes.
pub mod codes {
    /// Method not found.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid params.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Tool not found (server-defined).
    pub const TOOL_NOT_FOUND: i64 = -32002;
    /// Generic internal/backend error.
    pub const INTERNAL: i64 = -32000;
}

// ---------------------------------------------------------------------------
// Domain-level RPC error (used by Hub dispatch + pool)
// ---------------------------------------------------------------------------

/// Structured error type for JSON-RPC responses.
///
/// Maps to standard JSON-RPC 2.0 error codes so HTTP and stdio transports
/// can return proper error objects instead of generic -32000.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// -32601: Method not found.
    #[error("unsupported method: {0}")]
    MethodNotFound(String),
    /// -32602: Invalid params (e.g. missing tool name).
    #[error("{0}")]
    InvalidParams(String),
    /// -32002: Tool not found (server-defined error).
    #[error("unknown tool: {0}")]
    ToolNotFound(String),
    /// -32000: Generic backend / internal error.
    #[error("{0}")]
    Internal(String),
}

impl RpcError {
    /// The JSON-RPC error code this variant maps to.
    pub fn code(&self) -> i64 {
        match self {
            Self::MethodNotFound(_) => codes::METHOD_NOT_FOUND,
            Self::InvalidParams(_) => codes::INVALID_PARAMS,
            Self::ToolNotFound(_) => codes::TOOL_NOT_FOUND,
            Self::Internal(_) => codes::INTERNAL,
        }
    }

    /// Serialize as a JSON-RPC 2.0 error response bound to `id`.
    pub fn to_json(&self, id: &Value) -> Value {
        err(id, self.code(), &self.to_string())
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_has_jsonrpc_2_0() {
        let req = Request::new(42, "tools/list", json!({}));
        let s = serde_json::to_string(&req).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["method"], "tools/list");
    }

    #[test]
    fn notification_has_no_id() {
        let n = Notification::new("notifications/initialized", json!({}));
        let v: Value = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/initialized");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn response_deserializes_success() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, Some(1));
        assert_eq!(resp.result.unwrap()["ok"], true);
        assert!(resp.error.is_none());
    }

    #[test]
    fn response_deserializes_error() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, Some(7));
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "nope");
    }

    #[test]
    fn ok_builder() {
        let v = ok(&json!(1), json!({"x": 2}));
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["x"], 2);
    }

    #[test]
    fn err_builder() {
        let v = err(&json!("abc"), -32601, "missing");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "missing");
    }

    #[test]
    fn rpc_error_codes() {
        assert_eq!(
            RpcError::MethodNotFound("x".into()).code(),
            codes::METHOD_NOT_FOUND
        );
        assert_eq!(
            RpcError::InvalidParams("x".into()).code(),
            codes::INVALID_PARAMS
        );
        assert_eq!(
            RpcError::ToolNotFound("x".into()).code(),
            codes::TOOL_NOT_FOUND
        );
        assert_eq!(RpcError::Internal("x".into()).code(), codes::INTERNAL);
    }

    #[test]
    fn rpc_error_to_json_maps_code_and_message() {
        let v = RpcError::ToolNotFound("ghost".into()).to_json(&json!(5));
        assert_eq!(v["id"], 5);
        assert_eq!(v["error"]["code"], codes::TOOL_NOT_FOUND);
        assert_eq!(v["error"]["message"], "unknown tool: ghost");
    }

    #[test]
    fn rpc_error_from_anyhow() {
        let e: RpcError = anyhow::anyhow!("boom").into();
        assert!(matches!(e, RpcError::Internal(ref m) if m == "boom"));
    }
}
