//! Per-backend pool entry: the live backend handle, atomic health snapshot,
//! and idle-reap timestamp. Also the `BackendKey` / `SpawnLocks` types that
//! key the pool's internal map.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as TokioMutex;

use super::env_key::{DEFAULT_ENV_KEY, ENV_KEY_PREFIX, SESSION_KEY_PREFIX, relevant_env_keys};
use crate::backend::Backend;
use crate::config::{ServerConfig, Sharing};

/// Epoch for atomic timestamps — `last_used` stores elapsed millis from this base.
static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Convert an `Instant` to milliseconds since [`EPOCH`] (for atomic storage).
fn instant_to_millis(t: Instant) -> u64 {
    t.duration_since(*EPOCH).as_millis() as u64
}

/// Convert stored millis back to an `Instant`.
fn millis_to_instant(ms: u64) -> Instant {
    *EPOCH + Duration::from_millis(ms)
}

/// Sentinel value meaning "not reapable" (never reaped). Must NOT collide
/// with any real timestamp — the first entry spawned in a process can have
/// `last_used_ms == 0` because `LazyLock<EPOCH>` initializes inside
/// `instant_to_millis` *after* the argument `Instant::now()` was captured,
/// producing a zero delta.
const NOT_REAPABLE: u64 = u64::MAX;

/// Composite pool key: server name + hash of only the env vars that server
/// cares about (or a session prefix).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BackendKey {
    pub(super) server: String,
    pub(super) scope: String,
}

/// Per-key mutex set used to serialize duplicate-spawn attempts.
pub(super) type SpawnLocks = Arc<TokioMutex<HashMap<BackendKey, Arc<TokioMutex<()>>>>>;

/// A single backend instance.
///
/// `last_used_ms` is atomic so hot-path lookups can bump it under a *read*
/// lock instead of requiring a write lock per request. [`NOT_REAPABLE`] means
/// the entry is never reaped.
///
/// `sharing` and `env_key_vars` are cached from the server config at spawn
/// time so scope matching on the hot path doesn't need the config lock.
pub(super) struct BackendEntry {
    pub(super) backend: Arc<TokioMutex<Backend>>,
    last_used_ms: AtomicU64,
    sharing: Sharing,
    env_key_vars: Vec<String>,
    // --- Atomic health snapshot (lock-free reads for /health) ---
    pub(super) tool_count: Arc<AtomicU64>,
    pub(super) ready: Arc<AtomicBool>,
    pub(super) restart_count: Arc<AtomicU64>,
    pub(super) crashed: Arc<AtomicBool>,
}

impl BackendEntry {
    /// Wrap a backend in an entry. Entries for [`Sharing::Global`] servers
    /// are marked non-reapable; all others start an idle timer.
    pub(super) fn new(backend: Backend, srv: &ServerConfig) -> Self {
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
    pub(super) fn health_sync(&self) -> HealthSync {
        HealthSync {
            ready: self.ready.clone(),
            tool_count: self.tool_count.clone(),
            restart_count: self.restart_count.clone(),
        }
    }

    /// Touch the entry (reset idle timer). No-op for non-reapable entries.
    pub(super) fn touch(&self) {
        if self.is_reapable() {
            self.last_used_ms
                .store(instant_to_millis(Instant::now()), Ordering::Relaxed);
        }
    }

    /// Seconds since last use. Returns `None` for non-reapable entries.
    pub(super) fn idle_secs(&self) -> Option<u64> {
        let ms = self.last_used_ms.load(Ordering::Relaxed);
        if ms == NOT_REAPABLE {
            return None;
        }
        Some(millis_to_instant(ms).elapsed().as_secs())
    }

    fn is_reapable(&self) -> bool {
        self.last_used_ms.load(Ordering::Relaxed) != NOT_REAPABLE
    }

    /// Compute the expected scope key for this entry using cached sharing
    /// info — no config lock required.
    pub(super) fn expected_scope(
        &self,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> String {
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
                format!("{ENV_KEY_PREFIX}{:x}", crate::util::fnv1a(&s))
            }
        }
    }
}

/// Lightweight handle for syncing backend health atomics after a tool call.
/// Cloning `Arc<AtomicX>` is cheap and avoids holding a reference to the entry.
pub(super) struct HealthSync {
    ready: Arc<AtomicBool>,
    tool_count: Arc<AtomicU64>,
    restart_count: Arc<AtomicU64>,
}

impl HealthSync {
    pub(super) fn sync(&self, be: &Backend) {
        self.ready.store(be.ready, Ordering::Release);
        self.tool_count
            .store(be.tools.len() as u64, Ordering::Release);
        self.restart_count
            .store(be.restart_count as u64, Ordering::Release);
    }
}
