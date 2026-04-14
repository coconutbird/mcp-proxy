//! The Hub — thin routing layer combining the backend pool with custom tools.
//!
//! The actual backend lifecycle (spawning, reaping, hot-reload) lives in
//! [`crate::pool::BackendPool`]. This module exposes the unified JSON-RPC
//! dispatch surface used by both transports (stdio and HTTP).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{RwLock, mpsc};
use tracing::info;

use crate::backend::Tool;
use crate::config::Config;
use crate::custom_tools::CustomTools;
use crate::pool::BackendPool;

// Re-export items that external code references via `crate::server::*`.
pub use crate::pool::{BackendProgress, relevant_env_keys};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// MCP protocol version used in handshakes and initialize responses.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Session ID used for stdio transport (single implicit session).
pub const STDIO_SESSION_ID: &str = "__stdio__";

// ---------------------------------------------------------------------------
// JSON-RPC error type
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
    pub fn code(&self) -> i64 {
        use crate::jsonrpc::codes;
        match self {
            Self::MethodNotFound(_) => codes::METHOD_NOT_FOUND,
            Self::InvalidParams(_) => codes::INVALID_PARAMS,
            Self::ToolNotFound(_) => codes::TOOL_NOT_FOUND,
            Self::Internal(_) => codes::INTERNAL,
        }
    }

    pub fn to_json(&self, id: &Value) -> Value {
        crate::jsonrpc::err(id, self.code(), &self.to_string())
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Hub — thin routing layer combining BackendPool + CustomTools
// ---------------------------------------------------------------------------

/// The public aggregation layer. Combines a [`BackendPool`] for backend
/// lifecycle with [`CustomTools`] for user-defined shell/HTTP tools.
/// Exposes the unified JSON-RPC dispatch surface used by transports.
pub struct Hub {
    pool: Arc<BackendPool>,
    custom: RwLock<Arc<CustomTools>>,
}

impl Hub {
    /// Boot the hub: creates the backend pool and loads custom tools.
    pub async fn new(raw_config: Config) -> Result<Self> {
        let pool = Arc::new(BackendPool::new(&raw_config).await?);
        let custom = Arc::new(CustomTools::new(&raw_config.custom_tools));
        Ok(Self {
            pool,
            custom: RwLock::new(custom),
        })
    }

    /// List tools (backend + custom), filtered by the client's server list.
    pub async fn list_tools_for(
        self: &Arc<Self>,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> (Vec<Tool>, Vec<(String, String)>) {
        let errors = self
            .pool
            .ensure_backends(servers, env_overrides, session_id)
            .await;
        let mut tools = self
            .pool
            .backend_tools(servers, env_overrides, session_id)
            .await;
        tools.extend(self.custom.read().await.list());
        (tools, errors)
    }

    /// Streaming version of `list_tools_for`.
    pub async fn list_tools_streaming(
        self: &Arc<Self>,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
        tx: mpsc::Sender<BackendProgress>,
    ) -> (Vec<Tool>, Vec<(String, String)>) {
        let (mut tools, errors) = self
            .pool
            .backend_tools_streaming(servers, env_overrides, session_id, tx)
            .await;
        tools.extend(self.custom.read().await.list());
        (tools, errors)
    }

    /// Route a tool call to custom tools or the correct backend.
    pub async fn call_tool_for(
        &self,
        name: &str,
        args: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value, RpcError> {
        {
            let custom = self.custom.read().await;
            if custom.has(name) {
                return custom.call(name, &args).await.map_err(RpcError::from);
            }
        }
        self.pool
            .call_tool(name, args, servers, env_overrides, session_id)
            .await
    }

    /// Unified JSON-RPC method dispatch for both transports.
    pub async fn handle_request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mcp-proxy",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
            "notifications/initialized" => Ok(Value::Null),
            "tools/list" => {
                let (tools, errors) = self
                    .list_tools_for(servers, env_overrides, session_id)
                    .await;
                let mut result = serde_json::json!({ "tools": tools });
                if !errors.is_empty() {
                    let err_list: Vec<Value> = errors
                        .into_iter()
                        .map(|(name, msg)| serde_json::json!({ "server": name, "error": msg }))
                        .collect();
                    result["_errors"] = Value::Array(err_list);
                }
                Ok(result)
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::InvalidParams("missing tool name".into()))?;
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                self.call_tool_for(name, args, servers, env_overrides, session_id)
                    .await
            }
            "resources/list" | "resources/read" => Err(RpcError::MethodNotFound(
                "resources are not supported by mcp-proxy".into(),
            )),
            "prompts/list" | "prompts/get" => Err(RpcError::MethodNotFound(
                "prompts are not supported by mcp-proxy".into(),
            )),
            _ => Err(RpcError::MethodNotFound(method.to_string())),
        }
    }

    /// Health summary for /health endpoint.
    pub async fn health(&self) -> Value {
        self.pool.health().await
    }

    /// Kill all backends belonging to a specific session.
    pub async fn cleanup_session(&self, session_id: &str) {
        self.pool.cleanup_session(session_id).await;
    }

    /// Kill all backend processes and wait for them to exit.
    pub async fn shutdown(&self) {
        self.pool.shutdown().await;
    }

    /// Hot-reload: diff old vs new config, reload backends and custom tools.
    pub async fn reload(&self, new_config: Config) {
        let custom_changed = self.pool.reload_backends(&new_config).await;
        if custom_changed {
            *self.custom.write().await = Arc::new(CustomTools::new(&new_config.custom_tools));
            info!("custom tools reloaded");
        }
        info!("config reload complete");
    }
}
