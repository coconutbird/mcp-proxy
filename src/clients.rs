//! Editor/client auto-installation support.
//!
//! Detects known MCP clients (Claude Desktop, Claude CLI, Augment) and
//! can install or remove the `mcp-proxy` bridge entry from their config files.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

/// Known MCP client applications.
pub struct ClientDef {
    pub name: &'static str,
    pub config_path: PathBuf,
    pub mcp_key: &'static str,
    /// If true, the client is considered "available" when the parent directory
    /// exists (e.g. Claude Desktop creates the file on first launch). If false,
    /// the config file itself must exist.
    pub check_parent_dir: bool,
}

pub fn known_clients() -> Vec<ClientDef> {
    let home = dirs::home_dir().unwrap_or_default();
    let os = std::env::consts::OS;
    vec![
        ClientDef {
            name: "Claude Desktop",
            config_path: match os {
                "macos" => {
                    home.join("Library/Application Support/Claude/claude_desktop_config.json")
                }
                "windows" => home.join("AppData/Roaming/Claude/claude_desktop_config.json"),
                _ => home.join(".config/Claude/claude_desktop_config.json"),
            },
            mcp_key: "mcpServers",
            check_parent_dir: true,
        },
        ClientDef {
            name: "Claude CLI",
            config_path: home.join(".claude.json"),
            mcp_key: "mcpServers",
            check_parent_dir: false,
        },
        ClientDef {
            name: "Augment",
            config_path: home.join(".augment/settings.json"),
            mcp_key: "mcpServers",
            check_parent_dir: false,
        },
    ]
}

pub fn is_available(client: &ClientDef) -> bool {
    if client.check_parent_dir {
        client.config_path.parent().is_some_and(|p| p.exists())
    } else {
        client.config_path.exists()
    }
}

pub fn read_config(client: &ClientDef) -> Option<Value> {
    let raw = std::fs::read_to_string(&client.config_path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_config(client: &ClientDef, cfg: &Value) -> Result<()> {
    let mut out = serde_json::to_string_pretty(cfg)?;
    out.push('\n'); // consistent trailing newline (matches config::save_json)
    std::fs::write(&client.config_path, out)?;
    Ok(())
}

pub fn is_installed(client: &ClientDef) -> bool {
    read_config(client).is_some_and(|cfg| {
        cfg.get(client.mcp_key)
            .and_then(Value::as_object)
            .is_some_and(|m| m.contains_key("mcp-proxy"))
    })
}

/// Build the MCP server entry to write into a client config.
/// Always uses the bridge (stdio) so session lifecycle, env forwarding,
/// and clean disconnect all work correctly.
pub fn mcp_entry(_client: &ClientDef, self_exe: &str, profile: Option<&str>) -> Value {
    let mut args = vec!["bridge".to_string()];
    if let Some(p) = profile {
        args.extend(["--profile".into(), p.into()]);
    }
    serde_json::json!({
        "type": "stdio",
        "command": self_exe,
        "args": args,
    })
}

pub fn install(client: &ClientDef, self_exe: &str, profile: Option<&str>) -> Result<bool> {
    let mut cfg = read_config(client).unwrap_or_else(|| serde_json::json!({ client.mcp_key: {} }));
    let obj = cfg
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("client config for {} is not a JSON object", client.name))?;
    let servers = obj
        .entry(client.mcp_key)
        .or_insert_with(|| serde_json::json!({}));
    let map = servers.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "'{}' in {} config is not a JSON object",
            client.mcp_key,
            client.name
        )
    })?;
    map.insert("mcp-proxy".into(), mcp_entry(client, self_exe, profile));
    write_config(client, &cfg)?;
    Ok(true)
}

pub fn uninstall(client: &ClientDef) -> Result<bool> {
    let Some(mut cfg) = read_config(client) else {
        return Ok(false);
    };
    let Some(servers) = cfg
        .as_object_mut()
        .and_then(|o| o.get_mut(client.mcp_key))
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    if servers.remove("mcp-proxy").is_none() {
        return Ok(false);
    }
    write_config(client, &cfg)?;
    Ok(true)
}
