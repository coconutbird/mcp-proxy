use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;

use crate::backend::{Backend, Tool};
use crate::config::{Config, ServerConfig, Sharing, is_toggled_off};
use crate::custom_tools::CustomTools;

/// How long a non-default backend instance can sit idle before being reaped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A single backend instance, keyed by (server_name, relevant_env_hash).
struct BackendEntry {
    backend: Backend,
    /// `None` = default instance (no env overrides, never reaped).
    last_used: Option<Instant>,
}

/// Composite key: server name + hash of only the env vars that server cares about.
type BackendKey = (String, String);

fn djb2_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

/// Extract the `${VAR}` references from a server config's env values and args.
fn relevant_env_keys(srv: &ServerConfig) -> Vec<String> {
    let mut keys = Vec::new();
    for val in srv.env.values() {
        extract_var_refs(val, &mut keys);
    }
    for arg in &srv.args {
        extract_var_refs(arg, &mut keys);
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Pull `${VAR}` names out of a string.
fn extract_var_refs(s: &str, out: &mut Vec<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            out.push(after[..end].to_string());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
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
        return "__default__".to_string();
    }
    let s: String = relevant
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\0");
    format!("env:{:x}", djb2_hash(&s))
}

/// The aggregator: holds individual backend instances keyed by
/// (server_name, relevant_env_hash). Two clients that share the same
/// credentials for a given server will reuse the same backend process.
pub struct Hub {
    raw_config: Config,
    /// Individual backend instances keyed by (name, env_hash).
    backends: Arc<RwLock<HashMap<BackendKey, BackendEntry>>>,
    custom: Arc<CustomTools>,
}

impl Hub {
    /// Boot the hub: spawns all default backend instances and starts
    /// a background reaper that kills idle instances every minute.
    pub async fn new(raw_config: Config) -> Result<Self> {
        let empty_env = HashMap::new();
        let mut backends = HashMap::new();

        eprintln!("starting backends:");
        for (name, srv) in &raw_config.servers {
            if name.starts_with('_') {
                continue;
            }
            if is_toggled_off(srv.env_toggle.as_deref()) {
                eprintln!("  skipping {name} (toggled off)");
                continue;
            }
            match srv.shared {
                // Per-session: never start eagerly — spawned on connect.
                Sharing::Session => {
                    eprintln!("  deferring {name} (per-session)");
                    continue;
                }
                // Per-credential: only start if env vars are available now.
                Sharing::Credentials => {
                    let required = relevant_env_keys(srv);
                    let missing: Vec<&str> = required
                        .iter()
                        .filter(|k| std::env::var(k).is_err())
                        .map(|s| s.as_str())
                        .collect();
                    if !missing.is_empty() {
                        eprintln!("  deferring {name} (needs: {})", missing.join(", "));
                        continue;
                    }
                }
                // Global: always start eagerly.
                Sharing::Global => {}
            }
            let env_key = backend_env_key(srv, &empty_env);
            eprintln!("  starting {name}");
            match Backend::start(name.clone(), srv, &empty_env).await {
                Ok(be) => {
                    backends.insert(
                        (name.clone(), env_key),
                        BackendEntry {
                            backend: be,
                            last_used: None,
                        },
                    );
                }
                Err(e) => eprintln!("  failed to start {name}: {e}"),
            }
        }

        let custom = Arc::new(CustomTools::new(&raw_config.custom_tools));
        let backends = Arc::new(RwLock::new(backends));

        // Background reaper: every 60s, kill instances idle > 15 min
        let reaper = backends.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut map = reaper.write().await;
                let stale: Vec<BackendKey> = map
                    .iter()
                    .filter_map(|(key, entry)| {
                        let last = entry.last_used?;
                        if last.elapsed() > IDLE_TIMEOUT {
                            Some(key.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                for key in stale {
                    if let Some(mut entry) = map.remove(&key) {
                        entry.backend.kill();
                        eprintln!("reaped idle backend: {} ({})", key.0, key.1);
                    }
                }
            }
        });

        Ok(Self {
            raw_config,
            backends,
            custom,
        })
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
        let srv = self
            .raw_config
            .servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown server: {name}"))?;

        let env_key = match srv.shared {
            Sharing::Global => "__default__".to_string(),
            Sharing::Credentials => backend_env_key(srv, env_overrides),
            Sharing::Session => format!("session:{session_id}"),
        };
        let key: BackendKey = (name.to_string(), env_key.clone());

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

        // Fail fast: check all required env vars are resolvable.
        let required = relevant_env_keys(srv);
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
        let backend = Backend::start(name.to_string(), srv, env_overrides).await?;

        // Global backends are never reaped; others have an idle timer.
        let last_used = match srv.shared {
            Sharing::Global => None,
            _ => Some(Instant::now()),
        };

        let mut map = self.backends.write().await;
        map.insert(key.clone(), BackendEntry { backend, last_used });
        Ok(key)
    }

    /// Ensure all requested backends exist (or all if `servers` is empty).
    async fn ensure_backends(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) {
        let names: Vec<String> = if servers.is_empty() {
            self.raw_config
                .servers
                .keys()
                .filter(|n| !n.starts_with('_'))
                .filter(|n| !is_toggled_off(self.raw_config.servers[*n].env_toggle.as_deref()))
                .cloned()
                .collect()
        } else {
            servers.to_vec()
        };
        for name in &names {
            if let Err(e) = self.ensure_backend(name, env_overrides, session_id).await {
                eprintln!("failed to ensure backend {name}: {e}");
            }
        }
    }

    /// Compute the expected backend key for a server given the client context.
    fn expected_key(
        &self,
        srv_name: &str,
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Option<String> {
        let srv = self.raw_config.servers.get(srv_name)?;
        Some(match srv.shared {
            Sharing::Global => "__default__".to_string(),
            Sharing::Credentials => backend_env_key(srv, env_overrides),
            Sharing::Session => format!("session:{session_id}"),
        })
    }

    /// List tools, filtered by the client's server include list.
    /// Empty `servers` = all servers.
    pub async fn list_tools_for(
        &self,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Vec<Tool> {
        self.ensure_backends(servers, env_overrides, session_id)
            .await;

        let map = self.backends.read().await;
        let mut out: Vec<Tool> = Vec::new();

        for ((srv_name, env_key), entry) in map.iter() {
            let be = &entry.backend;
            if !servers.is_empty() && !servers.contains(srv_name) {
                continue;
            }
            let expected = match self.expected_key(srv_name, env_overrides, session_id) {
                Some(k) => k,
                None => continue,
            };
            if *env_key != expected {
                continue;
            }
            out.extend(be.tools.iter().cloned());
        }
        out.extend(self.custom.list());
        out
    }

    /// Route a tool call, respecting the client's server include list.
    pub async fn call_tool_for(
        &self,
        name: &str,
        args: Value,
        servers: &[String],
        env_overrides: &HashMap<String, String>,
        session_id: &str,
    ) -> Result<Value> {
        // Check custom tools first
        if self.custom.has(name) {
            return self.custom.call(name, &args).await;
        }

        // Find backend by prefix, checking include list
        let map = self.backends.read().await;
        for ((srv_name, env_key), entry) in map.iter() {
            let be = &entry.backend;
            if !servers.is_empty() && !servers.contains(srv_name) {
                continue;
            }
            let expected = match self.expected_key(srv_name, env_overrides, session_id) {
                Some(k) => k,
                None => continue,
            };
            if *env_key != expected {
                continue;
            }
            let prefix = format!("{}_", be.name);
            if let Some(original) = name.strip_prefix(&prefix) {
                return be.call_tool(original, args).await;
            }
        }

        anyhow::bail!("unknown tool: {name}");
    }

    // -- Convenience wrappers: all servers, no env overrides, stdio session --

    pub async fn list_tools(&self) -> Vec<Tool> {
        self.list_tools_for(&[], &HashMap::new(), "__stdio__").await
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.call_tool_for(name, args, &[], &HashMap::new(), "__stdio__")
            .await
    }

    /// Health summary for /health endpoint.
    pub async fn health(&self) -> Value {
        let map = self.backends.read().await;
        let entries: Vec<Value> = map
            .iter()
            .map(|((name, env_key), entry)| {
                serde_json::json!({
                    "name": name,
                    "env_key": env_key,
                    "ready": entry.backend.ready,
                    "tools": entry.backend.tools.len(),
                    "idle_secs": entry.last_used.map(|t| t.elapsed().as_secs()),
                })
            })
            .collect();
        serde_json::json!({ "backends": entries })
    }

    /// Kill all backends belonging to a specific session.
    pub async fn cleanup_session(&self, session_id: &str) {
        let session_key = format!("session:{session_id}");
        let mut map = self.backends.write().await;
        let stale: Vec<BackendKey> = map
            .keys()
            .filter(|(_, env_key)| *env_key == session_key)
            .cloned()
            .collect();
        for key in &stale {
            if let Some(mut entry) = map.remove(key) {
                info!("cleaning up {} for session {session_id}", key.0);
                entry.backend.kill();
            }
        }
    }

    /// Kill all backend processes.
    pub async fn shutdown(&self) {
        let mut map = self.backends.write().await;
        for entry in map.values_mut() {
            entry.backend.kill();
        }
    }
}
