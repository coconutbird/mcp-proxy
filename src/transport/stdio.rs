//! Stdio MCP transport — JSON-RPC over stdin/stdout.
//!
//! Each line on stdin is a JSON-RPC request; responses are written as
//! single-line JSON to stdout. Used when the aggregator is launched as
//! a subprocess by an MCP client.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{error, info};

use crate::jsonrpc;
use crate::server::{Hub, STDIO_SESSION_ID};
use crate::util::write_line;

/// Run the aggregator over stdin/stdout (JSON-RPC, one message per line).
pub async fn serve(hub: Arc<Hub>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let empty_env = HashMap::new();
    let empty_servers: Vec<String> = Vec::new();

    info!("stdio transport ready");

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                error!("bad json-rpc input: {e}");
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let response = hub
            .handle_request(
                &method,
                params,
                &empty_servers,
                &empty_env,
                STDIO_SESSION_ID,
            )
            .await;

        // Only send a response if the request had an id (not a notification)
        if let Some(id) = id {
            let resp = match response {
                Ok(result) => jsonrpc::ok(&id, result),
                Err(e) => e.to_json(&id),
            };
            let out = serde_json::to_string(&resp)?;
            write_line(&mut stdout, out.as_bytes()).await?;
        }
    }

    Ok(())
}
