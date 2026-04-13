use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};

use crate::server::Hub;

/// Run the aggregator over stdin/stdout (JSON-RPC, one message per line).
pub async fn serve(hub: Arc<Hub>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

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

        let response = handle(&hub, &method, params).await;

        // Only send a response if the request had an id (not a notification)
        if let Some(id) = id {
            let resp = match response {
                Ok(result) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                Err(e) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32000, "message": e.to_string() },
                }),
            };
            let out = serde_json::to_string(&resp)?;
            stdout.write_all(out.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn handle(hub: &Hub, method: &str, params: Value) -> Result<Value> {
    match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "mcp-hub",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let tools = hub.list_tools().await;
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            hub.call_tool(name, args).await
        }
        _ => anyhow::bail!("unsupported method: {method}"),
    }
}
