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
use crate::jsonrpc::RpcError;
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
        let custom = Arc::new(CustomTools::new(raw_config.custom_tools.clone()));
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
            *self.custom.write().await =
                Arc::new(CustomTools::new(new_config.custom_tools.clone()));
            info!("custom tools reloaded");
        }
        info!("config reload complete");
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end Hub dispatch tests using a python mock MCP server.

    use super::*;
    use crate::config::{ServerConfig, Sharing};

    /// Minimal MCP server: responds to initialize / tools/list / tools/call.
    const MOCK_MCP_SERVER: &str = r#"
import json, os, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    if "id" not in req:
        continue
    m = req.get("method", "")
    if m == "initialize":
        r = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0"}}
    elif m == "tools/list":
        r = {"tools":[{"name":"ping","description":"","inputSchema":{"type":"object"}}]}
    elif m == "tools/call":
        r = {"pid": os.getpid()}
    else:
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"not found"}}) + "\n")
        sys.stdout.flush()
        continue
    sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":r}) + "\n")
    sys.stdout.flush()
"#;

    fn have_python3() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn write_mock_script(label: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("mcp_hub_test_{label}_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("mock.py");
        std::fs::write(&script, MOCK_MCP_SERVER).unwrap();
        script
    }

    fn mock_config(script: &std::path::Path) -> Config {
        let srv = ServerConfig {
            command: "python3".into(),
            args: vec![script.display().to_string()],
            shared: Sharing::Session,
            ..Default::default()
        };
        let mut servers = HashMap::new();
        servers.insert("fake".to_string(), srv);
        Config {
            servers,
            ..Default::default()
        }
    }

    fn mock_config_with_idle(script: &std::path::Path, idle_secs: u64) -> Config {
        let srv = ServerConfig {
            command: "python3".into(),
            args: vec![script.display().to_string()],
            shared: Sharing::Session,
            idle_timeout_secs: Some(idle_secs),
            ..Default::default()
        };
        let mut servers = HashMap::new();
        servers.insert("fake".to_string(), srv);
        Config {
            servers,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tools_list_streaming_after_reap_respawns_for_same_session() {
        if !have_python3() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let script = write_mock_script("list_streaming_after_reap");
        let cfg = mock_config_with_idle(&script, 1);
        let hub = Arc::new(Hub::new(cfg).await.unwrap());
        let env = HashMap::new();

        // Helper: drain the streaming list_tools_for path and return tools.
        async fn list_streaming(hub: &Arc<Hub>, env: &HashMap<String, String>, sid: &str) -> usize {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<BackendProgress>(32);
            let h = hub.clone();
            let env = env.clone();
            let sid = sid.to_string();
            let handle =
                tokio::spawn(async move { h.list_tools_streaming(&[], &env, &sid, tx).await });
            // Drain progress events.
            while rx.recv().await.is_some() {}
            let (tools, _errs) = handle.await.unwrap();
            tools.len()
        }

        assert_eq!(list_streaming(&hub, &env, "sess-a").await, 1);
        assert_eq!(hub.health().await["backends"].as_array().unwrap().len(), 1);

        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        hub.pool.reap_once().await;
        assert_eq!(hub.health().await["backends"].as_array().unwrap().len(), 0);

        assert_eq!(
            list_streaming(&hub, &env, "sess-a").await,
            1,
            "streaming list on same session must respawn reaped backend"
        );

        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    /// Reproduction for the user's complaint: "session has tools, some get
    /// reaped, the reaped tools don't come back for that session but DO start
    /// for new sessions." Explores the crash path (process exits unexpectedly)
    /// rather than the idle-reap path.
    #[tokio::test]
    async fn crashed_backend_recovers_for_same_session() {
        if !have_python3() {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Mock that exits after ONE tool call. Session-scoped, max_restarts=0
        // so try_restart fails immediately, forcing the "stays in map crashed"
        // failure mode.
        let one_shot = r#"
import json, os, sys
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try: req = json.loads(line)
    except Exception: continue
    if "id" not in req: continue
    m = req.get("method", "")
    if m == "initialize":
        r = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0"}}
    elif m == "tools/list":
        r = {"tools":[{"name":"ping","description":"","inputSchema":{"type":"object"}}]}
    elif m == "tools/call":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"pid":os.getpid()}})+"\n")
        sys.stdout.flush()
        sys.exit(0)  # die after responding
    else:
        continue
    sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":r})+"\n")
    sys.stdout.flush()
"#;
        let tmp = std::env::temp_dir().join(format!("mcp_crash_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("oneshot.py");
        std::fs::write(&script, one_shot).unwrap();

        let srv = ServerConfig {
            command: "python3".into(),
            args: vec![script.display().to_string()],
            shared: Sharing::Session,
            auto_restart: true,
            max_restarts: Some(0), // restart attempts always fail
            ..Default::default()
        };
        let mut servers = HashMap::new();
        servers.insert("fake".to_string(), srv);
        let cfg = Config {
            servers,
            ..Default::default()
        };
        let hub = Arc::new(Hub::new(cfg).await.unwrap());
        let env = HashMap::new();

        // 1st call on session A: succeeds; the python process then exits.
        hub.handle_request(
            "tools/call",
            serde_json::json!({"name": "fake_ping", "arguments": {}}),
            &[],
            &env,
            "sess-a",
        )
        .await
        .expect("first call should succeed");

        // Give stdout-reader task time to mark crashed=true.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 2nd call on session A: tries auto-restart, fails (max_restarts=0).
        // Currently the dead entry stays in the map — so session A is stuck.
        let _ = hub
            .handle_request(
                "tools/call",
                serde_json::json!({"name": "fake_ping", "arguments": {}}),
                &[],
                &env,
                "sess-a",
            )
            .await;

        // Now verify the bug: a NEW session spawns a fresh backend (works),
        // but session A's next attempt should ALSO get a fresh process.
        let new_sess_result = hub
            .handle_request(
                "tools/call",
                serde_json::json!({"name": "fake_ping", "arguments": {}}),
                &[],
                &env,
                "sess-b",
            )
            .await;
        assert!(
            new_sess_result.is_ok(),
            "new session should spawn fresh backend: {new_sess_result:?}"
        );

        let same_sess_result = hub
            .handle_request(
                "tools/call",
                serde_json::json!({"name": "fake_ping", "arguments": {}}),
                &[],
                &env,
                "sess-a",
            )
            .await;
        assert!(
            same_sess_result.is_ok(),
            "ORIGINAL session should also recover with a fresh backend: {same_sess_result:?}"
        );

        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Reproduction for: "session has tools, some get reaped, the reaped tools
    /// don't come back for that session but do start for new sessions."
    #[tokio::test]
    async fn tools_list_after_reap_respawns_for_same_session() {
        if !have_python3() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let script = write_mock_script("list_after_reap");
        let cfg = mock_config_with_idle(&script, 1);
        let hub = Arc::new(Hub::new(cfg).await.unwrap());
        let env = HashMap::new();

        // Session A: first tools/list spawns the backend.
        let list = hub
            .handle_request("tools/list", Value::Null, &[], &env, "sess-a")
            .await
            .unwrap();
        assert_eq!(list["tools"].as_array().unwrap().len(), 1);
        assert_eq!(hub.health().await["backends"].as_array().unwrap().len(), 1);

        // Wait past the 1s idle timeout, then drive the reaper.
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        hub.pool.reap_once().await;
        assert_eq!(
            hub.health().await["backends"].as_array().unwrap().len(),
            0,
            "backend should have been reaped"
        );

        // Same session: tools/list MUST bring the backend back.
        let list = hub
            .handle_request("tools/list", Value::Null, &[], &env, "sess-a")
            .await
            .unwrap();
        assert_eq!(
            list["tools"].as_array().unwrap().len(),
            1,
            "same session should see tools again after reap: {list}"
        );

        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    async fn empty_hub() -> Arc<Hub> {
        Arc::new(Hub::new(Config::default()).await.unwrap())
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let hub = empty_hub().await;
        let resp = hub
            .handle_request("initialize", Value::Null, &[], &HashMap::new(), "sess")
            .await
            .unwrap();
        assert_eq!(resp["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(resp["capabilities"]["tools"].is_object());
        assert_eq!(resp["serverInfo"]["name"], "mcp-proxy");
    }

    #[tokio::test]
    async fn initialized_notification_returns_null() {
        let hub = empty_hub().await;
        let resp = hub
            .handle_request(
                "notifications/initialized",
                Value::Null,
                &[],
                &HashMap::new(),
                "sess",
            )
            .await
            .unwrap();
        assert_eq!(resp, Value::Null);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let hub = empty_hub().await;
        let err = hub
            .handle_request("nope/nope", Value::Null, &[], &HashMap::new(), "sess")
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::MethodNotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn resources_and_prompts_rejected() {
        let hub = empty_hub().await;
        for method in [
            "resources/list",
            "resources/read",
            "prompts/list",
            "prompts/get",
        ] {
            let err = hub
                .handle_request(method, Value::Null, &[], &HashMap::new(), "sess")
                .await
                .unwrap_err();
            assert!(
                matches!(err, RpcError::MethodNotFound(_)),
                "{method}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn tools_call_missing_name_is_invalid_params() {
        let hub = empty_hub().await;
        let err = hub
            .handle_request(
                "tools/call",
                serde_json::json!({}),
                &[],
                &HashMap::new(),
                "sess",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::InvalidParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_tool_not_found() {
        let hub = empty_hub().await;
        let err = hub
            .handle_request(
                "tools/call",
                serde_json::json!({"name": "ghost_do_thing"}),
                &[],
                &HashMap::new(),
                "sess",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::ToolNotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn cleanup_session_kills_only_that_session() {
        if !have_python3() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let script = write_mock_script("cleanup");
        let hub = Arc::new(Hub::new(mock_config(&script)).await.unwrap());
        let env = HashMap::new();

        // Spawn backends under two sessions via a tool call each.
        for sid in ["sess-a", "sess-b"] {
            hub.handle_request(
                "tools/call",
                serde_json::json!({"name": "fake_ping", "arguments": {}}),
                &[],
                &env,
                sid,
            )
            .await
            .unwrap();
        }

        let health = hub.health().await;
        let backends = health["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 2, "two session-scoped backends: {health}");

        // Tear down one session; the other must survive.
        hub.cleanup_session("sess-a").await;

        let health = hub.health().await;
        let backends = health["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 1, "one backend left: {health}");
        // The surviving backend should be the other session.
        assert_eq!(backends[0]["scope"], "session");

        // A follow-up call on the cleaned-up session should respawn cleanly.
        hub.handle_request(
            "tools/call",
            serde_json::json!({"name": "fake_ping", "arguments": {}}),
            &[],
            &env,
            "sess-a",
        )
        .await
        .expect("respawn after cleanup");

        let health = hub.health().await;
        assert_eq!(health["backends"].as_array().unwrap().len(), 2);

        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    #[tokio::test]
    async fn tools_list_and_call_end_to_end() {
        if !have_python3() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let script = write_mock_script("e2e");
        let hub = Arc::new(Hub::new(mock_config(&script)).await.unwrap());

        let env = HashMap::new();
        let list = hub
            .handle_request("tools/list", Value::Null, &[], &env, "sess")
            .await
            .unwrap();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "one tool expected: {list}");
        assert_eq!(tools[0]["name"], "fake_ping");

        let result = hub
            .handle_request(
                "tools/call",
                serde_json::json!({"name": "fake_ping", "arguments": {}}),
                &[],
                &env,
                "sess",
            )
            .await
            .unwrap();
        assert!(result.get("pid").is_some(), "expected pid field: {result}");

        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }
}
