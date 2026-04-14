//! Configuration data model, loading, saving, and environment expansion.
//!
//! The primary config file (`servers.json`) defines the backend servers.
//! A separate `profiles.json` stores client-side profiles with per-server
//! overrides and include lists. A user-level `config.json` persists the
//! active profile selection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default config path: ~/.config/mcp-proxy/servers.json
pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/mcp-proxy/servers.json")
}

const STARTER_CONFIG: &str = include_str!("../config/starter.json");

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
// Install method
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractMethod {
    Tar,
    Zip,
    None,
}

/// Where to run a backend that has an `install` config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Run inside an auto-built Docker container (default).
    #[default]
    Docker,
    /// Install and run directly on the host.
    Local,
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// How to install the server (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallConfig>,
    /// Where to run: `docker` (default) or `local`. Only relevant when
    /// `install` is set — servers without `install` always run locally.
    #[serde(default, skip_serializing_if = "is_default_runtime")]
    pub runtime: Runtime,
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
    /// How this backend is shared across client sessions.
    #[serde(default, skip_serializing_if = "Sharing::is_default")]
    pub shared: Sharing,
    /// Per-tool RPC timeout in seconds (default: 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Idle timeout in seconds before the backend is reaped (default: 900 = 15 min).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// Auto-restart the backend if it crashes (default: true).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub auto_restart: bool,
    /// Maximum restart attempts before giving up (default: 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u32>,
    /// Tool filter: glob patterns to include. If set, only matching tools are exposed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_tools: Vec<String>,
    /// Tool filter: glob patterns to exclude (applied after include).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
    /// Tool aliases: map from alias name → original tool name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tool_aliases: HashMap<String, String>,
}

fn default_true() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}

/// Controls how a backend process is shared across client sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// One instance per client session — fully isolated (default).
    #[default]
    Session,
    /// Shared across sessions with matching credentials for this server.
    /// Two clients with the same GITHUB_TOKEN share one github process.
    Credentials,
    /// Single global instance shared by everyone. No credentials needed.
    Global,
}

impl Sharing {
    fn is_default(&self) -> bool {
        matches!(self, Sharing::Session)
    }
}

fn is_default_runtime(r: &Runtime) -> bool {
    matches!(r, Runtime::Docker)
}

// ---------------------------------------------------------------------------
// Custom tool configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_toggle: Option<String>,
}

// ---------------------------------------------------------------------------
// Client-side profile configuration
// ---------------------------------------------------------------------------

/// Profiles are client-side: they tell the bridge which servers to request
/// from the hub and what env overrides to send. The server never sees profile
/// names — it just receives `X-MCP-Servers` and `X-MCP-Env` headers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileConfig {
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which servers to request from the hub (empty = all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Per-server env overrides sent to the hub via X-MCP-Env.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub servers: HashMap<String, ServerOverride>,
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
    #[serde(default)]
    pub custom_tools: HashMap<String, CustomToolConfig>,
}

/// Load and parse the servers config file.
pub fn load(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Save the servers config back to disk (pretty-printed JSON).
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{json}\n")).with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Profiles file (separate from servers.json)
// ---------------------------------------------------------------------------

/// Wrapper for the profiles.json file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfilesFile {
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

/// Derive the profiles.json path from the servers.json path (same directory).
pub fn profiles_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("profiles.json")
}

/// Load profiles from profiles.json. Returns empty map if file doesn't exist.
pub fn load_profiles(config_path: &Path) -> Result<ProfilesFile> {
    let path = profiles_path(config_path);
    if !path.exists() {
        return Ok(ProfilesFile::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Save profiles to profiles.json.
pub fn save_profiles(config_path: &Path, profiles: &ProfilesFile) -> Result<()> {
    let path = profiles_path(config_path);
    let json = serde_json::to_string_pretty(profiles)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("writing {}", path.display()))
}

/// List available profile names.
pub fn list_profiles(profiles: &ProfilesFile) -> Vec<(&str, Option<&str>)> {
    profiles
        .profiles
        .iter()
        .map(|(name, p)| (name.as_str(), p.description.as_deref()))
        .collect()
}

// ---------------------------------------------------------------------------
// User settings (~/.config/mcp-proxy/config.json)
// ---------------------------------------------------------------------------

/// User-level settings stored in config.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserConfig {
    /// Default profile to use when none is specified via CLI or env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
}

fn user_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/mcp-proxy/config.json")
}

pub fn load_user_config() -> UserConfig {
    let path = user_config_path();
    if !path.exists() {
        return UserConfig::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_user_config(cfg: &UserConfig) -> Result<()> {
    let path = user_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn read_active_profile() -> Option<String> {
    load_user_config().default_profile
}

pub fn write_active_profile(profile: Option<&str>) -> Result<()> {
    let mut cfg = load_user_config();
    cfg.default_profile = profile.map(String::from);
    save_user_config(&cfg)
}

/// Expand `${VAR}` references, checking `overrides` first, then process env.
pub fn expand_env_with_overrides(s: &str, overrides: &HashMap<String, String>) -> String {
    let mut out = s.to_string();
    while let Some(start) = out.find("${") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let key = &out[start + 2..start + end];
        let val = overrides
            .get(key)
            .cloned()
            .unwrap_or_else(|| std::env::var(key).unwrap_or_default());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_no_vars() {
        let out = expand_env_with_overrides("plain text", &HashMap::new());
        assert_eq!(out, "plain text");
    }

    #[test]
    fn expand_env_with_override() {
        let mut ovr = HashMap::new();
        ovr.insert("MY_VAR".to_string(), "hello".to_string());
        let out = expand_env_with_overrides("prefix_${MY_VAR}_suffix", &ovr);
        assert_eq!(out, "prefix_hello_suffix");
    }

    #[test]
    fn expand_env_multiple_vars() {
        let mut ovr = HashMap::new();
        ovr.insert("A".to_string(), "1".to_string());
        ovr.insert("B".to_string(), "2".to_string());
        let out = expand_env_with_overrides("${A}+${B}", &ovr);
        assert_eq!(out, "1+2");
    }

    #[test]
    fn expand_env_missing_var_is_empty() {
        // Ensure the var doesn't exist in process env
        unsafe { std::env::remove_var("__TEST_MISSING_VAR__") };
        let out = expand_env_with_overrides("${__TEST_MISSING_VAR__}", &HashMap::new());
        assert_eq!(out, "");
    }

    #[test]
    fn is_toggled_off_none() {
        assert!(!is_toggled_off(None));
    }

    #[test]
    fn is_toggled_off_false() {
        unsafe { std::env::set_var("__TEST_TOGGLE_F__", "false") };
        assert!(is_toggled_off(Some("__TEST_TOGGLE_F__")));
        unsafe { std::env::remove_var("__TEST_TOGGLE_F__") };
    }

    #[test]
    fn is_toggled_off_zero() {
        unsafe { std::env::set_var("__TEST_TOGGLE_Z__", "0") };
        assert!(is_toggled_off(Some("__TEST_TOGGLE_Z__")));
        unsafe { std::env::remove_var("__TEST_TOGGLE_Z__") };
    }

    #[test]
    fn is_toggled_off_true_means_on() {
        unsafe { std::env::set_var("__TEST_TOGGLE_T__", "true") };
        assert!(!is_toggled_off(Some("__TEST_TOGGLE_T__")));
        unsafe { std::env::remove_var("__TEST_TOGGLE_T__") };
    }

    #[test]
    fn sharing_default_is_session() {
        assert_eq!(Sharing::default(), Sharing::Session);
    }

    #[test]
    fn sharing_serde_roundtrip() {
        let json = r#""credentials""#;
        let s: Sharing = serde_json::from_str(json).unwrap();
        assert_eq!(s, Sharing::Credentials);
        let back = serde_json::to_string(&s).unwrap();
        assert_eq!(back, json);
    }
}
