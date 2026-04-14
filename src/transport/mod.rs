//! MCP transport implementations.
//!
//! - [`stdio`] — JSON-RPC over stdin/stdout (single client, direct subprocess)
//! - [`http`] — Streamable-HTTP server with session management
//! - [`bridge`] — Stdio-to-HTTP bridge for clients that only support stdio

pub mod bridge;
pub mod http;
pub mod stdio;
