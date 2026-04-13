use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::server::Hub;

struct Session {
    _id: String,
}

type Sessions = Arc<RwLock<HashMap<String, Session>>>;

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    sessions: Sessions,
    port: u16,
}

/// Start the Streamable-HTTP MCP server on the given port.
pub async fn serve(hub: Arc<Hub>, port: u16) -> anyhow::Result<()> {
    let state = AppState {
        hub,
        sessions: Arc::default(),
        port,
    };

    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/mcp", get(handle_mcp_sse))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!("http transport listening on {addr}");
    eprintln!("  MCP endpoint: http://localhost:{port}/mcp");
    eprintln!("  Health check: http://localhost:{port}/health");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    // Resolve or create session
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| {
            let id = Uuid::new_v4().to_string();
            let id_ret = id.clone();
            let sessions = state.sessions.clone();
            tokio::spawn(async move {
                sessions
                    .write()
                    .await
                    .insert(id.clone(), Session { _id: id });
            });
            id_ret
        });

    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let id = req.get("id").cloned();

    let result = handle_request(&state.hub, &method, params).await;

    // Notifications (no id) get 202 Accepted
    let Some(req_id) = id else {
        return (StatusCode::ACCEPTED, "").into_response();
    };

    let body = match result {
        Ok(val) => serde_json::json!({ "jsonrpc": "2.0", "id": req_id, "result": val }),
        Err(e) => serde_json::json!({
            "jsonrpc": "2.0", "id": req_id,
            "error": { "code": -32000, "message": e.to_string() }
        }),
    };

    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("mcp-session-id", &session_id)
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

/// GET /mcp — SSE endpoint for server-initiated messages (currently no-op).
async fn handle_mcp_sse() -> Response {
    // Placeholder: real SSE streaming for server notifications can be added later
    (StatusCode::METHOD_NOT_ALLOWED, "use POST").into_response()
}

async fn handle_health(State(state): State<AppState>) -> Json<Value> {
    let mut health = state.hub.health().await;
    if let Some(obj) = health.as_object_mut() {
        obj.insert("status".into(), "ok".into());
        obj.insert("transport".into(), "http".into());
        obj.insert("port".into(), state.port.into());
        obj.insert("sessions".into(), state.sessions.read().await.len().into());
    }
    Json(health)
}

async fn handle_request(hub: &Hub, method: &str, params: Value) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mcp-hub", "version": env!("CARGO_PKG_VERSION") },
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
