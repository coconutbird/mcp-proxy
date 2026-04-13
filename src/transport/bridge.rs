//! Stdio-to-HTTP bridge for clients that only support stdio transport.
//!
//! Reads JSON-RPC messages from stdin, POSTs them to the hub's HTTP /mcp
//! endpoint, and writes responses back to stdout. Optionally auto-starts
//! the Docker container first.

use std::collections::HashMap;

use anyhow::Result;
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::error;

/// Collect env vars by name from the current process environment.
fn collect_env_overrides(names: &[String]) -> HashMap<String, String> {
    names
        .iter()
        .filter_map(|name| {
            let val = std::env::var(name).ok()?;
            Some((name.clone(), val))
        })
        .collect()
}

/// Encode env overrides as a base64 JSON header value.
fn encode_env_header(env: &HashMap<String, String>) -> Option<String> {
    if env.is_empty() {
        return None;
    }
    let json = serde_json::to_string(env).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(json))
}

/// Run the bridge, forwarding stdin → HTTP → stdout.
///
/// Resolution order:
/// 1. `--profile` reads the local `servers.json` profile for include list + env overrides
/// 2. `--servers` overrides the profile's include list
/// 3. `--forward-env` adds env vars from the local process on top
///
/// The bridge sends `X-MCP-Servers` and `X-MCP-Env` headers — the server
/// never sees a profile name.
pub async fn run(
    url: &str,
    profile: Option<&str>,
    servers: &[String],
    forward_env: &[String],
    config_path: &std::path::Path,
) -> Result<()> {
    let client = reqwest::Client::new();
    let mut session_id: Option<String> = None;

    // Start with env vars forwarded from the local process
    let mut env_overrides = collect_env_overrides(forward_env);

    // Resolve the local profile if provided
    let mut server_list: Vec<String> = servers.to_vec();

    if let Some(prof_name) = profile {
        let pf = crate::config::load_profiles(config_path)?;
        if let Some(prof) = pf.profiles.get(prof_name) {
            // Profile include list (--servers takes priority)
            if server_list.is_empty() && !prof.include.is_empty() {
                server_list = prof.include.clone();
            }

            // Profile env overrides (profile < forward-env on conflict)
            for ovr in prof.servers.values() {
                if let Some(ref env_map) = ovr.env {
                    for (k, v) in env_map {
                        env_overrides.entry(k.clone()).or_insert_with(|| {
                            crate::config::expand_env_with_overrides(v, &HashMap::new())
                        });
                    }
                }
            }
        } else {
            anyhow::bail!("unknown local profile: {prof_name}");
        }
    }

    let env_header = encode_env_header(&env_overrides);
    let servers_header = if server_list.is_empty() {
        None
    } else {
        Some(server_list.join(","))
    };

    if !server_list.is_empty() {
        eprintln!("bridge requesting servers: {}", server_list.join(", "));
    }
    if !env_overrides.is_empty() {
        eprintln!(
            "bridge forwarding {} env var(s): {}",
            env_overrides.len(),
            env_overrides.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    eprintln!("bridge connecting to {url}");

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let mut req = client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");

        if let Some(ref sid) = session_id {
            req = req.header("mcp-session-id", sid);
        }

        if let Some(ref hdr) = env_header {
            req = req.header("x-mcp-env", hdr);
        }

        if let Some(ref s) = servers_header {
            req = req.header("x-mcp-servers", s);
        }

        match req.body(line).send().await {
            Ok(resp) => {
                // Capture session id
                if let Some(sid) = resp.headers().get("mcp-session-id") {
                    session_id = sid.to_str().ok().map(String::from);
                }

                let ct = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = resp.text().await.unwrap_or_default();

                if ct.contains("text/event-stream") {
                    // Parse SSE events, extract data lines
                    for event_line in body.lines() {
                        if let Some(data) = event_line.strip_prefix("data: ") {
                            let data = data.trim();
                            if !data.is_empty() {
                                stdout.write_all(data.as_bytes()).await?;
                                stdout.write_all(b"\n").await?;
                            }
                        }
                    }
                } else if !body.trim().is_empty() {
                    stdout.write_all(body.trim().as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                }
                stdout.flush().await?;
            }
            Err(e) => {
                error!("bridge request failed: {e}");
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32000, "message": format!("bridge error: {e}") }
                });
                let out = serde_json::to_string(&err)?;
                stdout.write_all(out.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
    }

    Ok(())
}
