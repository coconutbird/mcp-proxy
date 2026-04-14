//! Backend pool — lifecycle management for MCP backend processes.
//!
//! [`BackendPool`] owns backend processes and manages their lifecycle:
//! spawning, idle reaping, crash recovery, and hot-reload.
//! It does **not** know about custom tools — that concern lives in [`crate::server::Hub`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::backend::{Backend, Tool};
use crate::config::{Config, ServerConfig, Sharing, diff_fields, extract_var_names};
use crate::server::RpcError;

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

/// A single backend instance, keyed by (server_name, relevant_env_hash).
struct BackendEntry {
    backend: Arc<TokioMutex<Backend>>,
    /// `None` = default instance (no env overrides, never reaped).
    last_used: Option<Instant>,
}

impl BackendEntry {
    /// Wrap a backend in an entry. `reapable = true` starts the idle timer;
    /// `false` means the instance is never reaped (e.g. Global sharing).
    fn new(backend: Backend, reapable: bool) -> Self {
        Self {
            backend: Arc::new(TokioMutex::new(backend)),
            last_used: if reapable { Some(Instant::now()) } else { None },
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
            if srv.is_disabled(name) {
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
                        Ok((name, env_key, be))
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
            if let Ok(Ok((name, env_key, be))) = handle.await {
                backends.insert(
                    BackendKey {
                        server: name,
                        scope: env_key,
                    },
                    BackendEntry::new(be, false),
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

        // Fast path: check under a read lock first (cheap, no contention)
        {
            let map = self.backends.read().await;
            if map.contains_key(&key) {
                drop(map);
                let mut map = self.backends.write().await;
                if let Some(entry) = map.get_mut(&key) {
                    if entry.last_used.is_some() {
                        entry.last_used = Some(Instant::now());
                    }
                    return Ok(key);
                }
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

        // Re-check after acquiring the spawn lock.
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

        info!("spawning backend {name} ({env_key})");
        let backend = Backend::start(name.to_string(), &srv, env_overrides).await?;

        let reapable = !matches!(srv.shared, Sharing::Global);
        let mut map = self.backends.write().await;
        map.insert(key.clone(), BackendEntry::new(backend, reapable));
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

    fn expected_key_with(
        cfg: &Config,
        srv_name: &str,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Option<String> {
        let srv = cfg.servers.get(srv_name)?;
        Some(sharing_env_key(srv, env_overrides, session_id))
    }

    /// Return matching backends for the given server filter.
    pub(crate) async fn matched_backends(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<(String, Arc<TokioMutex<Backend>>)> {
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

    /// Route a tool call to the correct backend.
    pub(crate) async fn call_tool(
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

        // 2. Restart changed servers
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
