use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{info, warn};

use crate::config::{ServerConfig, expand_env_with_overrides};

#[derive(serde::Serialize)]
struct Rpc<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}
#[derive(serde::Deserialize)]
struct Resp {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcErr>,
}
#[derive(serde::Deserialize)]
struct RpcErr {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

/// A tool discovered from a backend, namespaced as <backend>_<tool>.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(skip)]
    #[allow(dead_code)]
    pub original_name: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub backend_name: String,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value>>>;

/// A running MCP server subprocess managed by the hub.
pub struct Backend {
    pub name: String,
    pub tools: Vec<Tool>,
    pub ready: bool,
    seq: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    _child: Option<Child>,
}

impl Backend {
    /// Spawn the server process, complete the MCP handshake, discover tools.
    /// `extra_env` are per-client overrides that take precedence over config env.
    ///
    /// If the command isn't found locally and the server has an `install` config,
    /// we automatically build a Docker image and run the backend in a container.
    pub async fn start(
        name: String,
        cfg: &ServerConfig,
        extra_env: &HashMap<String, String>,
    ) -> Result<Self> {
        // Expand ${VAR} references against BOTH the hub's process env AND
        // the client's forwarded env (client wins on conflict).
        let expand = |s: &str| expand_env_with_overrides(s, extra_env);

        let args: Vec<String> = cfg.args.iter().map(|a| expand(a)).collect();
        let mut env: HashMap<String, String> = cfg
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand(v)))
            .collect();
        // Also inject the raw client overrides so backends that read env
        // vars directly (not via config ${} refs) still get them.
        for (k, v) in extra_env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }

        let use_docker = cfg.install.is_some();

        let mut child = if use_docker {
            info!("[{name}] has install config, using Docker");
            crate::docker::ensure_image(&name, cfg).await?;
            crate::docker::run_container(&name, cfg, extra_env).await?
        } else {
            Command::new(&cfg.command)
                .args(&args)
                .envs(&env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| anyhow::anyhow!("spawn '{}' ({}): {e}", name, cfg.command))?
        };

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();
        let child_stderr = child.stderr.take().unwrap();

        let pending: Arc<Mutex<Pending>> = Arc::default();
        let stdin = Arc::new(Mutex::new(Some(child_stdin)));

        // Drain stderr to the hub stderr
        let tag = name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(child_stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{tag}] {line}");
            }
        });

        // Route stdout JSON-RPC responses to pending futures
        let p = pending.clone();
        let tag = name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(child_stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(resp) = serde_json::from_str::<Resp>(&line) else {
                    warn!("[{tag}] non-json stdout: {line}");
                    continue;
                };
                let Some(id) = resp.id else { continue };
                if let Some(tx) = p.lock().await.remove(&id) {
                    let val = match resp.error {
                        Some(e) => Err(anyhow::anyhow!("{}", e.message)),
                        None => Ok(resp.result.unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(val);
                }
            }
        });

        let mut be = Self {
            name,
            tools: Vec::new(),
            ready: false,
            seq: AtomicU64::new(1),
            pending,
            stdin,
            _child: Some(child),
        };
        be.handshake().await?;
        Ok(be)
    }

    async fn handshake(&mut self) -> Result<()> {
        self.rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mcp-proxy", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;

        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;

        match self.rpc("tools/list", serde_json::json!({})).await {
            Ok(val) => {
                let items = val.get("tools").and_then(Value::as_array);
                self.tools = items
                    .into_iter()
                    .flatten()
                    .filter_map(|t| {
                        let raw = t.get("name")?.as_str()?;
                        Some(Tool {
                            name: format!("{}_{raw}", self.name),
                            description: t
                                .get("description")
                                .and_then(Value::as_str)
                                .map(Into::into),
                            input_schema: t.get("inputSchema").cloned(),
                            original_name: raw.into(),
                            backend_name: self.name.clone(),
                        })
                    })
                    .collect();
            }
            Err(e) => warn!("[{}] tools/list failed: {e}", self.name),
        }

        self.ready = true;
        info!("[{}] ready with {} tool(s)", self.name, self.tools.len());
        Ok(())
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let msg = serde_json::to_string(&Rpc {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut g = self.stdin.lock().await;
            let w = g.as_mut().ok_or_else(|| anyhow::anyhow!("stdin closed"))?;
            w.write_all(msg.as_bytes()).await?;
            w.write_all(
                b"
",
            )
            .await?;
            w.flush().await?;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => bail!("[{}] response channel dropped", self.name),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("[{}] {method} timed out", self.name)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params,
        }))?;
        let mut g = self.stdin.lock().await;
        if let Some(w) = g.as_mut() {
            w.write_all(msg.as_bytes()).await?;
            w.write_all(
                b"
",
            )
            .await?;
            w.flush().await?;
        }
        Ok(())
    }

    /// Forward a tool call to this backend using the original tool name.
    pub async fn call_tool(&self, original_name: &str, args: Value) -> Result<Value> {
        self.rpc(
            "tools/call",
            serde_json::json!({
                "name": original_name, "arguments": args,
            }),
        )
        .await
    }

    /// Kill the child process.
    pub fn kill(&mut self) {
        if let Some(ref mut c) = self._child {
            let _ = c.start_kill();
        }
    }
}
