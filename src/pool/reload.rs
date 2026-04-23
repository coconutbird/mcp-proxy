//! Hot-reload: diff old vs new config, kill removed/changed Global backends,
//! re-spawn them, and leave session/credentials backends to respawn lazily.
//!
//! Session- and Credentials-scoped backends are NOT restarted here — they'll
//! be re-spawned on demand when the next request comes in. Only Global
//! backends are restarted eagerly, because they must exist at pool boot.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, error, info};

use super::entry::{BackendEntry, BackendKey, SpawnLocks};
use super::env_key::{DEFAULT_ENV_KEY, backend_env_key};
use super::purge_spawn_locks;
use crate::backend::Backend;
use crate::config::{Config, ServerConfig, Sharing, diff_fields, is_server_disabled};

/// Apply a new config to the pool. Returns whether custom tools changed
/// (the hub uses this to decide whether to rebuild its custom tool set).
pub(super) async fn reload_backends(
    backends: &Arc<RwLock<HashMap<BackendKey, BackendEntry>>>,
    raw_config: &Arc<RwLock<Config>>,
    spawn_locks: &SpawnLocks,
    new_config: &Config,
) -> bool {
    // Fast path: full structural equality.
    {
        let old_cfg = raw_config.read().await;
        if *old_cfg == *new_config {
            debug!("config file re-read, no changes");
            return false;
        }
    }

    let empty_env = HashMap::new();

    let (to_remove, to_restart, to_add, custom_changed) = {
        let old_cfg = raw_config.read().await;
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
        *raw_config.write().await = new_config.clone();
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

    // Phase 1: write lock only long enough to remove/kill old entries.
    let mut removed_keys = Vec::new();
    {
        let mut map = backends.write().await;

        // Remove deleted servers (kill all scopes).
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

        // Kill the default-scope entry for changed servers.
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

    // Phase 2: start backends without holding the map lock.
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

    // Sequential spawn is fine here — reads are unblocked during handshake.
    let mut started: Vec<(String, BackendEntry)> = Vec::new();
    for (name, srv) in &to_start {
        match Backend::start(name.clone(), srv, &empty_env).await {
            Ok(be) => {
                started.push((name.clone(), BackendEntry::new(be, srv)));
            }
            Err(e) => error!(server = %name, "start failed: {e}"),
        }
    }

    // Phase 3: re-acquire write lock to insert results (fast).
    {
        let mut map = backends.write().await;
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

    purge_spawn_locks(spawn_locks, &removed_keys).await;

    *raw_config.write().await = new_config.clone();
    custom_changed
}
