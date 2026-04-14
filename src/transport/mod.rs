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

pub mod headers {
    /// Session identifier header (both directions).
    pub const SESSION_ID: &str = "mcp-session-id";
    /// Base64-encoded JSON env overrides from the bridge.
    pub const ENV: &str = "x-mcp-env";
    /// Comma-separated server include list from the bridge.
    pub const SERVERS: &str = "x-mcp-servers";
}

pub mod content_types {
    pub const JSON: &str = "application/json";
    pub const SSE: &str = "text/event-stream";
}
