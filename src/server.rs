use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::backend::{Backend, Tool};
use crate::config::{ResolvedConfig, is_toggled_off};
use crate::custom_tools::CustomTools;

/// The aggregator: holds all backends + custom tools, routes requests.
pub struct Hub {
    pub backends: Arc<RwLock<HashMap<String, Backend>>>,
    pub custom: Arc<CustomTools>,
    pub profile_name: Option<String>,
}

impl Hub {
    /// Boot all configured backends and custom tools from a resolved config.
    pub async fn from_config(config: &ResolvedConfig) -> Result<Self> {
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
            match Backend::start(name.clone(), srv).await {
                Ok(be) => {
                    backends.insert(name.clone(), be);
                }
                Err(e) => eprintln!("failed to start {name}: {e}"),
            }
        }

        let custom = CustomTools::new(&config.custom_tools);

        Ok(Self {
            backends: Arc::new(RwLock::new(backends)),
            custom: Arc::new(custom),
            profile_name: config.profile_name.clone(),
        })
    }

    /// Collect every tool from every backend + custom tools.
    pub async fn list_tools(&self) -> Vec<Tool> {
        let mut out = Vec::new();
        let backends = self.backends.read().await;
        for be in backends.values() {
            out.extend(be.tools.iter().cloned());
        }
        out.extend(self.custom.list());
        out
    }

    /// Route a tool call to the right backend or custom tool.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        // Check custom tools first
        if self.custom.has(name) {
            return self.custom.call(name, &args).await;
        }

        // Find the backend by prefix match
        let backends = self.backends.read().await;
        for be in backends.values() {
            let prefix = format!("{}_", be.name);
            if let Some(original) = name.strip_prefix(&prefix) {
                return be.call_tool(original, args).await;
            }
        }

        anyhow::bail!("unknown tool: {name}");
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
        serde_json::json!({
            "backends": items,
            "profile": self.profile_name,
        })
    }

    /// Kill all backend processes.
    pub async fn shutdown(&self) {
        let mut backends = self.backends.write().await;
        for be in backends.values_mut() {
            be.kill();
        }
    }
}
