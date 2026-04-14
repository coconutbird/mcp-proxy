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

use crate::config::{ServerConfig, expand_env_with_overrides};

/// Default RPC timeout if not configured per-server.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(serde::Serialize)]
struct Rpc<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}
#[derive(serde::Deserialize)]
struct Resp {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcErr>,
}
#[derive(serde::Deserialize)]
struct RpcErr {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

/// A tool discovered from a backend, namespaced as <backend>_<tool>.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(skip)]
    #[allow(dead_code)]
    pub original_name: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub backend_name: String,
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
pub struct Backend {
    pub name: String,
    pub tools: Vec<Tool>,
    pub ready: bool,
    /// Number of times this backend has been restarted after a crash.
    pub restart_count: u32,
    /// Whether the process has exited unexpectedly.
    crashed: Arc<AtomicBool>,
    /// Notified when the process exits so callers can trigger restart.
    exit_notify: Arc<Notify>,
    seq: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
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
            stdin,
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
    ) -> Result<(
        Arc<Mutex<Pending>>,
        Arc<Mutex<Option<tokio::process::ChildStdin>>>,
        Child,
    )> {
        let expand = |s: &str| expand_env_with_overrides(s, extra_env);

        let args: Vec<String> = cfg.args.iter().map(|a| expand(a)).collect();
        let mut env: HashMap<String, String> = cfg
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand(v)))
            .collect();
        for (k, v) in extra_env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }

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

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();
        let child_stderr = child.stderr.take().unwrap();

        let pending: Arc<Mutex<Pending>> = Arc::default();
        let stdin = Arc::new(Mutex::new(Some(child_stdin)));

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
                let Ok(resp) = serde_json::from_str::<Resp>(&line) else {
                    warn!("[{tag}] non-json stdout: {line}");
                    continue;
                };
                let Some(id) = resp.id else { continue };
                if let Some(tx) = p.lock().await.remove(&id) {
                    let val = match resp.error {
                        Some(e) => Err(anyhow::anyhow!("{}", e.message)),
                        None => Ok(resp.result.unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(val);
                }
            }
            // stdout closed → process exited — fail all in-flight RPCs immediately
            crash_flag.store(true, Ordering::Relaxed);
            let mut pending = p.lock().await;
            for (_id, tx) in pending.drain() {
                let _ = tx.send(Err(anyhow::anyhow!("[{tag}] backend process exited")));
            }
            drop(pending);
            notify.notify_waiters();
            warn!("[{tag}] process exited (stdout closed)");
        });

        Ok((pending, stdin, child))
    }

    /// Apply include/exclude tool filters and aliases from the config.
    fn apply_tool_filters(&mut self) {
        let cfg = &self.cfg_snapshot;

        // Apply include filter
        if !cfg.include_tools.is_empty() {
            self.tools.retain(|t| {
                cfg.include_tools
                    .iter()
                    .any(|pat| glob_match(pat, &t.original_name))
            });
        }

        // Apply exclude filter
        if !cfg.exclude_tools.is_empty() {
            self.tools.retain(|t| {
                !cfg.exclude_tools
                    .iter()
                    .any(|pat| glob_match(pat, &t.original_name))
            });
        }

        // Apply aliases: add aliased copies of tools
        let mut aliased: Vec<Tool> = Vec::new();
        for (alias, original) in &cfg.tool_aliases {
            if let Some(t) = self.tools.iter().find(|t| t.original_name == *original) {
                let mut cloned = t.clone();
                cloned.name = format!("{}_{alias}", self.name);
                aliased.push(cloned);
            }
        }
        self.tools.extend(aliased);
    }

    /// Returns true if the backend process has crashed.
    pub fn has_crashed(&self) -> bool {
        self.crashed.load(Ordering::Relaxed)
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
        self.crashed.store(false, Ordering::Relaxed);
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
        self.stdin = stdin;
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
                "protocolVersion": "2024-11-05",
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
                            backend_name: self.name.clone(),
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

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        // Check if process has crashed before trying
        if self.crashed.load(Ordering::Relaxed) {
            bail!(
                "[{}] backend process has crashed — restart pending",
                self.name
            );
        }

        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let msg = serde_json::to_string(&Rpc {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut g = self.stdin.lock().await;
            let w = g.as_mut().ok_or_else(|| {
                anyhow::anyhow!("[{}] stdin closed — backend may have crashed", self.name)
            })?;
            crate::util::write_line(w, msg.as_bytes()).await?;
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

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params,
        }))?;
        let mut g = self.stdin.lock().await;
        if let Some(w) = g.as_mut() {
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
    pub fn kill(&mut self) {
        if let Some(ref mut c) = self.child {
            let _ = c.start_kill();
        }
    }

    /// Kill the child process and wait for it to fully exit.
    pub async fn kill_and_wait(&mut self) {
        if let Some(ref mut c) = self.child {
            let _ = c.start_kill();
            let _ = c.wait().await;
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
}
