use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default config path: ~/.config/mcp-proxy/servers.json
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("mcp-proxy")
        .join("servers.json")
}

const STARTER_CONFIG: &str = r#"{
  "servers": {},
  "customTools": {},
  "profiles": {}
}
"#;

/// Create a starter config file if it doesn't exist.
/// Returns the path written to, or None if it already exists.
pub fn init_config(path: &Path) -> Result<Option<PathBuf>> {
    if path.exists() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::write(path, STARTER_CONFIG).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path.to_path_buf()))
}

// ---------------------------------------------------------------------------
// Install method (for Docker generation only — not used at runtime)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InstallConfig {
    Npm {
        package: String,
    },
    Pip {
        package: String,
    },
    Binary {
        url: String,
        extract: Option<ExtractMethod>,
        binary: String,
    },
    Npx,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractMethod {
    Tar,
    Zip,
    None,
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// How to install in Docker (optional, only used by `generate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallConfig>,
    /// Command to spawn the MCP server.
    pub command: String,
    /// Arguments passed to the command. Supports `${VAR}` expansion.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables. Values support `${VAR}` expansion.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Env var that can disable this server when set to `false` or `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_toggle: Option<String>,
}

// ---------------------------------------------------------------------------
// Custom tool configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CustomToolConfig {
    Shell {
        description: String,
        #[serde(rename = "inputSchema")]
        input_schema: Value,
        command: String,
        #[serde(rename = "envToggle", skip_serializing_if = "Option::is_none")]
        env_toggle: Option<String>,
    },
    Http {
        description: String,
        #[serde(rename = "inputSchema")]
        input_schema: Value,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
        #[serde(rename = "envToggle", skip_serializing_if = "Option::is_none")]
        env_toggle: Option<String>,
    },
}

impl CustomToolConfig {
    pub fn description(&self) -> &str {
        match self {
            Self::Shell { description, .. } | Self::Http { description, .. } => description,
        }
    }

    pub fn input_schema(&self) -> &Value {
        match self {
            Self::Shell { input_schema, .. } | Self::Http { input_schema, .. } => input_schema,
        }
    }

    pub fn env_toggle(&self) -> Option<&str> {
        match self {
            Self::Shell { env_toggle, .. } | Self::Http { env_toggle, .. } => env_toggle.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Server override (all fields optional — used in profiles)
// ---------------------------------------------------------------------------

/// Partial server config used in profiles. Matches a base server name to
/// override specific fields, or defines a brand-new server (must set `command`).
///
/// Merge rules when overriding a base server:
///   - `command`, `args`, `install`, `envToggle` → replaced if present, inherited if omitted
///   - `env` → shallow-merged (override keys win, base keys preserved)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_toggle: Option<String>,
}

impl ServerOverride {
    /// Merge this override onto a base `ServerConfig`, returning the result.
    fn apply_to(&self, base: &ServerConfig) -> ServerConfig {
        let mut out = base.clone();
        if let Some(ref cmd) = self.command {
            out.command = cmd.clone();
        }
        if let Some(ref args) = self.args {
            out.args = args.clone();
        }
        if let Some(ref env) = self.env {
            // Shallow merge: override keys win, base keys preserved
            for (k, v) in env {
                out.env.insert(k.clone(), v.clone());
            }
        }
        if let Some(ref install) = self.install {
            out.install = Some(install.clone());
        }
        if self.env_toggle.is_some() {
            out.env_toggle.clone_from(&self.env_toggle);
        }
        out
    }

    /// Convert to a full ServerConfig (for new profile-only servers).
    fn into_server_config(self) -> Option<ServerConfig> {
        Some(ServerConfig {
            command: self.command?,
            args: self.args.unwrap_or_default(),
            env: self.env.unwrap_or_default(),
            install: self.install,
            env_toggle: self.env_toggle,
        })
    }
}

// ---------------------------------------------------------------------------
// Profile configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileConfig {
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Servers to include (if set, only these + profile servers are used).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Servers to exclude from the base set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Server overrides and additions. If a key matches a base server name,
    /// the fields are merged onto the base. Otherwise it's a new server
    /// (must have `command`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub servers: HashMap<String, ServerOverride>,
    /// Additional custom tools only available in this profile.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom_tools: HashMap<String, CustomToolConfig>,
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
    #[serde(default)]
    pub custom_tools: HashMap<String, CustomToolConfig>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub profiles: HashMap<String, ProfileConfig>,
}

/// The resolved config after applying a profile.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub servers: HashMap<String, ServerConfig>,
    pub custom_tools: HashMap<String, CustomToolConfig>,
}

/// Load and parse a JSON config file.
pub fn load(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Resolve a profile against the base config.
pub fn resolve(cfg: &Config, profile: Option<&str>) -> Result<ResolvedConfig> {
    let Some(name) = profile else {
        return Ok(ResolvedConfig {
            servers: cfg.servers.clone(),
            custom_tools: cfg.custom_tools.clone(),
        });
    };

    let prof = cfg
        .profiles
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown profile: {name}"))?;

    // 1. Start with base servers, filtered by include/exclude
    let mut servers: HashMap<String, ServerConfig> = if !prof.include.is_empty() {
        // Include list also picks up servers named in the profile's servers block
        cfg.servers
            .iter()
            .filter(|(k, _)| prof.include.iter().any(|i| i == *k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else {
        cfg.servers
            .iter()
            .filter(|(k, _)| !prof.exclude.iter().any(|e| e == *k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    // 2. Apply profile server overrides / additions
    for (srv_name, ovr) in &prof.servers {
        if let Some(base) = cfg.servers.get(srv_name) {
            // Override: merge onto the base server config
            servers.insert(srv_name.clone(), ovr.apply_to(base));
        } else {
            // New server: must have a command
            match ovr.clone().into_server_config() {
                Some(srv) => {
                    servers.insert(srv_name.clone(), srv);
                }
                None => anyhow::bail!(
                    "profile '{name}': server '{srv_name}' is not in base and has no command",
                ),
            }
        }
    }

    // 3. Custom tools: base set (filtered if include is used) + profile additions
    let mut custom_tools = if !prof.include.is_empty() {
        HashMap::new()
    } else {
        cfg.custom_tools.clone()
    };
    for (k, v) in &prof.custom_tools {
        custom_tools.insert(k.clone(), v.clone());
    }

    Ok(ResolvedConfig {
        servers,
        custom_tools,
    })
}

/// List available profile names from a config.
pub fn list_profiles(cfg: &Config) -> Vec<(&str, Option<&str>)> {
    cfg.profiles
        .iter()
        .map(|(name, p)| (name.as_str(), p.description.as_deref()))
        .collect()
}

// ---------------------------------------------------------------------------
// Active profile persistence (~/.mcp-proxy/active-profile)
// ---------------------------------------------------------------------------

fn profile_state_path() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_default().join(".mcp-proxy")
}

pub fn read_active_profile() -> Option<String> {
    let path = profile_state_path().join("active-profile");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_active_profile(profile: Option<&str>) -> Result<()> {
    let dir = profile_state_path();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("active-profile");
    match profile {
        Some(name) => std::fs::write(&path, name)?,
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Expand `${VAR}` references from the process environment.
pub fn expand_env(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(start) = out.find("${") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let key = &out[start + 2..start + end];
        let val = std::env::var(key).unwrap_or_default();
        out = format!("{}{}{}", &out[..start], val, &out[start + end + 1..]);
    }
    out
}

/// Check whether an env-toggle variable says "disabled".
pub fn is_toggled_off(toggle: Option<&str>) -> bool {
    toggle.is_some_and(|name| {
        std::env::var(name)
            .ok()
            .is_some_and(|v| matches!(v.to_lowercase().as_str(), "false" | "0"))
    })
}
