//! Idle backend reaper.
//!
//! A single pass:
//! 1. Snapshots per-server `idle_timeout_secs` from config.
//! 2. Collects candidates under a read lock.
//! 3. Re-checks each candidate under the write lock before removing, so a
//!    concurrent touch still saves the entry.
//! 4. Kills outside the map lock, then purges its spawn lock.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::info;

use super::DEFAULT_IDLE_TIMEOUT;
use super::entry::{BackendEntry, BackendKey, SpawnLocks};
use crate::backend::Backend;
use crate::config::Config;

/// Run one reap pass. See the module docs for the ordering contract.
pub(super) async fn reap_once(
    backends: &Arc<RwLock<HashMap<BackendKey, BackendEntry>>>,
    raw_config: &Arc<RwLock<Config>>,
    spawn_locks: &SpawnLocks,
) {
    let timeouts: HashMap<String, u64> = {
        let cfg = raw_config.read().await;
        cfg.servers
            .iter()
            .map(|(n, s)| {
                (
                    n.clone(),
                    s.idle_timeout_secs
                        .unwrap_or(DEFAULT_IDLE_TIMEOUT.as_secs()),
                )
            })
            .collect()
    };
    let timeout_for = |name: &str| {
        timeouts
            .get(name)
            .copied()
            .unwrap_or(DEFAULT_IDLE_TIMEOUT.as_secs())
    };

    let candidates: Vec<BackendKey> = {
        let map = backends.read().await;
        map.iter()
            .filter_map(|(key, entry)| {
                let idle = entry.idle_secs()?;
                if idle > timeout_for(&key.server) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect()
    };

    if candidates.is_empty() {
        return;
    }

    // Re-check each candidate under the write lock; an entry touched since
    // the scan above should NOT be reaped.
    let to_kill: Vec<(BackendKey, Arc<TokioMutex<Backend>>)> = {
        let mut map = backends.write().await;
        let mut kills = Vec::new();
        for key in &candidates {
            let still_stale = match map.get(key) {
                Some(entry) => match entry.idle_secs() {
                    Some(idle) => idle > timeout_for(&key.server),
                    None => false,
                },
                None => false,
            };
            if still_stale && let Some(entry) = map.remove(key) {
                kills.push((key.clone(), entry.backend));
            }
        }
        kills
    };

    for (key, be_arc) in &to_kill {
        be_arc.lock().await.kill();
        info!(server = %key.server, scope = %key.scope, "reaped idle backend");
    }

    let mut locks = spawn_locks.lock().await;
    for (key, _) in &to_kill {
        locks.remove(key);
    }
}
