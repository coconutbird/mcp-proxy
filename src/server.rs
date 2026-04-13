use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;

use crate::backend::{Backend, Tool};
use crate::config::{self, Config, ResolvedConfig, is_toggled_off};
use crate::custom_tools::CustomTools;

/// A pool of backends spawned for a specific profile + env combination.
struct BackendPool {
    backends: HashMap<String, Backend>,
    custom: Arc<CustomTools>,
    #[allow(dead_code)]
    profile: Option<String>,
}

/// Composite key: profile name + env overrides hash.
fn pool_key(profile: Option<&str>, env: &HashMap<String, String>) -> String {
    let p = profile.unwrap_or("__default__");
    if env.is_empty() {
        return p.to_string();
    }
    let mut pairs: Vec<_> = env.iter().collect();
    pairs.sort_by_key(|(k, _)| (*k).clone());
    let env_part: String = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\0");
    format!("{p}:{:x}", djb2_hash(&env_part))
}

fn djb2_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

/// Boot backends from a resolved config, optionally layering env overrides.
async fn spawn_backends(
    resolved: &ResolvedConfig,
    env_overrides: &HashMap<String, String>,
) -> (HashMap<String, Backend>, Arc<CustomTools>) {
    let mut backends = HashMap::new();
    for (name, srv) in &resolved.servers {
        if name.starts_with('_') {
            continue;
        }
        if is_toggled_off(srv.env_toggle.as_deref()) {
            eprintln!("  skipping {name} (toggled off)");
            continue;
        }
        eprintln!("  starting backend: {name}");
        match Backend::start(name.clone(), srv, env_overrides).await {
            Ok(be) => {
                backends.insert(name.clone(), be);
            }
            Err(e) => eprintln!("  failed to start {name}: {e}"),
        }
    }
    let custom = Arc::new(CustomTools::new(&resolved.custom_tools));
    (backends, custom)
}

/// The aggregator: holds backend pools keyed by (profile, env) and
/// lazily spawns new pools when a client requests a different profile
/// or provides env overrides.
pub struct Hub {
    /// Full raw config — needed to resolve arbitrary profiles on demand.
    raw_config: Config,
    /// Default profile name the hub was started with.
    pub default_profile: Option<String>,
    /// Backend pools keyed by (profile + env hash).
    pools: Arc<RwLock<HashMap<String, BackendPool>>>,
}

impl Hub {
    /// Boot the hub with a full config. The default pool uses `profile`.
    pub async fn new(raw_config: Config, profile: Option<&str>) -> Result<Self> {
        if let Some(p) = profile {
            eprintln!("default profile: {p}");
        }

        let resolved = config::resolve(&raw_config, profile)?;
        let empty_env = HashMap::new();
        let key = pool_key(profile, &empty_env);

        eprintln!("starting default pool ({key}):");
        let (backends, custom) = spawn_backends(&resolved, &empty_env).await;

        let mut pools = HashMap::new();
        pools.insert(
            key,
            BackendPool {
                backends,
                custom,
                profile: profile.map(String::from),
            },
        );

        Ok(Self {
            raw_config,
            default_profile: profile.map(String::from),
            pools: Arc::new(RwLock::new(pools)),
        })
    }

    /// Ensure a pool exists for the given profile + env, spawning if needed.
    /// Returns the pool key.
    async fn ensure_pool(
        &self,
        profile: Option<&str>,
        env_overrides: &HashMap<String, String>,
    ) -> Result<String> {
        let key = pool_key(profile, env_overrides);

        // Fast path
        {
            let pools = self.pools.read().await;
            if pools.contains_key(&key) {
                return Ok(key);
            }
        }

        // Slow path: resolve profile + spawn
        let effective_profile = profile.or(self.default_profile.as_deref());
        info!(
            "spawning pool: profile={effective_profile:?}, env_keys={:?}",
            env_overrides.keys().collect::<Vec<_>>()
        );

        let resolved = config::resolve(&self.raw_config, effective_profile)?;
        let (backends, custom) = spawn_backends(&resolved, env_overrides).await;

        let mut pools = self.pools.write().await;
        pools.insert(
            key.clone(),
            BackendPool {
                backends,
                custom,
                profile: effective_profile.map(String::from),
            },
        );
        Ok(key)
    }

    /// Resolve which pool to use: explicit profile > default.
    fn effective_profile<'a>(&'a self, requested: Option<&'a str>) -> Option<&'a str> {
        requested.or(self.default_profile.as_deref())
    }

    /// List tools for a given profile + env combination.
    pub async fn list_tools_for(
        &self,
        profile: Option<&str>,
        env_overrides: &HashMap<String, String>,
    ) -> Vec<Tool> {
        let effective = self.effective_profile(profile);
        let key = match self.ensure_pool(effective, env_overrides).await {
            Ok(k) => k,
            Err(e) => {
                eprintln!("pool error: {e}");
                return Vec::new();
            }
        };
        let pools = self.pools.read().await;
        let Some(pool) = pools.get(&key) else {
            return Vec::new();
        };
        let mut out: Vec<Tool> = pool
            .backends
            .values()
            .flat_map(|be| be.tools.iter().cloned())
            .collect();
        out.extend(pool.custom.list());
        out
    }

    /// Route a tool call for a given profile + env combination.
    pub async fn call_tool_for(
        &self,
        name: &str,
        args: Value,
        profile: Option<&str>,
        env_overrides: &HashMap<String, String>,
    ) -> Result<Value> {
        let effective = self.effective_profile(profile);
        let key = self.ensure_pool(effective, env_overrides).await?;
        let pools = self.pools.read().await;
        let pool = pools
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("pool not found: {key}"))?;

        // Check custom tools first
        if pool.custom.has(name) {
            return pool.custom.call(name, &args).await;
        }

        // Find backend by prefix
        for be in pool.backends.values() {
            let prefix = format!("{}_", be.name);
            if let Some(original) = name.strip_prefix(&prefix) {
                return be.call_tool(original, args).await;
            }
        }

        anyhow::bail!("unknown tool: {name}");
    }

    // -- Convenience wrappers for default profile, no env overrides --

    pub async fn list_tools(&self) -> Vec<Tool> {
        self.list_tools_for(None, &HashMap::new()).await
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.call_tool_for(name, args, None, &HashMap::new()).await
    }

    /// Health summary for /health endpoint.
    pub async fn health(&self) -> Value {
        let pools = self.pools.read().await;
        let pool_summaries: Vec<Value> = pools
            .iter()
            .map(|(key, pool)| {
                let backends: Vec<Value> = pool
                    .backends
                    .values()
                    .map(|b| {
                        serde_json::json!({
                            "name": b.name,
                            "ready": b.ready,
                            "tools": b.tools.len(),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "pool": key,
                    "profile": pool.profile,
                    "backends": backends,
                })
            })
            .collect();
        serde_json::json!({
            "defaultProfile": self.default_profile,
            "pools": pool_summaries,
        })
    }

    /// Kill all backend processes across all pools.
    pub async fn shutdown(&self) {
        let mut pools = self.pools.write().await;
        for pool in pools.values_mut() {
            for be in pool.backends.values_mut() {
                be.kill();
            }
        }
    }
}
