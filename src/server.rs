use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;

use crate::backend::{Backend, Tool};
use crate::config::{ResolvedConfig, ServerConfig, is_toggled_off};
use crate::custom_tools::CustomTools;

/// Key for a backend pool — hash of env overrides so clients with the
/// same env share backend processes.
fn env_pool_key(env: &HashMap<String, String>) -> String {
    if env.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<_> = env.iter().collect();
    pairs.sort_by_key(|(k, _)| (*k).clone());
    let joined: String = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\0");
    format!("{:x}", md5_hash(&joined))
}

/// Simple non-crypto hash (djb2) for pool keying — no extra dependency.
fn md5_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

/// A pool of backends spawned with a specific set of env overrides.
struct BackendPool {
    backends: HashMap<String, Backend>,
    #[allow(dead_code)]
    env_overrides: HashMap<String, String>,
}

/// The aggregator: holds the default backends + lazily-spawned env pools
/// for clients with custom env overrides.
pub struct Hub {
    /// Default backends (no env overrides).
    pub backends: Arc<RwLock<HashMap<String, Backend>>>,
    /// Per-env-override backend pools, keyed by env hash.
    env_pools: Arc<RwLock<HashMap<String, BackendPool>>>,
    /// Resolved server configs (needed to spawn new pools).
    server_configs: HashMap<String, ServerConfig>,
    pub custom: Arc<CustomTools>,
    pub profile_name: Option<String>,
}

impl Hub {
    /// Boot all configured backends and custom tools from a resolved config.
    pub async fn from_config(config: &ResolvedConfig) -> Result<Self> {
        let empty_env = HashMap::new();
        let mut backends = HashMap::new();

        if let Some(ref p) = config.profile_name {
            eprintln!("profile: {p}");
        }

        for (name, srv) in &config.servers {
            if name.starts_with('_') {
                continue;
            }
            if is_toggled_off(srv.env_toggle.as_deref()) {
                eprintln!("skipping {name} (toggled off)");
                continue;
            }
            eprintln!("starting backend: {name}");
            match Backend::start(name.clone(), srv, &empty_env).await {
                Ok(be) => {
                    backends.insert(name.clone(), be);
                }
                Err(e) => eprintln!("failed to start {name}: {e}"),
            }
        }

        let custom = CustomTools::new(&config.custom_tools);

        Ok(Self {
            backends: Arc::new(RwLock::new(backends)),
            env_pools: Arc::default(),
            server_configs: config.servers.clone(),
            custom: Arc::new(custom),
            profile_name: config.profile_name.clone(),
        })
    }

    /// Collect every tool from every backend + custom tools (default env).
    pub async fn list_tools(&self) -> Vec<Tool> {
        self.list_tools_with_env(&HashMap::new()).await
    }

    /// List tools, using an env-specific pool if overrides are present.
    pub async fn list_tools_with_env(&self, env_overrides: &HashMap<String, String>) -> Vec<Tool> {
        let mut out = Vec::new();

        if env_overrides.is_empty() {
            let backends = self.backends.read().await;
            for be in backends.values() {
                out.extend(be.tools.iter().cloned());
            }
        } else {
            let pool = self.get_or_create_pool(env_overrides).await;
            let pools = self.env_pools.read().await;
            if let Some(p) = pools.get(&pool) {
                for be in p.backends.values() {
                    out.extend(be.tools.iter().cloned());
                }
            }
        }

        out.extend(self.custom.list());
        out
    }

    /// Route a tool call to the right backend or custom tool (default env).
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.call_tool_with_env(name, args, &HashMap::new()).await
    }

    /// Route a tool call with per-client env overrides.
    pub async fn call_tool_with_env(
        &self,
        name: &str,
        args: Value,
        env_overrides: &HashMap<String, String>,
    ) -> Result<Value> {
        // Check custom tools first
        if self.custom.has(name) {
            return self.custom.call(name, &args).await;
        }

        if env_overrides.is_empty() {
            // Use default pool
            let backends = self.backends.read().await;
            for be in backends.values() {
                let prefix = format!("{}_", be.name);
                if let Some(original) = name.strip_prefix(&prefix) {
                    return be.call_tool(original, args).await;
                }
            }
        } else {
            // Use env-specific pool
            let pool_key = self.get_or_create_pool(env_overrides).await;
            let pools = self.env_pools.read().await;
            if let Some(pool) = pools.get(&pool_key) {
                for be in pool.backends.values() {
                    let prefix = format!("{}_", be.name);
                    if let Some(original) = name.strip_prefix(&prefix) {
                        return be.call_tool(original, args).await;
                    }
                }
            }
        }

        anyhow::bail!("unknown tool: {name}");
    }

    /// Get or lazily create a backend pool for the given env overrides.
    /// Returns the pool key.
    async fn get_or_create_pool(&self, env_overrides: &HashMap<String, String>) -> String {
        let key = env_pool_key(env_overrides);

        // Fast path: pool already exists
        {
            let pools = self.env_pools.read().await;
            if pools.contains_key(&key) {
                return key;
            }
        }

        // Slow path: spawn backends with env overrides
        info!(
            "spawning backend pool for env overrides: {:?}",
            env_overrides.keys().collect::<Vec<_>>()
        );

        let mut backends = HashMap::new();
        for (name, srv) in &self.server_configs {
            if name.starts_with('_') {
                continue;
            }
            if is_toggled_off(srv.env_toggle.as_deref()) {
                continue;
            }
            match Backend::start(name.clone(), srv, env_overrides).await {
                Ok(be) => {
                    backends.insert(name.clone(), be);
                }
                Err(e) => eprintln!("failed to start {name} (env pool): {e}"),
            }
        }

        let mut pools = self.env_pools.write().await;
        pools.insert(
            key.clone(),
            BackendPool {
                backends,
                env_overrides: env_overrides.clone(),
            },
        );
        key
    }

    /// Health summary for /health endpoint.
    pub async fn health(&self) -> Value {
        let backends = self.backends.read().await;
        let items: Vec<Value> = backends
            .values()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "ready": b.ready,
                    "tools": b.tools.len(),
                })
            })
            .collect();
        let pool_count = self.env_pools.read().await.len();
        serde_json::json!({
            "backends": items,
            "profile": self.profile_name,
            "envPools": pool_count,
        })
    }

    /// Kill all backend processes (default + all env pools).
    pub async fn shutdown(&self) {
        let mut backends = self.backends.write().await;
        for be in backends.values_mut() {
            be.kill();
        }
        let mut pools = self.env_pools.write().await;
        for pool in pools.values_mut() {
            for be in pool.backends.values_mut() {
                be.kill();
            }
        }
    }
}
