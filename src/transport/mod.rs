//! MCP transport implementations.
//!
//! - [`stdio`] — JSON-RPC over stdin/stdout (single client, direct subprocess)
//! - [`http`] — Streamable-HTTP server with session management
//! - [`bridge`] — Stdio-to-HTTP bridge for clients that only support stdio

pub mod bridge;
pub mod http;
pub mod stdio;

// ---------------------------------------------------------------------------
// Shared header / content-type constants used by HTTP and bridge transports
// ---------------------------------------------------------------------------

/// Canonical HTTP header names exchanged between the bridge and the HTTP
/// transport.
pub mod headers {
    /// Session identifier header (both directions).
    pub const SESSION_ID: &str = "mcp-session-id";
    /// Base64-encoded JSON env overrides from the bridge.
    pub const ENV: &str = "x-mcp-env";
    /// Comma-separated server include list from the bridge.
    pub const SERVERS: &str = "x-mcp-servers";
}

/// HTTP `Content-Type` values used by the HTTP transport.
pub mod content_types {
    /// Ordinary JSON-RPC response.
    pub const JSON: &str = "application/json";
    /// Server-Sent Events stream (used for `tools/list` progress).
    pub const SSE: &str = "text/event-stream";
}

// ---------------------------------------------------------------------------
// Shared env-header codec (used by both HTTP server and bridge client)
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use base64::Engine;

/// Encode env overrides as a base64 JSON header value.
pub fn encode_env_header(env: &HashMap<String, String>) -> Option<String> {
    if env.is_empty() {
        return None;
    }
    let json = serde_json::to_string(env).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(json))
}

/// Decode a base64 JSON env-header value back to a map.
pub fn decode_env_header(raw: &str) -> HashMap<String, String> {
    base64::engine::general_purpose::STANDARD
        .decode(raw)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HashMap<String, String>>(&bytes).ok())
        .unwrap_or_default()
}
