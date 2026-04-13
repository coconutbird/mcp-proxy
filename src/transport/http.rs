use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::server::Hub;

struct Session {
    _id: String,
    /// Per-client profile from X-MCP-Profile header.
    #[allow(dead_code)]
    profile: Option<String>,
    /// Per-client env overrides decoded from X-MCP-Env header.
    #[allow(dead_code)]
    env_overrides: HashMap<String, String>,
}

type Sessions = Arc<RwLock<HashMap<String, Session>>>;

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    sessions: Sessions,
    port: u16,
}

/// Decode the X-MCP-Env header: base64-encoded JSON map of env vars.
fn decode_env_header(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .get("x-mcp-env")
        .and_then(|v| v.to_str().ok())
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| serde_json::from_slice::<HashMap<String, String>>(&bytes).ok())
        .unwrap_or_default()
}

/// Extract requested profile from X-MCP-Profile header.
fn decode_profile_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-mcp-profile")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .filter(|s| !s.is_empty())
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
    // Decode per-client headers
    let env_overrides = decode_env_header(&headers);
    let profile = decode_profile_header(&headers);

    // Resolve or create session
    let session_id = match headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
    {
        Some(id) => id,
        None => {
            let id = Uuid::new_v4().to_string();
            let id_clone = id.clone();
            let env_clone = env_overrides.clone();
            let prof_clone = profile.clone();
            let sessions = state.sessions.clone();
            tokio::spawn(async move {
                sessions.write().await.insert(
                    id.clone(),
                    Session {
                        _id: id,
                        profile: prof_clone,
                        env_overrides: env_clone,
                    },
                );
            });
            id_clone
        }
    };

    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let id = req.get("id").cloned();

    let result = handle_request(
        &state.hub,
        &method,
        params,
        profile.as_deref(),
        &env_overrides,
    )
    .await;

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

async fn handle_request(
    hub: &Hub,
    method: &str,
    params: Value,
    profile: Option<&str>,
    env_overrides: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mcp-proxy", "version": env!("CARGO_PKG_VERSION") },
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let tools = hub.list_tools_for(profile, env_overrides).await;
            Ok(serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            hub.call_tool_for(name, args, profile, env_overrides).await
        }
        _ => anyhow::bail!("unsupported method: {method}"),
    }
}
