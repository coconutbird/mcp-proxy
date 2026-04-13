use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

/// Known MCP client applications.
pub struct ClientDef {
    pub name: &'static str,
    pub config_path: PathBuf,
    pub mcp_key: &'static str,
    /// Whether this client supports HTTP transport natively.
    pub supports_http: bool,
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
            supports_http: false,
        },
        ClientDef {
            name: "Claude CLI",
            config_path: home.join(".claude.json"),
            mcp_key: "mcpServers",
            supports_http: true,
        },
        ClientDef {
            name: "Augment",
            config_path: home.join(".augment/settings.json"),
            mcp_key: "mcpServers",
            supports_http: true,
        },
    ]
}

pub fn is_available(client: &ClientDef) -> bool {
    if client.name == "Claude Desktop" {
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
    let out = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&client.config_path, out)?;
    Ok(())
}

pub fn is_installed(client: &ClientDef) -> bool {
    read_config(client).is_some_and(|cfg| {
        cfg.get(client.mcp_key)
            .and_then(Value::as_object)
            .is_some_and(|m| m.contains_key("mcp-hub"))
    })
}

/// Build the MCP server entry to write into a client config.
/// `forward_env` lists env var names the client should set locally and
/// forward to the hub via the bridge. When non-empty, we always use
/// the bridge (stdio) so the env vars can be sent as headers.
pub fn mcp_entry(
    port: u16,
    client: &ClientDef,
    self_exe: &str,
    profile: Option<&str>,
    forward_env: &[String],
) -> Value {
    let use_bridge = !client.supports_http || !forward_env.is_empty();

    if use_bridge {
        let mut args = vec![
            "bridge".to_string(),
            "--url".to_string(),
            format!("http://localhost:{port}/mcp"),
        ];
        if let Some(p) = profile {
            args.extend(["--profile".into(), p.into()]);
        }
        if !forward_env.is_empty() {
            args.push("--forward-env".into());
            args.push(forward_env.join(","));
        }
        let mut entry = serde_json::json!({
            "type": "stdio",
            "command": self_exe,
            "args": args,
        });
        // Put env var placeholders so the user knows what to fill in
        if !forward_env.is_empty() {
            let env_obj: serde_json::Map<String, Value> = forward_env
                .iter()
                .map(|k| (k.clone(), Value::String(format!("${{YOUR_{k}}}"))))
                .collect();
            entry
                .as_object_mut()
                .unwrap()
                .insert("env".into(), Value::Object(env_obj));
        }
        entry
    } else {
        serde_json::json!({
            "type": "http",
            "url": format!("http://localhost:{port}/mcp"),
        })
    }
}

pub fn install(
    client: &ClientDef,
    port: u16,
    self_exe: &str,
    profile: Option<&str>,
    forward_env: &[String],
) -> Result<bool> {
    let mut cfg = read_config(client).unwrap_or_else(|| serde_json::json!({ client.mcp_key: {} }));
    let servers = cfg
        .as_object_mut()
        .unwrap()
        .entry(client.mcp_key)
        .or_insert_with(|| serde_json::json!({}));
    let map = servers.as_object_mut().unwrap();
    map.insert(
        "mcp-hub".into(),
        mcp_entry(port, client, self_exe, profile, forward_env),
    );
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
    if servers.remove("mcp-hub").is_none() {
        return Ok(false);
    }
    write_config(client, &cfg)?;
    Ok(true)
}
