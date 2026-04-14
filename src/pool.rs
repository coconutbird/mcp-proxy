//! Backend pool — lifecycle management for MCP backend processes.
//!
//! [`BackendPool`] owns backend processes and manages their lifecycle:
//! spawning, idle reaping, crash recovery, and hot-reload.
//! It does **not** know about custom tools — that concern lives in [`crate::server::Hub`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::backend::{Backend, Tool};
use crate::config::{
    Config, ServerConfig, Sharing, diff_fields, extract_var_names, is_server_disabled,
};
use crate::jsonrpc::RpcError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default env key for backends with no per-client env overrides.
const DEFAULT_ENV_KEY: &str = "__default__";

/// Session key prefix for per-session backends.
const SESSION_KEY_PREFIX: &str = "session:";

/// Env key prefix for credential-scoped backends.
const ENV_KEY_PREFIX: &str = "env:";

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
// Internal types
// ---------------------------------------------------------------------------

/// Epoch for atomic timestamps — `last_used` stores elapsed millis from this base.
static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Convert an `Instant` to milliseconds since EPOCH (for atomic storage).
fn instant_to_millis(t: Instant) -> u64 {
    t.duration_since(*EPOCH).as_millis() as u64
}

/// Convert stored millis back to an `Instant`.
fn millis_to_instant(ms: u64) -> Instant {
    *EPOCH + Duration::from_millis(ms)
}

/// Sentinel value meaning "not reapable" (never reaped).
const NOT_REAPABLE: u64 = 0;

/// A single backend instance, keyed by (server_name, relevant_env_hash).
///
/// `last_used_ms` is an atomic counter so that [`BackendPool::matched_backends`]
/// can bump it under a *read* lock instead of requiring a write lock on every
/// request. `NOT_REAPABLE` (0) means the entry is never reaped.
///
/// `sharing` and `env_key_vars` are cached from the server config at spawn time
/// so that hot-path matching can avoid locking `raw_config`.
struct BackendEntry {
    backend: Arc<TokioMutex<Backend>>,
    last_used_ms: AtomicU64,
    /// Cached sharing mode from the server config.
    sharing: Sharing,
    /// Cached list of relevant env variable names (for Credentials matching).
    env_key_vars: Vec<String>,
    // --- Atomic health snapshot (lock-free reads for /health) ---
    /// Number of discovered tools.
    tool_count: Arc<AtomicU64>,
    /// Whether the backend has completed the MCP handshake.
    ready: Arc<AtomicBool>,
    /// Number of crash-restarts.
    restart_count: Arc<AtomicU64>,
    /// Shared crash flag (same Arc as Backend.crashed).
    crashed: Arc<AtomicBool>,
}

/// Lightweight handle for syncing backend health atomics after a tool call.
/// Cloning `Arc<AtomicX>` is cheap and avoids holding a reference to the entry.
struct HealthSync {
    ready: Arc<AtomicBool>,
    tool_count: Arc<AtomicU64>,
    restart_count: Arc<AtomicU64>,
}

impl HealthSync {
    fn sync(&self, be: &Backend) {
        self.ready.store(be.ready, Ordering::Release);
        self.tool_count
            .store(be.tools.len() as u64, Ordering::Release);
        self.restart_count
            .store(be.restart_count as u64, Ordering::Release);
    }
}

impl BackendEntry {
    /// Wrap a backend in an entry. `reapable = true` starts the idle timer;
    /// `false` means the instance is never reaped (e.g. Global sharing).
    fn new(backend: Backend, srv: &ServerConfig) -> Self {
        let reapable = !matches!(srv.shared, Sharing::Global);
        let tool_count = backend.tools.len() as u64;
        let ready = backend.ready;
        let restart_count = backend.restart_count as u64;
        let crashed = backend.crashed_flag();
        Self {
            backend: Arc::new(TokioMutex::new(backend)),
            last_used_ms: AtomicU64::new(if reapable {
                instant_to_millis(Instant::now())
            } else {
                NOT_REAPABLE
            }),
            sharing: srv.shared.clone(),
            env_key_vars: relevant_env_keys(srv),
            tool_count: Arc::new(AtomicU64::new(tool_count)),
            ready: Arc::new(AtomicBool::new(ready)),
            restart_count: Arc::new(AtomicU64::new(restart_count)),
            crashed,
        }
    }

    /// Build a [`HealthSync`] handle from this entry (cheap Arc clones).
    fn health_sync(&self) -> HealthSync {
        HealthSync {
            ready: self.ready.clone(),
            tool_count: self.tool_count.clone(),
            restart_count: self.restart_count.clone(),
        }
    }

    /// Whether this entry is subject to idle reaping.
    fn is_reapable(&self) -> bool {
        self.last_used_ms.load(Ordering::Relaxed) != NOT_REAPABLE
    }

    /// Touch the entry (reset idle timer). No-op for non-reapable entries.
    fn touch(&self) {
        if self.is_reapable() {
            self.last_used_ms
                .store(instant_to_millis(Instant::now()), Ordering::Relaxed);
        }
    }

    /// Seconds since last use. Returns `None` for non-reapable entries.
    fn idle_secs(&self) -> Option<u64> {
        let ms = self.last_used_ms.load(Ordering::Relaxed);
        if ms == NOT_REAPABLE {
            return None;
        }
        Some(millis_to_instant(ms).elapsed().as_secs())
    }

    /// Compute the expected scope key for this entry using cached sharing
    /// info — no config lock required.
    fn expected_scope(&self, env_overrides: &HashMap<String, String>, session_id: &str) -> String {
        match self.sharing {
            Sharing::Global => DEFAULT_ENV_KEY.to_string(),
            Sharing::Session => format!("{SESSION_KEY_PREFIX}{session_id}"),
            Sharing::Credentials => {
                let relevant: Vec<&String> = self
                    .env_key_vars
                    .iter()
                    .filter(|k| env_overrides.contains_key(k.as_str()))
                    .collect();
                if relevant.is_empty() {
                    return DEFAULT_ENV_KEY.to_string();
                }
                let s = relevant
                    .iter()
                    .map(|k| format!("{}={}", k, env_overrides.get(k.as_str()).unwrap()))
                    .collect::<Vec<_>>()
                    .join("\0");
                format!("{ENV_KEY_PREFIX}{:x}", fnv1a(&s))
            }
        }
    }
}

/// Composite key: server name + hash of only the env vars that server cares about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BackendKey {
    server: String,
    scope: String,
}

/// Per-key mutex to prevent duplicate backend spawns.
type SpawnLocks = Arc<TokioMutex<HashMap<BackendKey, Arc<TokioMutex<()>>>>>;

// ---------------------------------------------------------------------------
// Env-key helpers
// ---------------------------------------------------------------------------

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
                let cfg = reaper_config.read().await;
                let stale: Vec<BackendKey> = {
                    let map = reaper_backends.read().await;
                    map.iter()
                        .filter_map(|(key, entry)| {
                            let idle = entry.idle_secs()?;
                            let idle_timeout = cfg
                                .servers
                                .get(&key.server)
                                .and_then(|s| s.idle_timeout_secs)
                                .unwrap_or(DEFAULT_IDLE_TIMEOUT.as_secs());
                            if idle > idle_timeout {
                                Some(key.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                drop(cfg);

                if stale.is_empty() {
                    continue;
                }

                // Remove stale entries under the write lock, but collect the
                // backend handles without killing yet — we don't want to hold
                // the map lock while awaiting backend mutexes.
                let to_kill: Vec<(BackendKey, Arc<TokioMutex<Backend>>)> = {
                    let mut map = reaper_backends.write().await;
                    stale
                        .iter()
                        .filter_map(|key| {
                            let entry = map.remove(key)?;
                            Some((key.clone(), entry.backend))
                        })
                        .collect()
                };

                // Now kill outside the map lock.
                for (key, be_arc) in &to_kill {
                    be_arc.lock().await.kill();
                    info!(server = %key.server, scope = %key.scope, "reaped idle backend");
                }

                let mut locks = reaper_locks.lock().await;
                for (key, _) in &to_kill {
                    locks.remove(key);
                }
            }
        });

        Ok(Self {
            raw_config,
            backends,
            spawn_locks,
        })
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
        self.purge_spawn_locks(keys).await;
    }

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

    /// Ensure all requested backends exist (spawns concurrently).
    pub(crate) async fn ensure_backends(
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

        // Wrap in Arc to avoid cloning the map per-task.
        let env = Arc::new(env_overrides.clone());
        let mut handles = Vec::new();
        for name in names {
            let pool = Arc::clone(self);
            let env = Arc::clone(&env);
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

    /// Return matching backends for the given server filter.
    ///
    /// Uses cached sharing mode on each entry so we only need the backends
    /// read-lock — no config lock on the hot path.
    pub(crate) async fn matched_backends(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<(String, Arc<TokioMutex<Backend>>)> {
        let map = self.backends.read().await;
        let mut matched = Vec::new();

        for (key, entry) in map.iter() {
            if !servers.is_empty() && !servers.contains(&key.server) {
                continue;
            }
            let expected = entry.expected_scope(env_overrides, session_id);
            if key.scope != expected {
                continue;
            }
            entry.touch();
            matched.push((key.server.clone(), entry.backend.clone()));
        }
        matched
    }

    /// Collect tools from matching backends.
    pub(crate) async fn backend_tools(
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
    pub(crate) async fn backend_tools_streaming(
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

        let env = Arc::new(env_overrides.clone());
        let mut handles = Vec::new();
        for name in names {
            let pool = Arc::clone(self);
            let env = Arc::clone(&env);
            let sid = session_id.to_string();
            let progress_tx = tx.clone();
            handles.push(tokio::spawn(async move {
                match pool.ensure_backend(&name, &env, &sid).await {
                    Ok(key) => {
                        let tool_count = {
                            let map = pool.backends.read().await;
                            match map.get(&key) {
                                Some(e) => e.tool_count.load(Ordering::Acquire) as usize,
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

    /// Route a tool call to the correct backend.
    ///
    /// Uses backend-entry cached sharing info so we only need the backends
    /// read-lock — no config lock on the hot path.
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        args: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value, RpcError> {
        let map = self.backends.read().await;

        // We need both the backend Arc AND health-sync atomics.
        let (be_arc, original_name, health_sync) = {
            let mut found = None;
            for (key, entry) in map.iter() {
                if !servers.is_empty() && !servers.contains(&key.server) {
                    continue;
                }
                let prefix = format!("{}_", key.server);
                if let Some(original) = name.strip_prefix(&prefix) {
                    let expected = entry.expected_scope(env_overrides, session_id);
                    if key.scope == expected {
                        entry.touch();
                        // Capture atomics for post-call sync (cheap Arc clones).
                        let sync = entry.health_sync();
                        found = Some((entry.backend.clone(), original.to_string(), sync));
                    }
                    break;
                }
            }
            // Drop lock before awaiting the backend mutex.
            drop(map);
            match found {
                Some(f) => f,
                None => return Err(RpcError::ToolNotFound(name.to_string())),
            }
        };

        let mut be = be_arc.lock().await;
        let result = be.call_tool(&original_name, args).await;
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
        // Fast path: full structural equality
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
                if is_server_disabled(name, old_srv) {
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
                if !is_server_disabled(name, srv) && !old_servers.contains_key(name) {
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

        // Phase 1: Hold write lock only to remove/kill old entries (fast).
        let mut removed_keys = Vec::new();
        {
            let mut map = self.backends.write().await;

            // Remove deleted servers (kill all scopes)
            for name in &to_remove {
                let keys: Vec<BackendKey> =
                    map.keys().filter(|k| k.server == *name).cloned().collect();
                for key in keys {
                    if let Some(entry) = map.remove(&key) {
                        entry.backend.lock().await.kill();
                        removed_keys.push(key);
                    }
                }
                info!(server = %name, "removed");
            }

            // Kill the default-scope entry for changed servers
            for name in &to_restart {
                let default_key = BackendKey {
                    server: name.clone(),
                    scope: DEFAULT_ENV_KEY.to_string(),
                };
                if let Some(entry) = map.remove(&default_key) {
                    entry.backend.lock().await.kill();
                    removed_keys.push(default_key);
                }
            }
        } // write lock dropped — reads are unblocked during spawns

        // Phase 2: Start backends concurrently without holding the lock.
        // Collect (name, config) pairs that need a fresh Backend::start.
        let mut to_start: Vec<(String, &ServerConfig)> = Vec::new();

        for name in &to_restart {
            let Some(srv) = new_servers.get(name) else {
                continue;
            };
            if is_server_disabled(name, srv) {
                info!(server = %name, "disabled after reload");
                continue;
            }
            if !matches!(srv.shared, Sharing::Global) {
                continue;
            }
            info!(server = %name, "restarting");
            to_start.push((name.clone(), srv));
        }

        for name in &to_add {
            let Some(srv) = new_servers.get(name) else {
                continue;
            };
            if is_server_disabled(name, srv) {
                continue;
            }
            if !matches!(srv.shared, Sharing::Global) {
                debug!(server = %name, "deferring new server (not Global)");
                continue;
            }
            info!(server = %name, "starting (new in config)");
            to_start.push((name.clone(), srv));
        }

        // Start backends sequentially but WITHOUT the write lock held.
        // This is the critical improvement: reads (tools/list, tools/call) are
        // not blocked while backends spawn and handshake.
        let mut started: Vec<(String, BackendEntry)> = Vec::new();
        for (name, srv) in &to_start {
            match Backend::start(name.clone(), srv, &empty_env).await {
                Ok(be) => {
                    started.push((name.clone(), BackendEntry::new(be, srv)));
                }
                Err(e) => error!(server = %name, "start failed: {e}"),
            }
        }

        // Phase 3: Re-acquire write lock to insert results (fast).
        {
            let mut map = self.backends.write().await;
            for (name, entry) in started {
                let srv = new_servers.get(&name).expect("server vanished from config");
                let env_key = backend_env_key(srv, &empty_env);
                map.insert(
                    BackendKey {
                        server: name,
                        scope: env_key,
                    },
                    entry,
                );
            }
        }

        self.purge_spawn_locks(&removed_keys).await;

        *self.raw_config.write().await = new_config.clone();
        custom_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sharing;

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
