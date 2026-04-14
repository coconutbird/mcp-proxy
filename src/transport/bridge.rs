//! Stdio-to-HTTP bridge for clients that only support stdio transport.
//!
//! Reads JSON-RPC messages from stdin, POSTs them to the hub's HTTP /mcp
//! endpoint, and writes responses back to stdout. Optionally auto-starts
//! the Docker container first.

use std::collections::HashMap;

use anyhow::Result;
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};
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

    // Auto-detect required env vars from server configs
    let mut auto_env_keys: Vec<String> = Vec::new();
    if let Ok(cfg) = crate::config::load(config_path) {
        for srv in cfg.servers.values() {
            auto_env_keys.extend(crate::server::relevant_env_keys(srv));
        }
        auto_env_keys.sort();
        auto_env_keys.dedup();
    }

    // Collect auto-detected + explicit --forward-env (explicit wins on overlap)
    let mut env_overrides = collect_env_overrides(&auto_env_keys);
    env_overrides.extend(collect_env_overrides(forward_env));

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
        debug!(servers = server_list.join(", "), "requesting servers");
    }
    if !env_overrides.is_empty() {
        debug!(
            vars = env_overrides.keys().cloned().collect::<Vec<_>>().join(", "),
            "forwarding env"
        );
    }

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    info!(hub = url, "bridge ready");

    loop {
        // Race the next input line against ctrl+c so we can clean up gracefully
        // instead of calling process::exit which skips destructors.
        let line = tokio::select! {
            result = lines.next_line() => {
                match result? {
                    Some(l) => l,
                    None => break, // EOF
                }
            }
            _ = tokio::signal::ctrl_c() => {
                warn!("interrupted (ctrl+c)");
                break;
            }
        };
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

        // Parse request id for error responses (JSON-RPC 2.0 requires it).
        let req_id = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);

        match req.body(line).send().await {
            Ok(mut resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    error!(status = %status, body = body, "hub returned error");
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32000, "message": format!("hub returned HTTP {status}") }
                    });
                    let out = serde_json::to_string(&err)?;
                    stdout.write_all(out.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    continue;
                }

                // Capture session id on first response
                if let Some(sid_hdr) = resp.headers().get("mcp-session-id") {
                    let new_sid = sid_hdr.to_str().ok().map(String::from);
                    if session_id.is_none() && new_sid.is_some() {
                        info!("connected");
                    }
                    session_id = new_sid;
                }

                let is_sse = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .contains("text/event-stream");

                if is_sse {
                    // Stream SSE chunks as they arrive
                    let mut buf = String::new();
                    while let Some(chunk) = resp.chunk().await? {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        // Process complete lines
                        while let Some(pos) = buf.find('\n') {
                            let line = buf[..pos].trim().to_string();
                            buf = buf[pos + 1..].to_string();
                            if let Some(data) = line.strip_prefix("data: ") {
                                let data = data.trim();
                                if data.is_empty() {
                                    continue;
                                }
                                // Progress events → log to stderr, don't forward
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
                                    && v.get("_progress").is_some()
                                {
                                    let server =
                                        v.get("server").and_then(|s| s.as_str()).unwrap_or("?");
                                    let status =
                                        v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
                                    match status {
                                        "ready" => {
                                            let tools = v
                                                .get("tools")
                                                .and_then(|t| t.as_u64())
                                                .unwrap_or(0);
                                            info!(server = server, tools = tools, "backend ready");
                                        }
                                        "failed" => {
                                            let err = v
                                                .get("error")
                                                .and_then(|s| s.as_str())
                                                .unwrap_or("?");
                                            warn!(server = server, "{err}");
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }
                                // Regular JSON-RPC data → forward to client
                                stdout.write_all(data.as_bytes()).await?;
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                            }
                        }
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    if !body.trim().is_empty() {
                        stdout.write_all(body.trim().as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                    }
                    stdout.flush().await?;
                }
            }
            Err(e) => {
                error!(error = %e, "request failed");
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": { "code": -32000, "message": format!("bridge error: {e}") }
                });
                let out = serde_json::to_string(&err)?;
                stdout.write_all(out.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }
    }

    // Client disconnected — tell the server to clean up our session.
    info!("stdin closed, disconnecting");
    if let Some(ref sid) = session_id {
        info!(session = %sid, "cleaning up session");
        let _ = client
            .delete(url)
            .header("mcp-session-id", sid)
            .send()
            .await;
    }

    Ok(())
}
