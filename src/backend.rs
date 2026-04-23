//! MCP backend subprocess management.
//!
//! A [`Backend`] represents a single running MCP server process. It handles:
//! - Process spawning (local or Docker)
//! - JSON-RPC handshake and tool discovery
//! - Request routing with timeouts
//! - Crash detection and auto-restart with exponential backoff
//! - Tool filtering (include/exclude globs) and aliasing

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, oneshot};
use tracing::{debug, info, warn};

use crate::config::{ServerConfig, expand_env_with_overrides, resolve_env};
use crate::jsonrpc;

/// Default RPC timeout if not configured per-server.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A tool discovered from a backend, namespaced as `<backend>_<tool>`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    /// Namespaced name exposed to MCP clients (e.g. `github_search_issues`).
    pub name: String,
    /// Human-readable description forwarded from the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments, as reported by the backend.
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// The original tool name before namespacing (used for filtering/aliases).
    #[serde(skip)]
    pub original_name: String,
}

/// Simple glob matcher supporting `*` wildcards.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == parts.len() - 1 {
            if !rest.ends_with(part) {
                return false;
            }
            return true;
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value>>>;

/// A running MCP server subprocess managed by the hub.
///
/// Owns the child process's stdin and routes JSON-RPC responses from stdout
/// to waiting futures via `pending`. Crash detection fires when stdout
/// closes; [`Backend::call_tool`] auto-restarts on the next call if
/// `auto_restart` is enabled in the server config.
pub struct Backend {
    /// Server name from config (used for log tagging and tool namespacing).
    pub name: String,
    /// Tools discovered during handshake, post-filtering and aliasing.
    pub tools: Vec<Tool>,
    /// `true` once the MCP `initialize` handshake has completed successfully.
    pub ready: bool,
    /// Number of times this backend has been restarted after a crash.
    pub restart_count: u32,
    /// Whether the process has exited unexpectedly.
    crashed: Arc<AtomicBool>,
    /// Notified when the process exits so callers can trigger restart.
    exit_notify: Arc<Notify>,
    seq: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    stdin: Option<tokio::process::ChildStdin>,
    timeout: Duration,
    child: Option<Child>,
    /// Stored so we can restart with the same params.
    cfg_snapshot: ServerConfig,
    extra_env_snapshot: HashMap<String, String>,
}

impl Backend {
    /// Spawn the server process, complete the MCP handshake, discover tools.
    /// `extra_env` are per-client overrides that take precedence over config env.
    ///
    /// If the command isn't found locally and the server has an `install` config,
    /// we automatically build a Docker image and run the backend in a container.
    pub async fn start(
        name: String,
        cfg: &ServerConfig,
        extra_env: &HashMap<String, String>,
    ) -> Result<Self> {
        let timeout_secs = cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let crashed = Arc::new(AtomicBool::new(false));
        let exit_notify = Arc::new(Notify::new());

        let (pending, stdin, child) =
            Self::spawn_process(&name, cfg, extra_env, &crashed, &exit_notify).await?;

        let mut be = Self {
            name,
            tools: Vec::new(),
            ready: false,
            restart_count: 0,
            crashed,
            exit_notify,
            seq: AtomicU64::new(1),
            pending,
            stdin: Some(stdin),
            timeout: Duration::from_secs(timeout_secs),
            child: Some(child),
            cfg_snapshot: cfg.clone(),
            extra_env_snapshot: extra_env.clone(),
        };
        be.handshake().await?;
        be.apply_tool_filters();
        Ok(be)
    }

    /// Spawn the child process and wire up stdin/stdout/stderr.
    async fn spawn_process(
        name: &str,
        cfg: &ServerConfig,
        extra_env: &HashMap<String, String>,
        crashed: &Arc<AtomicBool>,
        exit_notify: &Arc<Notify>,
    ) -> Result<(Arc<Mutex<Pending>>, tokio::process::ChildStdin, Child)> {
        let expand = |s: &str| expand_env_with_overrides(s, extra_env);

        let args: Vec<String> = cfg.args.iter().map(|a| expand(a)).collect();
        let env = resolve_env(&cfg.env, extra_env);

        let use_docker =
            cfg.install.is_some() && matches!(cfg.runtime, crate::config::Runtime::Docker);

        let mut child = if use_docker {
            info!("[{name}] using Docker");
            crate::docker::ensure_image(name, cfg).await?;
            crate::docker::run_container(name, cfg, extra_env).await?
        } else {
            Command::new(&cfg.command)
                .args(&args)
                .envs(&env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to spawn '{name}': command '{}' — {e}\n\
                         hint: is '{}' installed and on your PATH?",
                        cfg.command,
                        cfg.command
                    )
                })?
        };

        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("[{name}] failed to capture stdin"))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("[{name}] failed to capture stdout"))?;
        let child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("[{name}] failed to capture stderr"))?;

        let pending: Arc<Mutex<Pending>> = Arc::default();

        // Drain stderr to the hub stderr
        let tag = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(child_stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(server = %tag, "{line}");
            }
        });

        // Route stdout JSON-RPC responses to pending futures.
        // When stdout closes (process exit), mark as crashed and notify.
        let p = pending.clone();
        let tag = name.to_string();
        let crash_flag = crashed.clone();
        let notify = exit_notify.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(child_stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut resp) = serde_json::from_str::<jsonrpc::Response>(&line) else {
                    warn!("[{tag}] non-json stdout: {line}");
                    continue;
                };
                let Some(id) = resp.id else { continue };
                if let Some(tx) = p.lock().await.remove(&id) {
                    let val = match resp.error.take() {
                        Some(e) => Err(anyhow::anyhow!("{}", e.message)),
                        None => Ok(resp.result.take().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(val);
                }
            }
            // stdout closed → process exited — fail all in-flight RPCs immediately
            crash_flag.store(true, Ordering::Release);
            let mut pending = p.lock().await;
            for (_id, tx) in pending.drain() {
                let _ = tx.send(Err(anyhow::anyhow!("[{tag}] backend process exited")));
            }
            drop(pending);
            notify.notify_waiters();
            warn!("[{tag}] process exited (stdout closed)");
        });

        Ok((pending, child_stdin, child))
    }

    /// Apply include/exclude tool filters and aliases from the config.
    fn apply_tool_filters(&mut self) {
        apply_tool_filters(&mut self.tools, &self.name, &self.cfg_snapshot);
    }
}

/// Apply include/exclude filters and aliases to a backend's tool list.
///
/// - `include_tools`: if non-empty, only tools whose original name matches
///   at least one glob are kept.
/// - `exclude_tools`: tools whose original name matches any glob are dropped
///   (applied after include).
/// - `tool_aliases`: for each `{alias} -> {original}` pair, adds a cloned
///   tool named `<server>_<alias>` alongside the original.
fn apply_tool_filters(tools: &mut Vec<Tool>, server_name: &str, cfg: &ServerConfig) {
    if !cfg.include_tools.is_empty() {
        tools.retain(|t| {
            cfg.include_tools
                .iter()
                .any(|pat| glob_match(pat, &t.original_name))
        });
    }

    if !cfg.exclude_tools.is_empty() {
        tools.retain(|t| {
            !cfg.exclude_tools
                .iter()
                .any(|pat| glob_match(pat, &t.original_name))
        });
    }

    let mut aliased: Vec<Tool> = Vec::new();
    for (alias, original) in &cfg.tool_aliases {
        if let Some(t) = tools.iter().find(|t| t.original_name == *original) {
            let mut cloned = t.clone();
            cloned.name = format!("{server_name}_{alias}");
            aliased.push(cloned);
        }
    }
    tools.extend(aliased);
}

impl Backend {
    /// Returns true if the backend process has crashed.
    pub fn has_crashed(&self) -> bool {
        self.crashed.load(Ordering::Acquire)
    }

    /// Clone of the shared crash flag for lock-free observation.
    pub fn crashed_flag(&self) -> Arc<AtomicBool> {
        self.crashed.clone()
    }

    /// Attempt to restart the backend after a crash.
    /// Returns an error if max restarts exceeded or restart fails.
    pub async fn try_restart(&mut self) -> Result<()> {
        let max = self.cfg_snapshot.max_restarts.unwrap_or(5);
        if self.restart_count >= max {
            bail!("[{}] max restarts ({max}) exceeded, giving up", self.name);
        }

        // Exponential backoff: 1s, 2s, 4s, 8s, 16s (capped)
        let delay = Duration::from_secs(1 << self.restart_count.min(4));
        warn!(
            "[{}] restarting in {:?} (attempt {}/{})",
            self.name,
            delay,
            self.restart_count + 1,
            max
        );
        tokio::time::sleep(delay).await;

        // Kill old process if still around
        self.kill();

        // Reset crash state
        self.crashed.store(false, Ordering::Release);
        self.ready = false;
        self.restart_count += 1;

        let (pending, stdin, child) = Self::spawn_process(
            &self.name,
            &self.cfg_snapshot,
            &self.extra_env_snapshot,
            &self.crashed,
            &self.exit_notify,
        )
        .await?;

        self.pending = pending;
        self.stdin = Some(stdin);
        self.child = Some(child);

        self.handshake().await?;
        self.apply_tool_filters();
        info!(
            "[{}] restarted successfully (attempt {})",
            self.name, self.restart_count
        );
        Ok(())
    }

    async fn handshake(&mut self) -> Result<()> {
        self.rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": crate::server::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-proxy", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "[{}] MCP handshake failed: {e}\nhint: is '{}' a valid MCP server?",
                self.name,
                self.cfg_snapshot.command
            )
        })?;

        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;

        match self.rpc("tools/list", serde_json::json!({})).await {
            Ok(val) => {
                let items = val.get("tools").and_then(Value::as_array);
                self.tools = items
                    .into_iter()
                    .flatten()
                    .filter_map(|t| {
                        let raw = t.get("name")?.as_str()?;
                        Some(Tool {
                            name: format!("{}_{raw}", self.name),
                            description: t
                                .get("description")
                                .and_then(Value::as_str)
                                .map(Into::into),
                            input_schema: t.get("inputSchema").cloned(),
                            original_name: raw.into(),
                        })
                    })
                    .collect();
            }
            Err(e) => warn!("[{}] tools/list failed: {e}", self.name),
        }

        self.ready = true;
        info!("[{}] ready with {} tool(s)", self.name, self.tools.len());
        Ok(())
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        // Check if process has crashed before trying
        if self.crashed.load(Ordering::Acquire) {
            bail!(
                "[{}] backend process has crashed — restart pending",
                self.name
            );
        }

        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let msg = serde_json::to_string(&jsonrpc::Request::new(id, method, params))?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        // Write to stdin — if this fails, clean up the pending entry so the
        // receiver doesn't hang until timeout or backend exit.
        let write_result = match self.stdin.as_mut() {
            Some(w) => crate::util::write_line(w, msg.as_bytes()).await,
            None => Err(anyhow::anyhow!(
                "[{}] stdin closed — backend may have crashed",
                self.name
            )),
        };
        if let Err(e) = write_result {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => bail!(
                "[{}] response channel dropped — backend may have crashed",
                self.name
            ),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!(
                    "[{}] {method} timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                )
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = serde_json::to_string(&jsonrpc::Notification::new(method, params))?;
        if let Some(w) = self.stdin.as_mut() {
            crate::util::write_line(w, msg.as_bytes()).await?;
        }
        Ok(())
    }

    /// Forward a tool call to this backend using the original tool name.
    /// If the backend has crashed and auto_restart is enabled, attempt restart first.
    pub async fn call_tool(&mut self, original_name: &str, args: Value) -> Result<Value> {
        // Auto-restart on crash if configured
        if self.has_crashed() && self.cfg_snapshot.auto_restart {
            info!(
                "[{}] backend crashed, attempting auto-restart before tool call",
                self.name
            );
            self.try_restart().await?;
        }

        self.rpc(
            "tools/call",
            serde_json::json!({
                "name": original_name, "arguments": args,
            }),
        )
        .await
    }

    /// Kill the child process (fire-and-forget).
    ///
    /// Closes stdin first so well-behaved backends can exit on their own,
    /// then sends SIGKILL as a fallback.
    pub fn kill(&mut self) {
        // Drop stdin so the backend sees EOF and can shut down gracefully.
        self.stdin.take();
        if let Some(ref mut c) = self.child {
            let _ = c.start_kill();
        }
    }

    /// Kill the child process and wait for it to fully exit.
    ///
    /// Closes stdin first, gives the backend a brief window to exit
    /// gracefully, then sends SIGKILL and waits for termination.
    pub async fn kill_and_wait(&mut self) {
        // Close stdin so the backend sees EOF.
        self.stdin.take();
        if let Some(ref mut c) = self.child {
            // Give the backend a moment to exit on its own.
            match tokio::time::timeout(Duration::from_millis(500), c.wait()).await {
                Ok(_) => (),
                Err(_) => {
                    let _ = c.start_kill();
                    let _ = c.wait().await;
                }
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Ensure the child process is killed when the Backend is dropped.
        // This prevents orphaned processes if a Backend is dropped without
        // an explicit kill() call.
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("read_file", "read_file"));
        assert!(!glob_match("read_file", "write_file"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(glob_match("*_file", "read_file"));
        assert!(glob_match("*_file", "write_file"));
        assert!(!glob_match("*_file", "read_dir"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_match("read_*", "read_file"));
        assert!(glob_match("read_*", "read_dir"));
        assert!(!glob_match("read_*", "write_file"));
    }

    #[test]
    fn glob_star_middle() {
        assert!(glob_match("read_*_v2", "read_file_v2"));
        assert!(!glob_match("read_*_v2", "read_file_v3"));
    }

    #[test]
    fn glob_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_double_star() {
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "aXbY"));
    }

    // ---------------------------------------------------------------------
    // apply_tool_filters
    // ---------------------------------------------------------------------

    fn tool(name: &str) -> Tool {
        Tool {
            name: format!("srv_{name}"),
            description: None,
            input_schema: None,
            original_name: name.to_string(),
        }
    }

    fn cfg_with(include: &[&str], exclude: &[&str], aliases: &[(&str, &str)]) -> ServerConfig {
        ServerConfig {
            command: "echo".into(),
            include_tools: include.iter().map(|s| s.to_string()).collect(),
            exclude_tools: exclude.iter().map(|s| s.to_string()).collect(),
            tool_aliases: aliases
                .iter()
                .map(|(a, o)| (a.to_string(), o.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn filters_noop_when_no_rules() {
        let mut tools = vec![tool("read"), tool("write")];
        apply_tool_filters(&mut tools, "srv", &cfg_with(&[], &[], &[]));
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn include_filter_keeps_only_matching() {
        let mut tools = vec![tool("read_file"), tool("write_file"), tool("list_dirs")];
        apply_tool_filters(&mut tools, "srv", &cfg_with(&["*_file"], &[], &[]));
        let names: Vec<&str> = tools.iter().map(|t| t.original_name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "write_file"]);
    }

    #[test]
    fn exclude_filter_removes_matching() {
        let mut tools = vec![tool("read_file"), tool("delete_file"), tool("list")];
        apply_tool_filters(&mut tools, "srv", &cfg_with(&[], &["delete_*"], &[]));
        let names: Vec<&str> = tools.iter().map(|t| t.original_name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "list"]);
    }

    #[test]
    fn include_then_exclude() {
        let mut tools = vec![
            tool("read_file"),
            tool("write_file"),
            tool("delete_file"),
            tool("ignored"),
        ];
        apply_tool_filters(
            &mut tools,
            "srv",
            &cfg_with(&["*_file"], &["delete_*"], &[]),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.original_name.as_str()).collect();
        assert_eq!(names, vec!["read_file", "write_file"]);
    }

    #[test]
    fn alias_adds_tool_with_prefixed_name() {
        let mut tools = vec![tool("search_repositories")];
        apply_tool_filters(
            &mut tools,
            "gh",
            &cfg_with(&[], &[], &[("search", "search_repositories")]),
        );
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"srv_search_repositories"));
        assert!(names.contains(&"gh_search"));
        // Alias preserves the original name so call_tool routes correctly.
        let alias_tool = tools.iter().find(|t| t.name == "gh_search").unwrap();
        assert_eq!(alias_tool.original_name, "search_repositories");
    }

    #[test]
    fn alias_for_unknown_tool_is_ignored() {
        let mut tools = vec![tool("read")];
        apply_tool_filters(
            &mut tools,
            "srv",
            &cfg_with(&[], &[], &[("shortcut", "nonexistent")]),
        );
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn alias_runs_after_filters() {
        // Excluded original tool → no alias.
        let mut tools = vec![tool("dangerous")];
        apply_tool_filters(
            &mut tools,
            "srv",
            &cfg_with(&[], &["dangerous"], &[("safe", "dangerous")]),
        );
        assert_eq!(tools.len(), 0, "alias should not resurrect excluded tool");
    }
}
