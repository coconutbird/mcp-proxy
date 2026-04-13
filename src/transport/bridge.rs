//! Stdio-to-HTTP bridge for clients that only support stdio transport.
//!
//! Reads JSON-RPC messages from stdin, POSTs them to the hub's HTTP /mcp
//! endpoint, and writes responses back to stdout. Optionally auto-starts
//! the Docker container first.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::error;

/// Run the bridge, forwarding stdin → HTTP → stdout.
pub async fn run(url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let mut session_id: Option<String> = None;

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
