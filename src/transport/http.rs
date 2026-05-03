//! Streamable-HTTP MCP transport.
//!
//! Exposes the hub as an HTTP server with:
//! - `POST /mcp` — JSON-RPC requests (with optional SSE streaming for `tools/list`)
//! - `DELETE /mcp` — session teardown
//! - `GET  /health` — health check endpoint

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::jsonrpc;
use crate::server::{BackendProgress, Hub};
use crate::transport::{content_types, headers};

// ---------------------------------------------------------------------------
// Session tracking with TTL
// ---------------------------------------------------------------------------

/// Default session idle timeout (30 minutes).
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Default maximum concurrent sessions. 0 = unlimited.
const MAX_SESSIONS: usize = 100;

/// A tracked session with last-activity timestamp.
struct SessionEntry {
    last_active: Instant,
}

/// Active sessions with TTL tracking.
type Sessions = Arc<RwLock<HashMap<String, SessionEntry>>>;

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    sessions: Sessions,
    port: u16,
}

/// Per-request client context decoded from MCP headers.
///
/// Bundles env overrides, server filter, and session ID so handlers don't
/// have to decode multiple headers independently.
struct ClientContext {
    env_overrides: HashMap<String, String>,
    servers: Vec<String>,
}

impl ClientContext {
    /// Decode all per-client headers in one pass.
    fn from_headers(hdrs: &HeaderMap) -> Self {
        let env_overrides = hdrs
            .get(headers::ENV)
            .and_then(|v| v.to_str().ok())
            .map(super::decode_env_header)
            .unwrap_or_default();
        let servers = hdrs
            .get(headers::SERVERS)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                s.split(',')
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            env_overrides,
            servers,
        }
    }
}

/// Start the Streamable-HTTP MCP server on the given port.
pub async fn serve(hub: Arc<Hub>, port: u16) -> anyhow::Result<()> {
    let sessions: Sessions = Arc::default();

    // Background reaper: every 60s, evict sessions idle longer than SESSION_TTL.
    {
        let sessions = sessions.clone();
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let expired: Vec<String> = {
                    let map = sessions.read().await;
                    map.iter()
                        .filter(|(_, e)| e.last_active.elapsed() > SESSION_TTL)
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                if !expired.is_empty() {
                    let mut map = sessions.write().await;
                    for id in &expired {
                        map.remove(id);
                    }
                    drop(map);
                    for id in &expired {
                        debug!(session = %id, "session expired (idle)");
                        hub.cleanup_session(id).await;
                    }
                }
            }
        });
    }

    let state = AppState {
        hub,
        sessions,
        port,
    };

    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/mcp", delete(handle_mcp_delete))
        .route("/health", get(handle_health))
        .with_state(state);

    // Bind to loopback only by default — this is a local dev tool and
    // exposing the unauthenticated proxy to the network is dangerous.
    // Use MCP_BIND=0.0.0.0 or --bind 0.0.0.0 to listen on all interfaces.
    let bind_addr: std::net::IpAddr = std::env::var("MCP_BIND")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let addr = std::net::SocketAddr::from((bind_addr, port));
    info!(addr = %addr, "http transport listening");
    info!("  MCP endpoint: http://localhost:{port}/mcp");
    info!("  Health check: http://localhost:{port}/health");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn handle_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    // Decode per-client headers
    let ctx = ClientContext::from_headers(&headers);

    // Resolve or create session — reject unknown session IDs to prevent
    // hijacking of per-session backends.
    let session_id = match headers
        .get(headers::SESSION_ID)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
    {
        Some(id) => {
            let mut map = state.sessions.write().await;
            match map.get_mut(&id) {
                Some(entry) => {
                    entry.last_active = Instant::now();
                }
                None => {
                    return (StatusCode::NOT_FOUND, "unknown session").into_response();
                }
            }
            id
        }
        None => {
            let mut map = state.sessions.write().await;
            if MAX_SESSIONS > 0 && map.len() >= MAX_SESSIONS {
                warn!(max = MAX_SESSIONS, "session limit reached");
                return (StatusCode::SERVICE_UNAVAILABLE, "too many active sessions")
                    .into_response();
            }
            let id = Uuid::new_v4().to_string();
            map.insert(
                id.clone(),
                SessionEntry {
                    last_active: Instant::now(),
                },
            );
            id
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
        let _ = state
            .hub
            .handle_request(
                &method,
                params,
                &ctx.servers,
                &ctx.env_overrides,
                &session_id,
            )
            .await;
        return (StatusCode::ACCEPTED, "").into_response();
    };

    // tools/list: stream progress events via SSE, then the final result
    if method == "tools/list" {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BackendProgress>(32);
        let hub = state.hub.clone();
        let servers = ctx.servers.clone();
        let env_overrides = ctx.env_overrides.clone();
        let sid = session_id.clone();
        let rid = req_id.clone();

        let stream = async_stream::stream! {
            // Spawn the backend startup work
            let handle = tokio::spawn(async move {
                hub.list_tools(&servers, &env_overrides, &sid, Some(tx)).await
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
            let (tools, errors) = match handle.await {
                Ok(r) => r,
                Err(e) => {
                    let err_resp = jsonrpc::err(&rid, jsonrpc::codes::INTERNAL, &format!("internal error: {e}"));
                    yield Ok(format!("data: {}\n\n", serde_json::to_string(&err_resp).unwrap()));
                    return;
                }
            };
            let mut result = serde_json::json!({ "tools": tools });
            if !errors.is_empty() {
                let err_list: Vec<Value> = errors
                    .into_iter()
                    .map(|(name, msg)| serde_json::json!({ "server": name, "error": msg }))
                    .collect();
                result["_errors"] = Value::Array(err_list);
            }
            let body = jsonrpc::ok(&rid, result);
            yield Ok(format!("data: {}\n\n", serde_json::to_string(&body).unwrap()));
        };

        return Response::builder()
            .status(200)
            .header("content-type", content_types::SSE)
            .header(headers::SESSION_ID, &session_id)
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let result = state
        .hub
        .handle_request(
            &method,
            params,
            &ctx.servers,
            &ctx.env_overrides,
            &session_id,
        )
        .await;

    let body = match result {
        Ok(val) => jsonrpc::ok(&req_id, val),
        Err(e) => e.to_json(&req_id),
    };

    Response::builder()
        .status(200)
        .header("content-type", content_types::JSON)
        .header(headers::SESSION_ID, &session_id)
        .body(Body::from(serde_json::to_string(&body).unwrap_or_default()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// DELETE /mcp — client session teardown. Kills all per-session backends.
async fn handle_mcp_delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let session_id = match headers
        .get(headers::SESSION_ID)
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

/// Wait for SIGINT or SIGTERM so axum can drain connections gracefully.
async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async { signal::ctrl_c().await.expect("ctrl+c handler") };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received, draining connections…");
}
