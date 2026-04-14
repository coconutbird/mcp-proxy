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
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> Request<'a> {
    pub fn new(id: u64, method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// An outgoing JSON-RPC 2.0 notification (no `id`).
#[derive(Debug, Serialize)]
pub struct Notification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> Notification<'a> {
    pub fn new(method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

/// An incoming JSON-RPC 2.0 response from a backend.
#[derive(Debug, Deserialize)]
pub struct Response {
    pub id: Option<u64>,
    pub result: Option<Value>,
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
