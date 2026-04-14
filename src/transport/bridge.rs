//! Stdio-to-HTTP bridge for clients that only support stdio transport.
//!
//! Reads JSON-RPC messages from stdin, POSTs them to the hub's HTTP /mcp
//! endpoint, and writes responses back to stdout. Optionally auto-starts
//! the Docker container first.

use std::collections::HashMap;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::jsonrpc;
use crate::transport::{content_types, headers};
use crate::util::write_line;

// ---------------------------------------------------------------------------
// SSE line parser
// ---------------------------------------------------------------------------

/// Result of parsing a single SSE line.
#[derive(Debug, PartialEq)]
pub enum SseEvent {
    /// A progress event (has `_progress` field) — should be logged, not forwarded.
    Progress { server: String, status: String },
    /// A regular JSON-RPC data payload — should be forwarded to the client.
    Data(String),
    /// Not a data line, or empty data — skip.
    Skip,
}

/// Extract the `data:` payload from an SSE line, if present.
fn strip_sse_data(line: &str) -> Option<&str> {
    let line = line.trim();
    line.strip_prefix("data: ")
        .or_else(|| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Classify a single SSE data payload.
fn classify_sse_data(data: &str) -> SseEvent {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
        && v.get("_progress").is_some()
    {
        let server = v
            .get("server")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        return SseEvent::Progress { server, status };
    }
    SseEvent::Data(data.to_string())
}

/// Parse a single SSE line into an [`SseEvent`].
pub fn parse_sse_line(line: &str) -> SseEvent {
    match strip_sse_data(line) {
        Some(data) => classify_sse_data(data),
        None => SseEvent::Skip,
    }
}
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

// encode_env_header is now in super (transport/mod.rs)
use super::encode_env_header;

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
        debug!(count = env_overrides.len(), "forwarding env vars to hub");
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
            .header("content-type", content_types::JSON)
            .header(
                "accept",
                format!("{}, {}", content_types::JSON, content_types::SSE),
            );

        if let Some(ref sid) = session_id {
            req = req.header(headers::SESSION_ID, sid);
        }

        if let Some(ref hdr) = env_header {
            req = req.header(headers::ENV, hdr);
        }

        if let Some(ref s) = servers_header {
            req = req.header(headers::SERVERS, s);
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
                    let err = jsonrpc::err(
                        &req_id,
                        jsonrpc::codes::INTERNAL,
                        &format!("hub returned HTTP {status}"),
                    );
                    let out = serde_json::to_string(&err)?;
                    write_line(&mut stdout, out.as_bytes()).await?;
                    continue;
                }

                // Capture session id on first response
                if let Some(sid_hdr) = resp.headers().get(headers::SESSION_ID) {
                    let new_sid = sid_hdr.to_str().ok().map(String::from);
                    if session_id.is_none() && new_sid.is_some() {
                        info!("connected");
                    }
                    session_id = new_sid;
                }

                let is_sse = resp
                    .headers()
                    .get("content-type") // standard HTTP header, not our custom one
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .contains("text/event-stream");

                if is_sse {
                    let mut buf = String::new();
                    while let Some(chunk) = resp.chunk().await? {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = buf.find('\n') {
                            let line: String = buf.drain(..=pos).collect();
                            match parse_sse_line(&line) {
                                SseEvent::Progress { server, status } => match status.as_str() {
                                    "ready" => info!(server = %server, "backend ready"),
                                    "failed" => warn!(server = %server, "backend failed"),
                                    _ => {}
                                },
                                SseEvent::Data(data) => {
                                    write_line(&mut stdout, data.as_bytes()).await?;
                                }
                                SseEvent::Skip => {}
                            }
                        }
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    if !body.trim().is_empty() {
                        write_line(&mut stdout, body.trim().as_bytes()).await?;
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "request failed");
                let err = jsonrpc::err(
                    &req_id,
                    jsonrpc::codes::INTERNAL,
                    &format!("bridge error: {e}"),
                );
                let out = serde_json::to_string(&err)?;
                write_line(&mut stdout, out.as_bytes()).await?;
            }
        }
    }

    // Client disconnected — tell the server to clean up our session.
    info!("stdin closed, disconnecting");
    if let Some(ref sid) = session_id {
        info!(session = %sid, "cleaning up session");
        let _ = client
            .delete(url)
            .header(headers::SESSION_ID, sid)
            .send()
            .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_line() {
        let event = parse_sse_line("data: {\"jsonrpc\":\"2.0\",\"id\":1}");
        assert_eq!(
            event,
            SseEvent::Data("{\"jsonrpc\":\"2.0\",\"id\":1}".to_string())
        );
    }

    #[test]
    fn parse_data_no_space() {
        let event = parse_sse_line("data:{\"id\":2}");
        assert_eq!(event, SseEvent::Data("{\"id\":2}".to_string()));
    }

    #[test]
    fn parse_progress_ready() {
        let line = r#"data: {"_progress":true,"server":"foo","status":"ready","tools":3}"#;
        assert_eq!(
            parse_sse_line(line),
            SseEvent::Progress {
                server: "foo".into(),
                status: "ready".into(),
            }
        );
    }

    #[test]
    fn parse_progress_failed() {
        let line = r#"data: {"_progress":true,"server":"bar","status":"failed","error":"boom"}"#;
        assert_eq!(
            parse_sse_line(line),
            SseEvent::Progress {
                server: "bar".into(),
                status: "failed".into(),
            }
        );
    }

    #[test]
    fn parse_empty_data() {
        assert_eq!(parse_sse_line("data: "), SseEvent::Skip);
        assert_eq!(parse_sse_line("data:"), SseEvent::Skip);
    }

    #[test]
    fn parse_non_data_line() {
        assert_eq!(parse_sse_line("event: message"), SseEvent::Skip);
        assert_eq!(parse_sse_line(""), SseEvent::Skip);
        assert_eq!(parse_sse_line(": comment"), SseEvent::Skip);
    }
}
