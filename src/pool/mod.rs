//! Backend pool — lifecycle management for MCP backend processes.
//!
//! [`BackendPool`] owns backend processes and manages their lifecycle:
//! spawning, idle reaping, crash recovery, and hot-reload.
//! It does **not** know about custom tools — that concern lives in [`crate::server::Hub`].
//!
//! Module layout:
//! - [`env_key`]  — scope-key encoding (Global / Session / Credentials).
//! - [`entry`]    — `BackendEntry`, `BackendKey`, `HealthSync`, timestamp helpers.
//! - [`reap`]     — the idle-reap pass (runs every 60s).
//! - [`reload`]   — hot-reload diff + apply.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::backend::{Backend, Tool};
use crate::config::{Config, ServerConfig, Sharing, is_server_disabled};
use crate::jsonrpc::RpcError;

mod entry;
mod env_key;
mod reap;
mod reload;

use entry::{BackendEntry, BackendKey, HealthSync, SpawnLocks};
pub use env_key::relevant_env_keys;
use env_key::{
    DEFAULT_ENV_KEY, ENV_KEY_PREFIX, SESSION_KEY_PREFIX, backend_env_key, sharing_env_key,
};

/// Live handle returned by [`BackendPool::lookup_entry`]. Bundles everything a
/// caller needs to invoke the backend and react to its health without holding
/// the pool's read lock.
struct BackendHandle {
    key: BackendKey,
    backend: Arc<TokioMutex<Backend>>,
    health_sync: HealthSync,
    crashed: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default idle timeout if not configured per-server (15 min).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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

// ---------------------------------------------------------------------------
// BackendPool
// ---------------------------------------------------------------------------

/// Owns backend processes and manages their lifecycle: spawning, idle reaping,
/// crash recovery, and hot-reload. Does NOT know about custom tools — that
/// concern lives in [`crate::server::Hub`].
pub struct BackendPool {
    raw_config: Arc<RwLock<Config>>,
    backends: Arc<RwLock<HashMap<BackendKey, BackendEntry>>>,
    spawn_locks: SpawnLocks,
}

impl BackendPool {
    /// Boot the pool: spawns all default backend instances concurrently
    /// and starts a background reaper that kills idle instances.
    pub(crate) async fn new(raw_config: &Config) -> Result<Self> {
        let empty_env = HashMap::new();

        // Collect servers to start eagerly
        let mut to_start: Vec<(String, ServerConfig, String)> = Vec::new();
        let mut to_prebuild: Vec<(String, ServerConfig)> = Vec::new();

        info!("starting backends");
        for (name, srv) in &raw_config.servers {
            if is_server_disabled(name, srv) {
                debug!(server = %name, "skipping (disabled)");
                continue;
            }
            match srv.shared {
                Sharing::Session => {
                    debug!(server = %name, "deferring (per-session)");
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
                        debug!(server = %name, needs = missing.join(", "), "deferring");
                        to_prebuild.push((name.clone(), srv.clone()));
                        continue;
                    }
                }
                Sharing::Global => {}
            }
            let env_key = backend_env_key(srv, &empty_env);
            to_start.push((name.clone(), srv.clone(), env_key));
        }

        // Pre-build Docker images (sequentially — Docker builds are heavy)
        for (name, srv) in &to_prebuild {
            if srv.install.is_some() && matches!(srv.runtime, crate::config::Runtime::Docker) {
                info!(server = %name, "pre-building docker image");
                if let Err(e) = crate::docker::ensure_image(name, srv).await {
                    warn!(server = %name, error = %e, "docker pre-build failed");
                }
            }
        }

        // Spawn concurrently
        let mut handles = Vec::new();
        for (name, srv, env_key) in to_start {
            let env = empty_env.clone();
            handles.push(tokio::spawn(async move {
                match Backend::start(name.clone(), &srv, &env).await {
                    Ok(be) => {
                        info!(server = %name, tools = be.tools.len(), "backend ready");
                        Ok((name, env_key, be, srv))
                    }
                    Err(e) => {
                        error!(server = %name, "start failed: {e}");
                        Err(e)
                    }
                }
            }));
        }

        let mut backends = HashMap::new();
        for handle in handles {
            if let Ok(Ok((name, env_key, be, srv))) = handle.await {
                backends.insert(
                    BackendKey {
                        server: name,
                        scope: env_key,
                    },
                    BackendEntry::new(be, &srv),
                );
            }
        }

        let backends = Arc::new(RwLock::new(backends));
        let spawn_locks: SpawnLocks = Arc::new(TokioMutex::new(HashMap::new()));
        let raw_config = Arc::new(RwLock::new(raw_config.clone()));

        // Background reaper: every 60s, kill instances idle > their configured timeout.
        let reaper_backends = backends.clone();
        let reaper_config = raw_config.clone();
        let reaper_locks = spawn_locks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                reap::reap_once(&reaper_backends, &reaper_config, &reaper_locks).await;
            }
        });

        Ok(Self {
            raw_config,
            backends,
            spawn_locks,
        })
    }

    /// Run one reap pass: kill backends idle longer than their configured
    /// timeout. Exposed for tests — the background task calls the same logic
    /// on a 60s interval.
    #[cfg(test)]
    pub(crate) async fn reap_once(&self) {
        reap::reap_once(&self.backends, &self.raw_config, &self.spawn_locks).await;
    }

    /// Remove backend entries from the map and kill them outside the lock.
    /// Also purges the corresponding spawn locks.
    async fn remove_and_kill(&self, keys: &[BackendKey]) {
        if keys.is_empty() {
            return;
        }
        let to_kill: Vec<(BackendKey, Arc<TokioMutex<Backend>>)> = {
            let mut map = self.backends.write().await;
            keys.iter()
                .filter_map(|key| {
                    let entry = map.remove(key)?;
                    Some((key.clone(), entry.backend))
                })
                .collect()
        };
        for (key, be_arc) in &to_kill {
            be_arc.lock().await.kill();
            info!(server = %key.server, scope = %key.scope, "killed backend");
        }
        purge_spawn_locks(&self.spawn_locks, keys).await;
    }

    /// Get or spawn a backend for the given server + client context.
    pub(crate) async fn ensure_backend(
        &self,
        name: &str,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<BackendKey> {
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

        // Fast path: read lock + atomic touch (no write lock contention).
        {
            let map = self.backends.read().await;
            if let Some(entry) = map.get(&key) {
                entry.touch();
                return Ok(key);
            }
        }

        // Acquire per-key spawn lock to prevent duplicate spawns.
        let spawn_lock = {
            let mut locks = self.spawn_locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };
        let _guard = spawn_lock.lock().await;

        // Re-check after acquiring the spawn lock (another task may have spawned it).
        {
            let map = self.backends.read().await;
            if let Some(entry) = map.get(&key) {
                entry.touch();
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

        info!("spawning backend {name} ({env_key})");
        let backend = Backend::start(name.to_string(), &srv, env_overrides).await?;

        let mut map = self.backends.write().await;
        map.insert(key.clone(), BackendEntry::new(backend, &srv));
        Ok(key)
    }

    /// Ensure all requested backends, then collect their tools.
    ///
    /// If `progress` is `Some`, sends a [`BackendProgress`] event per backend
    /// as it finishes spawning (used by HTTP for SSE). If `None`, runs silent.
    pub(crate) async fn list_tools(
        self: &Arc<Self>,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
        progress: Option<mpsc::Sender<BackendProgress>>,
    ) -> (Vec<Tool>, Vec<(String, String)>) {
        let names: Vec<String> = {
            let cfg = self.raw_config.read().await;
            if servers.is_empty() {
                cfg.active_server_names()
            } else {
                servers.to_vec()
            }
        };

        let env = Arc::new(env_overrides.clone());
        let mut handles = Vec::new();
        for name in names {
            let pool = Arc::clone(self);
            let env = Arc::clone(&env);
            let sid = session_id.to_string();
            let progress = progress.clone();
            handles.push(tokio::spawn(async move {
                match pool.ensure_backend(&name, &env, &sid).await {
                    Ok(key) => {
                        if let Some(tx) = &progress {
                            let tools = {
                                let map = pool.backends.read().await;
                                map.get(&key)
                                    .map(|e| e.tool_count.load(Ordering::Acquire) as usize)
                                    .unwrap_or(0)
                            };
                            let _ = tx
                                .send(BackendProgress::Ready {
                                    server: name.clone(),
                                    tools,
                                })
                                .await;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        warn!(server = %name, error = %e, "failed to ensure backend");
                        let msg = e.to_string();
                        if let Some(tx) = &progress {
                            let _ = tx
                                .send(BackendProgress::Failed {
                                    server: name.clone(),
                                    error: msg.clone(),
                                })
                                .await;
                        }
                        Err((name, msg))
                    }
                }
            }));
        }
        drop(progress); // close channel when all tasks finish

        let mut errors = Vec::new();
        for handle in handles {
            if let Ok(Err(err)) = handle.await {
                errors.push(err);
            }
        }

        (
            self.collect_tools(servers, env_overrides, session_id).await,
            errors,
        )
    }

    /// Direct map lookup: for each configured server matching the filter,
    /// compute its scope key and read the entry's tools (if present).
    async fn collect_tools(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<Tool> {
        let keys: Vec<BackendKey> = {
            let cfg = self.raw_config.read().await;
            cfg.servers
                .iter()
                .filter(|(name, srv)| {
                    (servers.is_empty() || servers.contains(name)) && !is_server_disabled(name, srv)
                })
                .map(|(name, srv)| BackendKey {
                    server: name.clone(),
                    scope: sharing_env_key(srv, env_overrides, session_id),
                })
                .collect()
        };

        let backends: Vec<Arc<TokioMutex<Backend>>> = {
            let map = self.backends.read().await;
            keys.iter()
                .filter_map(|k| {
                    let entry = map.get(k)?;
                    entry.touch();
                    Some(entry.backend.clone())
                })
                .collect()
        };

        let mut out = Vec::new();
        for be_arc in backends {
            let be = be_arc.lock().await;
            out.extend(be.tools.iter().cloned());
        }
        out
    }

    /// Route a tool call to the correct backend. If the backend was reaped
    /// for idleness, respawn it on demand before returning `ToolNotFound`.
    /// If the existing entry's process has died and exhausted its restart
    /// budget, evict it and respawn so the same session isn't permanently
    /// stuck on a dead process.
    ///
    /// Owner resolution goes through config (not the live map), so a tool that
    /// belongs to a reaped server can't be accidentally claimed by a sibling
    /// server whose name is a shorter prefix.
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        args: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value, RpcError> {
        let Some((server_name, srv)) = self.server_for_tool(name, servers).await else {
            return Err(RpcError::ToolNotFound(name.to_string()));
        };
        let prefix_len = server_name.len() + 1;
        let original_name = name[prefix_len..].to_string();
        let scope = sharing_env_key(&srv, env_overrides, session_id);

        // First attempt: hit the existing entry if present.
        if let Some(h) = self.lookup_entry(&server_name, &scope).await {
            let result = Self::invoke(
                h.backend.clone(),
                &original_name,
                args.clone(),
                &h.health_sync,
            )
            .await;
            // Return on success, or when the backend is still alive (transient
            // error). Only evict when the process is dead AND auto-restart has
            // given up — otherwise we'd starve a backend that's mid-restart.
            if result.is_ok() || !h.crashed.load(Ordering::Acquire) {
                return result;
            }
            warn!(server = %server_name, scope = %h.key.scope, "evicting dead backend after failed call");
            self.remove_and_kill(&[h.key]).await;
            // Fall through to spawn-and-retry.
        }

        info!(server = %server_name, tool = %name, "spawning backend for tool call");
        self.ensure_backend(&server_name, env_overrides, session_id)
            .await
            .map_err(|e| {
                RpcError::Internal(format!("failed to spawn backend '{server_name}': {e}"))
            })?;

        match self.lookup_entry(&server_name, &scope).await {
            Some(h) => Self::invoke(h.backend, &original_name, args, &h.health_sync).await,
            None => Err(RpcError::ToolNotFound(name.to_string())),
        }
    }

    /// Direct hashmap lookup keyed by `(server, scope)`. Touches the entry's
    /// idle timer on hit.
    async fn lookup_entry(&self, server_name: &str, scope: &str) -> Option<BackendHandle> {
        let key = BackendKey {
            server: server_name.to_string(),
            scope: scope.to_string(),
        };
        let map = self.backends.read().await;
        let entry = map.get(&key)?;
        entry.touch();
        Some(BackendHandle {
            key,
            backend: entry.backend.clone(),
            health_sync: entry.health_sync(),
            crashed: entry.crashed.clone(),
        })
    }

    /// Find the configured server whose `{name}_` prefix matches the tool,
    /// preferring the longest match. Returns the server's config too so the
    /// caller can compute the scope key without re-locking.
    async fn server_for_tool(
        &self,
        name: &str,
        servers: &[String],
    ) -> Option<(String, ServerConfig)> {
        let cfg = self.raw_config.read().await;
        let mut best: Option<(usize, String, ServerConfig)> = None;
        for (srv_name, srv) in &cfg.servers {
            if !servers.is_empty() && !servers.contains(srv_name) {
                continue;
            }
            if is_server_disabled(srv_name, srv) {
                continue;
            }
            let prefix_len = srv_name.len() + 1;
            if name.len() > prefix_len
                && name.starts_with(srv_name.as_str())
                && name.as_bytes()[srv_name.len()] == b'_'
                && best.as_ref().is_none_or(|(l, _, _)| prefix_len > *l)
            {
                best = Some((prefix_len, srv_name.clone(), srv.clone()));
            }
        }
        best.map(|(_, n, s)| (n, s))
    }

    async fn invoke(
        be_arc: Arc<TokioMutex<Backend>>,
        original_name: &str,
        args: Value,
        health_sync: &HealthSync,
    ) -> Result<Value, RpcError> {
        let mut be = be_arc.lock().await;
        let result = be.call_tool(original_name, args).await;
        // Sync atomics in case call_tool triggered an auto-restart.
        health_sync.sync(&be);
        result.map_err(RpcError::from)
    }

    /// Health summary of backend processes.
    pub(crate) async fn health(&self) -> Value {
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
            // Read from atomics — no backend mutex needed.
            entries.push(serde_json::json!({
                "name": key.server,
                "scope": scope,
                "ready": entry.ready.load(Ordering::Acquire),
                "crashed": entry.crashed.load(Ordering::Acquire),
                "tools": entry.tool_count.load(Ordering::Acquire),
                "restarts": entry.restart_count.load(Ordering::Acquire),
                "idle_secs": entry.idle_secs(),
            }));
        }

        serde_json::json!({
            "backends": entries,
            "session_count": session_count,
        })
    }

    /// Kill all backends belonging to a specific session.
    pub(crate) async fn cleanup_session(&self, session_id: &str) {
        let session_key = format!("{SESSION_KEY_PREFIX}{session_id}");
        let keys: Vec<BackendKey> = {
            let map = self.backends.read().await;
            map.keys()
                .filter(|k| k.scope == session_key)
                .cloned()
                .collect()
        };
        for key in &keys {
            info!("cleaning up {} for session {session_id}", key.server);
        }
        self.remove_and_kill(&keys).await;
    }

    /// Kill all backend processes and wait for them to exit.
    pub(crate) async fn shutdown(&self) {
        let mut map = self.backends.write().await;
        for entry in map.values_mut() {
            entry.backend.lock().await.kill_and_wait().await;
        }
    }

    /// Hot-reload backend processes. Returns whether custom tools changed.
    pub(crate) async fn reload_backends(&self, new_config: &Config) -> bool {
        reload::reload_backends(
            &self.backends,
            &self.raw_config,
            &self.spawn_locks,
            new_config,
        )
        .await
    }
}

/// Drop the per-key spawn lock entries for `keys`. Does not block any holder
/// of an outstanding lock Arc — those just finish normally.
pub(super) async fn purge_spawn_locks(spawn_locks: &SpawnLocks, keys: &[BackendKey]) {
    if keys.is_empty() {
        return;
    }
    let mut locks = spawn_locks.lock().await;
    for key in keys {
        locks.remove(key);
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end reap + respawn tests using a tiny python mock MCP server.
    //! Env-key helper tests live in [`super::env_key`].

    use super::*;
    use crate::config::{Config, Sharing};

    /// Minimal MCP server: responds to initialize / tools/list / tools/call
    /// and appends its pid + timestamp to $MCP_SPAWN_LOG every spawn.
    const MOCK_MCP_SERVER: &str = r#"
import json, os, sys, time
log = os.environ["MCP_SPAWN_LOG"]
with open(log, "a") as f:
    f.write(f"{os.getpid()} {time.time()}\n")
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

    fn mock_server_config(
        script_path: &std::path::Path,
        spawn_log: &std::path::Path,
        idle_timeout_secs: u64,
    ) -> ServerConfig {
        let mut env = HashMap::new();
        env.insert("MCP_SPAWN_LOG".to_string(), spawn_log.display().to_string());
        ServerConfig {
            command: "python3".into(),
            args: vec![script_path.display().to_string()],
            env,
            // Session sharing so the entry is reapable; we spawn it explicitly
            // in the test via ensure_backend rather than at startup.
            shared: Sharing::Session,
            idle_timeout_secs: Some(idle_timeout_secs),
            ..Default::default()
        }
    }

    async fn backend_count(pool: &BackendPool) -> usize {
        pool.backends.read().await.len()
    }

    #[tokio::test]
    async fn reaped_backend_comes_back_on_tool_call() {
        // Requires python3; skip if not on PATH.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!("mcp_reap_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("mock_mcp.py");
        let spawn_log = tmp.join("spawns.log");
        std::fs::write(&script, MOCK_MCP_SERVER).unwrap();

        let mut servers = HashMap::new();
        servers.insert(
            "fake".to_string(),
            mock_server_config(&script, &spawn_log, 1),
        );
        let config = Config {
            servers,
            ..Default::default()
        };

        let pool = BackendPool::new(&config).await.unwrap();

        // Session-scoped backends are deferred — spawn explicitly.
        let env = HashMap::new();
        pool.ensure_backend("fake", &env, "sess").await.unwrap();

        assert_eq!(backend_count(&pool).await, 1, "spawned on ensure_backend");
        let initial_spawns = std::fs::read_to_string(&spawn_log).unwrap();
        assert_eq!(initial_spawns.lines().count(), 1, "one spawn logged");

        // Wait past the 1s idle timeout, then drive the reaper directly.
        // idle_secs() truncates to seconds, so we need > 2s elapsed to make
        // a 1s idle_timeout trigger (1 > 1 is false).
        tokio::time::sleep(Duration::from_millis(2200)).await;
        pool.reap_once().await;

        assert_eq!(backend_count(&pool).await, 0, "reaped");

        // Tool call after reap must transparently respawn and succeed.
        let result = pool
            .call_tool("fake_ping", serde_json::json!({}), &[], &env, "sess")
            .await
            .expect("call should succeed via respawn");
        assert!(result.get("pid").is_some(), "got tool result: {result}");

        // Back in the map, and the mock script logged a second spawn.
        assert_eq!(backend_count(&pool).await, 1, "respawned");
        let spawns = std::fs::read_to_string(&spawn_log).unwrap();
        assert_eq!(spawns.lines().count(), 2, "two spawns total:\n{spawns}");

        // Different pids for the two spawns (fresh process).
        let pids: Vec<&str> = spawns
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        assert_ne!(pids[0], pids[1], "respawn produced a fresh pid");

        pool.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn touch_during_reap_spares_entry() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!("mcp_touch_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("mock_mcp.py");
        let spawn_log = tmp.join("spawns.log");
        std::fs::write(&script, MOCK_MCP_SERVER).unwrap();

        let mut servers = HashMap::new();
        servers.insert(
            "fake".to_string(),
            mock_server_config(&script, &spawn_log, 1),
        );
        let config = Config {
            servers,
            ..Default::default()
        };

        let pool = BackendPool::new(&config).await.unwrap();
        pool.ensure_backend("fake", &HashMap::new(), "sess")
            .await
            .unwrap();
        // idle_secs() truncates to seconds, so we need > 2s elapsed to make
        // a 1s idle_timeout trigger (1 > 1 is false).
        tokio::time::sleep(Duration::from_millis(2200)).await;

        // Touch the entry right before the reaper re-checks under write lock.
        {
            let map = pool.backends.read().await;
            for entry in map.values() {
                entry.touch();
            }
        }
        pool.reap_once().await;

        assert_eq!(
            backend_count(&pool).await,
            1,
            "touched entry should not be reaped"
        );

        pool.shutdown().await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
