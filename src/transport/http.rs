use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use crate::server::BackendProgress;

use crate::server::Hub;

struct Session {
    _id: String,
    /// Per-client server include list from X-MCP-Servers header.
    #[allow(dead_code)]
    servers: Vec<String>,
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

/// Decode X-MCP-Servers header: comma-separated list of server names.
fn decode_servers_header(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("x-mcp-servers")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
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
        .route("/mcp", delete(handle_mcp_delete))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!(addr = %addr, "http transport listening");
    info!("  MCP endpoint: http://localhost:{port}/mcp");
    info!("  Health check: http://localhost:{port}/health");

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
    let servers = decode_servers_header(&headers);

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
            let servers_clone = servers.clone();
            let sessions = state.sessions.clone();
            tokio::spawn(async move {
                sessions.write().await.insert(
                    id.clone(),
                    Session {
                        _id: id,
                        servers: servers_clone,
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

    // Notifications (no id) get 202 Accepted
    let Some(req_id) = id else {
        let _ = handle_request(
            &state.hub,
            &method,
            params,
            &servers,
            &env_overrides,
            &session_id,
        )
        .await;
        return (StatusCode::ACCEPTED, "").into_response();
    };

    // tools/list: stream progress events via SSE, then the final result
    if method == "tools/list" {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BackendProgress>(32);
        let hub = state.hub.clone();
        let servers = servers.clone();
        let env_overrides = env_overrides.clone();
        let sid = session_id.clone();
        let rid = req_id.clone();

        let stream = async_stream::stream! {
            // Spawn the backend startup work
            let handle = tokio::spawn(async move {
                hub.list_tools_streaming(&servers, &env_overrides, &sid, tx).await
            });

            // Yield progress events as they arrive
            while let Some(progress) = rx.recv().await {
                let event = match &progress {
                    BackendProgress::Ready { server, tools } => {
                        serde_json::json!({
                            "_progress": true,
                            "server": server,
                            "status": "ready",
                            "tools": tools,
                        })
                    }
                    BackendProgress::Failed { server, error } => {
                        serde_json::json!({
                            "_progress": true,
                            "server": server,
                            "status": "failed",
                            "error": error,
                        })
                    }
                };
                yield Ok::<_, std::convert::Infallible>(
                    format!("data: {}\n\n", serde_json::to_string(&event).unwrap())
                );
            }

            // All backends done — build final result
            let (tools, errors) = handle.await.unwrap();
            let mut result = serde_json::json!({ "tools": tools });
            if !errors.is_empty() {
                let err_list: Vec<Value> = errors
                    .into_iter()
                    .map(|(name, msg)| serde_json::json!({ "server": name, "error": msg }))
                    .collect();
                result["_errors"] = Value::Array(err_list);
            }
            let body = serde_json::json!({ "jsonrpc": "2.0", "id": rid, "result": result });
            yield Ok(format!("data: {}\n\n", serde_json::to_string(&body).unwrap()));
        };

        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("mcp-session-id", &session_id)
            .body(Body::from_stream(stream))
            .unwrap();
    }

    let result = handle_request(
        &state.hub,
        &method,
        params,
        &servers,
        &env_overrides,
        &session_id,
    )
    .await;

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

/// DELETE /mcp — client session teardown. Kills all per-session backends.
async fn handle_mcp_delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let session_id = match headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
    {
        Some(id) => id,
        None => {
            return (StatusCode::BAD_REQUEST, "missing mcp-session-id header").into_response();
        }
    };

    // Remove session
    let removed = state.sessions.write().await.remove(&session_id).is_some();
    if !removed {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    }

    // Kill all per-session backends
    info!("session {session_id} disconnected, cleaning up backends");
    state.hub.cleanup_session(&session_id).await;

    (StatusCode::OK, "session closed").into_response()
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
    servers: &[String],
    env_overrides: &HashMap<String, String>,
    session_id: &str,
) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mcp-proxy", "version": env!("CARGO_PKG_VERSION") },
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => {
            let (tools, errors) = hub.list_tools_for(servers, env_overrides, session_id).await;
            let mut result = serde_json::json!({ "tools": tools });
            if !errors.is_empty() {
                let err_list: Vec<Value> = errors
                    .into_iter()
                    .map(|(name, msg)| serde_json::json!({ "server": name, "error": msg }))
                    .collect();
                result["_errors"] = Value::Array(err_list);
            }
            Ok(result)
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            hub.call_tool_for(name, args, servers, env_overrides, session_id)
                .await
        }
        _ => anyhow::bail!("unsupported method: {method}"),
    }
}
