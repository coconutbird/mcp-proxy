//! The Hub — central aggregation layer for MCP backends.
//!
//! This module is split into two layers:
//!
//! - [`BackendPool`] — owns backend processes and their lifecycle (spawning,
//!   reaping idle instances, crash recovery, hot-reload).
//! - [`Hub`] — thin routing layer that combines the pool with custom tools
//!   and exposes a unified JSON-RPC dispatch surface.
//!
//! Two transports (stdio and HTTP) both delegate to [`Hub::handle_request`]
//! for a single unified JSON-RPC dispatch path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::backend::{Backend, Tool};
use crate::config::{Config, ServerConfig, Sharing, diff_fields, extract_var_names};
use crate::custom_tools::CustomTools;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// MCP protocol version used in handshakes and initialize responses.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default env key for backends with no per-client env overrides.
const DEFAULT_ENV_KEY: &str = "__default__";

/// Session key prefix for per-session backends.
const SESSION_KEY_PREFIX: &str = "session:";

/// Env key prefix for credential-scoped backends.
const ENV_KEY_PREFIX: &str = "env:";

/// Session ID used for stdio transport (single implicit session).
pub const STDIO_SESSION_ID: &str = "__stdio__";

// ---------------------------------------------------------------------------
// JSON-RPC helpers
// ---------------------------------------------------------------------------

/// Build a JSON-RPC 2.0 success response.
pub fn jsonrpc_ok(id: &Value, result: Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build a JSON-RPC 2.0 error response.
pub fn jsonrpc_err(id: &Value, code: i64, message: &str) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

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
        match self {
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::ToolNotFound(_) => -32002,
            Self::Internal(_) => -32000,
        }
    }

    pub fn to_json(&self, id: &Value) -> Value {
        jsonrpc_err(id, self.code(), &self.to_string())
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Progress events
// ---------------------------------------------------------------------------

/// Progress event sent while backends are starting up.
#[derive(Debug, Clone)]
pub enum BackendProgress {
    /// Backend started successfully with N tools.
    Ready { server: String, tools: usize },
    /// Backend failed to start.
    Failed { server: String, error: String },
}

/// Default idle timeout if not configured per-server (15 min).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A single backend instance, keyed by (server_name, relevant_env_hash).
struct BackendEntry {
    backend: Arc<tokio::sync::Mutex<Backend>>,
    /// `None` = default instance (no env overrides, never reaped).
    last_used: Option<Instant>,
}

impl BackendEntry {
    /// Wrap a backend in an entry. `reapable = true` starts the idle timer;
    /// `false` means the instance is never reaped (e.g. Global sharing).
    fn new(backend: Backend, reapable: bool) -> Self {
        Self {
            backend: Arc::new(tokio::sync::Mutex::new(backend)),
            last_used: if reapable { Some(Instant::now()) } else { None },
        }
    }
}

/// Composite key: server name + hash of only the env vars that server cares about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BackendKey {
    server: String,
    scope: String,
}

use crate::util::fnv1a;

/// Extract the `${VAR}` references from a server config's env values and args.
pub fn relevant_env_keys(srv: &ServerConfig) -> Vec<String> {
    let mut keys = Vec::new();
    for val in srv.env.values() {
        keys.extend(extract_var_names(val));
    }
    for arg in &srv.args {
        keys.extend(extract_var_names(arg));
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Build the env-hash portion of a backend key, using only the env vars
/// that this server actually references.
fn backend_env_key(srv: &ServerConfig, env_overrides: &HashMap<String, String>) -> String {
    let keys = relevant_env_keys(srv);
    let relevant: Vec<(&str, &str)> = keys
        .iter()
        .filter_map(|k| {
            env_overrides
                .get(k.as_str())
                .map(|v| (k.as_str(), v.as_str()))
        })
        .collect();
    if relevant.is_empty() {
        return DEFAULT_ENV_KEY.to_string();
    }
    let s: String = relevant
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\0");
    format!("{ENV_KEY_PREFIX}{:x}", fnv1a(&s))
}

/// Compute the env-key portion of a backend key based on sharing mode.
fn sharing_env_key(
    srv: &ServerConfig,
    env_overrides: &HashMap<String, String>,
    session_id: &str,
) -> String {
    match srv.shared {
        Sharing::Global => DEFAULT_ENV_KEY.to_string(),
        Sharing::Credentials => backend_env_key(srv, env_overrides),
        Sharing::Session => format!("{SESSION_KEY_PREFIX}{session_id}"),
    }
}

// ---------------------------------------------------------------------------
// BackendPool — backend lifecycle management
// ---------------------------------------------------------------------------

/// Per-key mutex to prevent duplicate backend spawns.
type SpawnLocks = Arc<TokioMutex<HashMap<BackendKey, Arc<TokioMutex<()>>>>>;

/// Owns backend processes and manages their lifecycle: spawning, idle reaping,
/// crash recovery, and hot-reload. Does NOT know about custom tools — that
/// concern lives in [`Hub`].
pub struct BackendPool {
    raw_config: Arc<RwLock<Config>>,
    backends: Arc<RwLock<HashMap<BackendKey, BackendEntry>>>,
    spawn_locks: SpawnLocks,
}

impl BackendPool {
    /// Boot the pool: spawns all default backend instances concurrently
    /// and starts a background reaper that kills idle instances.
    async fn new(raw_config: &Config) -> Result<Self> {
        let empty_env = HashMap::new();

        // Collect servers to start eagerly
        let mut to_start: Vec<(String, ServerConfig, String)> = Vec::new();
        let mut to_prebuild: Vec<(String, ServerConfig)> = Vec::new();

        info!("starting backends");
        for (name, srv) in &raw_config.servers {
            if srv.is_disabled(name) {
                debug!(server = name, "skipping (disabled)");
                continue;
            }
            match srv.shared {
                Sharing::Session => {
                    debug!(server = name, "deferring (per-session)");
                    to_prebuild.push((name.clone(), srv.clone()));
                    continue;
                }
                Sharing::Credentials => {
                    let required = relevant_env_keys(srv);
                    let missing: Vec<&str> = required
                        .iter()
                        .filter(|k| std::env::var(k).is_err())
                        .map(|s| s.as_str())
                        .collect();
                    if !missing.is_empty() {
                        debug!(server = name, needs = missing.join(", "), "deferring");
                        to_prebuild.push((name.clone(), srv.clone()));
                        continue;
                    }
                }
                Sharing::Global => {}
            }
            let env_key = backend_env_key(srv, &empty_env);
            to_start.push((name.clone(), srv.clone(), env_key));
        }

        // Start all eager backends concurrently
        let mut handles = Vec::new();
        for (name, srv, env_key) in to_start {
            let empty = empty_env.clone();
            handles.push(tokio::spawn(async move {
                info!(server = %name, "starting");
                match Backend::start(name.clone(), &srv, &empty).await {
                    Ok(be) => Ok((name, env_key, be)),
                    Err(e) => {
                        error!(server = %name, error = %e, "failed to start");
                        Err((name, e))
                    }
                }
            }));
        }

        let mut backends = HashMap::new();
        for handle in handles {
            match handle.await {
                Ok(Ok((name, env_key, be))) => {
                    backends.insert(
                        BackendKey {
                            server: name,
                            scope: env_key,
                        },
                        BackendEntry::new(be, false),
                    );
                }
                Ok(Err((name, e))) => {
                    error!(server = %name, "startup failed: {e}");
                }
                Err(e) => error!("spawn task panicked: {e}"),
            }
        }

        // Pre-build Docker images for deferred servers so first connect is fast.
        for (name, srv) in &to_prebuild {
            if srv.install.is_some() && matches!(srv.runtime, crate::config::Runtime::Docker) {
                info!(server = name, "pre-building docker image");
                if let Err(e) = crate::docker::ensure_image(name, srv).await {
                    warn!(server = name, error = %e, "docker pre-build failed");
                }
            }
        }

        let raw_config = Arc::new(RwLock::new(raw_config.clone()));
        let backends = Arc::new(RwLock::new(backends));
        let spawn_locks: SpawnLocks = Arc::new(TokioMutex::new(HashMap::new()));

        // Background reaper: every 60s, kill instances idle > their configured timeout.
        let reaper_backends = backends.clone();
        let reaper_config = raw_config.clone();
        let reaper_locks = spawn_locks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let cfg = reaper_config.read().await;
                let mut map = reaper_backends.write().await;
                let stale: Vec<BackendKey> = map
                    .iter()
                    .filter_map(|(key, entry)| {
                        let last = entry.last_used?;
                        let idle_timeout = cfg
                            .servers
                            .get(&key.server)
                            .and_then(|s| s.idle_timeout_secs)
                            .map(Duration::from_secs)
                            .unwrap_or(DEFAULT_IDLE_TIMEOUT);
                        if last.elapsed() > idle_timeout {
                            Some(key.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                drop(cfg);
                for key in &stale {
                    if let Some(entry) = map.remove(key) {
                        entry.backend.lock().await.kill();
                        info!(server = %key.server, scope = %key.scope, "reaped idle backend");
                    }
                }
                if !stale.is_empty() {
                    let mut locks = reaper_locks.lock().await;
                    for key in &stale {
                        locks.remove(key);
                    }
                }
            }
        });

        Ok(Self {
            raw_config,
            backends,
            spawn_locks,
        })
    }

    /// Remove spawn-lock entries for the given backend keys.
    async fn purge_spawn_locks(&self, keys: &[BackendKey]) {
        if keys.is_empty() {
            return;
        }
        let mut locks = self.spawn_locks.lock().await;
        for key in keys {
            locks.remove(key);
        }
    }

    /// Get or spawn a backend for the given server + client context.
    ///
    /// Key strategy by sharing mode:
    ///   Global      → (name, "__default__")           — one process for all
    ///   Credentials → (name, env:<hash of cred vars>) — shared when creds match
    ///   Session     → (name, session:<id>)            — isolated per client
    async fn ensure_backend(
        &self,
        name: &str,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<BackendKey> {
        // Read config once, clone what we need, then release
        let srv = {
            let cfg = self.raw_config.read().await;
            cfg.servers
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown server: {name}"))?
        };

        let env_key = sharing_env_key(&srv, env_overrides, session_id);
        let key = BackendKey {
            server: name.to_string(),
            scope: env_key.clone(),
        };

        // Fast path: already running, touch idle timer
        {
            let mut map = self.backends.write().await;
            if let Some(entry) = map.get_mut(&key) {
                if entry.last_used.is_some() {
                    entry.last_used = Some(Instant::now());
                }
                return Ok(key);
            }
        }

        // Acquire per-key spawn lock to prevent duplicate spawns.
        // Two concurrent requests for the same un-spawned backend will
        // serialize here; the second one will find it in the map.
        let spawn_lock = {
            let mut locks = self.spawn_locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };
        let _guard = spawn_lock.lock().await;

        // Re-check after acquiring the spawn lock — another task may have
        // already spawned this backend while we were waiting.
        {
            let mut map = self.backends.write().await;
            if let Some(entry) = map.get_mut(&key) {
                if entry.last_used.is_some() {
                    entry.last_used = Some(Instant::now());
                }
                return Ok(key);
            }
        }

        // Fail fast: check all required env vars are resolvable.
        let required = relevant_env_keys(&srv);
        let missing: Vec<&str> = required
            .iter()
            .filter(|k| !env_overrides.contains_key(k.as_str()) && std::env::var(k).is_err())
            .map(|s| s.as_str())
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "server '{name}' requires env vars not provided: {}",
                missing.join(", ")
            );
        }

        // Spawn new instance
        info!("spawning backend {name} ({env_key})");
        let backend = Backend::start(name.to_string(), &srv, env_overrides).await?;

        let reapable = !matches!(srv.shared, Sharing::Global);
        let mut map = self.backends.write().await;
        map.insert(key.clone(), BackendEntry::new(backend, reapable));
        Ok(key)
    }

    /// Ensure all requested backends exist (or all if `servers` is empty).
    /// Spawns backends concurrently for faster startup.
    /// Returns a list of `(server_name, error_message)` for backends that
    /// failed to start so callers can surface them to the client.
    async fn ensure_backends(
        self: &Arc<Self>,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<(String, String)> {
        let cfg = self.raw_config.read().await;
        let names: Vec<String> = if servers.is_empty() {
            cfg.active_server_names()
        } else {
            servers.to_vec()
        };
        drop(cfg);

        let mut handles = Vec::new();
        for name in names {
            let pool = Arc::clone(self);
            let env = env_overrides.clone();
            let sid = session_id.to_string();
            handles.push(tokio::spawn(async move {
                if let Err(e) = pool.ensure_backend(&name, &env, &sid).await {
                    warn!(server = %name, error = %e, "failed to ensure backend");
                    Some((name, e.to_string()))
                } else {
                    None
                }
            }));
        }

        let mut errors = Vec::new();
        for handle in handles {
            if let Ok(Some(err)) = handle.await {
                errors.push(err);
            }
        }
        errors
    }

    /// Compute the expected backend key for a server given the client context.
    /// Takes a config ref to avoid re-acquiring the lock.
    fn expected_key_with(
        cfg: &Config,
        srv_name: &str,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Option<String> {
        let srv = cfg.servers.get(srv_name)?;
        Some(sharing_env_key(srv, env_overrides, session_id))
    }

    /// Return matching backends for the given server filter, touching idle
    /// timers along the way. Each entry is `(server_name, backend_arc)`.
    async fn matched_backends(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<(String, Arc<tokio::sync::Mutex<Backend>>)> {
        let cfg = self.raw_config.read().await;
        let mut map = self.backends.write().await;
        let mut matched = Vec::new();

        for (key, entry) in map.iter_mut() {
            if !servers.is_empty() && !servers.contains(&key.server) {
                continue;
            }
            let expected =
                match Self::expected_key_with(&cfg, &key.server, env_overrides, session_id) {
                    Some(k) => k,
                    None => continue,
                };
            if key.scope != expected {
                continue;
            }
            if entry.last_used.is_some() {
                entry.last_used = Some(Instant::now());
            }
            matched.push((key.server.clone(), entry.backend.clone()));
        }
        matched
    }

    /// Collect tools from matching backends, touching idle timers.
    /// Returns only backend tools — custom tools are added by [`Hub`].
    async fn backend_tools(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<Tool> {
        let matched = self
            .matched_backends(servers, env_overrides, session_id)
            .await;
        let mut out: Vec<Tool> = Vec::new();
        for (_name, be_arc) in &matched {
            let be = be_arc.lock().await;
            out.extend(be.tools.iter().cloned());
        }
        out
    }

    /// Ensure all backends then collect their tools with progress events.
    /// Returns only backend tools — custom tools are added by [`Hub`].
    async fn backend_tools_streaming(
        self: &Arc<Self>,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
        tx: mpsc::Sender<BackendProgress>,
    ) -> (Vec<Tool>, Vec<(String, String)>) {
        let names: Vec<String> = {
            let cfg = self.raw_config.read().await;
            if servers.is_empty() {
                cfg.active_server_names()
            } else {
                servers.to_vec()
            }
        };

        // Spawn all backends concurrently
        let mut handles = Vec::new();
        for name in names {
            let pool = Arc::clone(self);
            let env = env_overrides.clone();
            let sid = session_id.to_string();
            let progress_tx = tx.clone();
            handles.push(tokio::spawn(async move {
                match pool.ensure_backend(&name, &env, &sid).await {
                    Ok(key) => {
                        let tool_count = {
                            let map = pool.backends.read().await;
                            match map.get(&key) {
                                Some(e) => e.backend.lock().await.tools.len(),
                                None => 0,
                            }
                        };
                        let _ = progress_tx
                            .send(BackendProgress::Ready {
                                server: name.clone(),
                                tools: tool_count,
                            })
                            .await;
                        Ok((name, key))
                    }
                    Err(e) => {
                        warn!(server = %name, error = %e, "failed to ensure backend");
                        let msg = e.to_string();
                        let _ = progress_tx
                            .send(BackendProgress::Failed {
                                server: name.clone(),
                                error: msg.clone(),
                            })
                            .await;
                        Err((name, msg))
                    }
                }
            }));
        }
        drop(tx);

        let mut errors = Vec::new();
        for handle in handles {
            if let Ok(Err((name, msg))) = handle.await {
                errors.push((name, msg));
            }
        }

        let tools = self.backend_tools(servers, env_overrides, session_id).await;
        (tools, errors)
    }

    /// Route a tool call to the correct backend by matching the tool name
    /// prefix against server names from the pre-filtered set.
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value, RpcError> {
        let matched = self
            .matched_backends(servers, env_overrides, session_id)
            .await;
        let (be_arc, original_name) = {
            let mut found = None;
            for (srv_name, be) in &matched {
                let prefix = format!("{srv_name}_");
                if let Some(original) = name.strip_prefix(&prefix) {
                    found = Some((be.clone(), original.to_string()));
                    break;
                }
            }
            match found {
                Some(f) => f,
                None => return Err(RpcError::ToolNotFound(name.to_string())),
            }
        };

        let mut be = be_arc.lock().await;
        be.call_tool(&original_name, args)
            .await
            .map_err(RpcError::from)
    }

    /// Health summary of backend processes.
    async fn health(&self) -> Value {
        let map = self.backends.read().await;
        let mut session_count = 0u64;
        let mut entries: Vec<Value> = Vec::new();

        for (key, entry) in map.iter() {
            let scope = if key.scope == DEFAULT_ENV_KEY {
                "global"
            } else if key.scope.starts_with(ENV_KEY_PREFIX) {
                "credentials"
            } else if key.scope.starts_with(SESSION_KEY_PREFIX) {
                session_count += 1;
                "session"
            } else {
                "session"
            };
            let be = entry.backend.lock().await;
            entries.push(serde_json::json!({
                "name": key.server,
                "scope": scope,
                "ready": be.ready,
                "crashed": be.has_crashed(),
                "tools": be.tools.len(),
                "restarts": be.restart_count,
                "idle_secs": entry.last_used.map(|t| t.elapsed().as_secs()),
            }));
        }

        serde_json::json!({
            "backends": entries,
            "session_count": session_count,
        })
    }

    /// Kill all backends belonging to a specific session.
    async fn cleanup_session(&self, session_id: &str) {
        let session_key = format!("{SESSION_KEY_PREFIX}{session_id}");
        let mut map = self.backends.write().await;
        let stale: Vec<BackendKey> = map
            .keys()
            .filter(|k| k.scope == session_key)
            .cloned()
            .collect();
        for key in &stale {
            if let Some(entry) = map.remove(key) {
                info!("cleaning up {} for session {session_id}", key.server);
                entry.backend.lock().await.kill();
            }
        }
        self.purge_spawn_locks(&stale).await;
    }

    /// Kill all backend processes and wait for them to exit.
    async fn shutdown(&self) {
        let mut map = self.backends.write().await;
        for entry in map.values_mut() {
            entry.backend.lock().await.kill_and_wait().await;
        }
    }

    /// Hot-reload backend processes: diff old vs new config, stop removed
    /// servers, restart changed servers, start new ones. Returns whether
    /// custom tools changed (so the caller can reload them separately).
    async fn reload_backends(&self, new_config: &Config) -> bool {
        // Fast path: full structural equality — skip everything if identical.
        {
            let old_cfg = self.raw_config.read().await;
            if *old_cfg == *new_config {
                debug!("config file re-read, no changes");
                return false;
            }
        }

        let empty_env = HashMap::new();

        let (to_remove, to_restart, to_add, custom_changed) = {
            let old_cfg = self.raw_config.read().await;
            let old_servers = &old_cfg.servers;
            let new_servers = &new_config.servers;

            let mut remove = Vec::new();
            let mut restart = Vec::new();
            let mut add = Vec::new();

            for (name, old_srv) in old_servers {
                if old_srv.is_disabled(name) {
                    continue;
                }
                match new_servers.get(name) {
                    None => remove.push(name.clone()),
                    Some(new_srv) if *old_srv != *new_srv => {
                        let changes = diff_fields(old_srv, new_srv);
                        info!(
                            server = %name,
                            fields = %changes.join(", "),
                            "server config changed"
                        );
                        restart.push(name.clone());
                    }
                    _ => {}
                }
            }

            for (name, srv) in new_servers {
                if !srv.is_disabled(name) && !old_servers.contains_key(name) {
                    add.push(name.clone());
                }
            }

            let custom_changed = old_cfg.custom_tools != new_config.custom_tools;
            (remove, restart, add, custom_changed)
        };

        if to_remove.is_empty() && to_restart.is_empty() && to_add.is_empty() && !custom_changed {
            info!("config reloaded — no actionable changes");
            *self.raw_config.write().await = new_config.clone();
            return false;
        }

        info!(
            removed = to_remove.len(),
            restarted = to_restart.len(),
            added = to_add.len(),
            custom_tools_changed = custom_changed,
            "applying config reload"
        );

        let new_servers = &new_config.servers;
        let mut map = self.backends.write().await;

        // 1. Remove deleted servers (kill all scopes)
        let mut removed_keys = Vec::new();
        for name in &to_remove {
            let keys: Vec<BackendKey> = map.keys().filter(|k| k.server == *name).cloned().collect();
            for key in keys {
                if let Some(entry) = map.remove(&key) {
                    entry.backend.lock().await.kill();
                    removed_keys.push(key);
                }
            }
            info!(server = %name, "removed");
        }

        // 2. Restart changed servers (kill default instance, re-spawn)
        for name in &to_restart {
            let default_key = BackendKey {
                server: name.clone(),
                scope: DEFAULT_ENV_KEY.to_string(),
            };
            if let Some(entry) = map.remove(&default_key) {
                entry.backend.lock().await.kill();
                removed_keys.push(default_key);
            }
            let Some(srv) = new_servers.get(name) else {
                continue;
            };
            if srv.is_disabled(name) {
                info!(server = %name, "disabled after reload");
                continue;
            }
            if !matches!(srv.shared, Sharing::Global) {
                continue;
            }
            info!(server = %name, "restarting");
            match Backend::start(name.clone(), srv, &empty_env).await {
                Ok(be) => {
                    let env_key = backend_env_key(srv, &empty_env);
                    map.insert(
                        BackendKey {
                            server: name.clone(),
                            scope: env_key,
                        },
                        BackendEntry::new(be, false),
                    );
                }
                Err(e) => error!(server = %name, "restart failed: {e}"),
            }
        }

        // 3. Start newly added servers
        for name in &to_add {
            let Some(srv) = new_servers.get(name) else {
                continue;
            };
            if srv.is_disabled(name) {
                continue;
            }
            if !matches!(srv.shared, Sharing::Global) {
                debug!(server = %name, "deferring new server (not Global)");
                continue;
            }
            info!(server = %name, "starting (new in config)");
            match Backend::start(name.clone(), srv, &empty_env).await {
                Ok(be) => {
                    let env_key = backend_env_key(srv, &empty_env);
                    map.insert(
                        BackendKey {
                            server: name.clone(),
                            scope: env_key,
                        },
                        BackendEntry::new(be, false),
                    );
                }
                Err(e) => error!(server = %name, "start failed: {e}"),
            }
        }

        drop(map);
        self.purge_spawn_locks(&removed_keys).await;

        *self.raw_config.write().await = new_config.clone();
        custom_changed
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

    /// Streaming version of `list_tools_for` — sends progress events as
    /// each backend finishes, then returns the combined tool list.
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
        // Check custom tools first
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

    /// Unified JSON-RPC method dispatch for both stdio and HTTP transports.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sharing;

    /// Build a minimal ServerConfig for testing.
    fn test_srv(env: Vec<(&str, &str)>, args: Vec<&str>, sharing: Sharing) -> ServerConfig {
        ServerConfig {
            command: "echo".into(),
            args: args.into_iter().map(String::from).collect(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            shared: sharing,
            ..Default::default()
        }
    }

    #[test]
    fn extract_var_names_basic() {
        assert_eq!(extract_var_names("--token=${TOKEN}"), vec!["TOKEN"]);
    }

    #[test]
    fn extract_var_names_multiple() {
        assert_eq!(extract_var_names("${A}_${B}_plain"), vec!["A", "B"]);
    }

    #[test]
    fn extract_var_names_none() {
        assert!(extract_var_names("no vars here").is_empty());
    }

    #[test]
    fn relevant_env_keys_deduplicates() {
        let srv = test_srv(
            vec![("X", "${TOK}"), ("Y", "${TOK}")],
            vec![],
            Sharing::Session,
        );
        let keys = relevant_env_keys(&srv);
        assert_eq!(keys, vec!["TOK"]);
    }

    #[test]
    fn relevant_env_keys_from_args_and_env() {
        let srv = test_srv(vec![("X", "${A}")], vec!["--flag=${B}"], Sharing::Session);
        let keys = relevant_env_keys(&srv);
        assert_eq!(keys, vec!["A", "B"]);
    }

    #[test]
    fn backend_env_key_default_when_no_overrides() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let key = backend_env_key(&srv, &HashMap::new());
        assert_eq!(key, "__default__");
    }

    #[test]
    fn backend_env_key_hashes_when_overrides_present() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let mut env = HashMap::new();
        env.insert("TOK".to_string(), "secret123".to_string());
        let key = backend_env_key(&srv, &env);
        assert!(key.starts_with("env:"), "expected env: prefix, got {key}");
    }

    #[test]
    fn backend_env_key_same_creds_same_hash() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let mut env1 = HashMap::new();
        env1.insert("TOK".to_string(), "abc".to_string());
        let mut env2 = HashMap::new();
        env2.insert("TOK".to_string(), "abc".to_string());
        assert_eq!(backend_env_key(&srv, &env1), backend_env_key(&srv, &env2));
    }

    #[test]
    fn backend_env_key_different_creds_different_hash() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let mut env1 = HashMap::new();
        env1.insert("TOK".to_string(), "abc".to_string());
        let mut env2 = HashMap::new();
        env2.insert("TOK".to_string(), "xyz".to_string());
        assert_ne!(backend_env_key(&srv, &env1), backend_env_key(&srv, &env2));
    }

    #[test]
    fn backend_env_key_ignores_irrelevant_overrides() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let mut env = HashMap::new();
        env.insert("TOK".to_string(), "val".to_string());
        env.insert("UNRELATED".to_string(), "noise".to_string());
        let key_with = backend_env_key(&srv, &env);

        let mut env2 = HashMap::new();
        env2.insert("TOK".to_string(), "val".to_string());
        let key_without = backend_env_key(&srv, &env2);

        assert_eq!(key_with, key_without);
    }
}
