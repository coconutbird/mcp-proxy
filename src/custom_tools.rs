use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;
use tracing::{info, warn};

use crate::backend::Tool;
use crate::config::{CustomToolConfig, is_toggled_off};

/// Executor for lightweight custom tools (shell / http) defined in config.
pub struct CustomTools {
    tools: HashMap<String, CustomToolConfig>,
}

impl CustomTools {
    pub fn new(configs: &HashMap<String, CustomToolConfig>) -> Self {
        let mut tools = HashMap::new();
        for (name, cfg) in configs {
            if name.starts_with('_') {
                continue;
            }
            if is_toggled_off(cfg.env_toggle()) {
                warn!("skipping custom tool {name} (toggled off)");
                continue;
            }
            tools.insert(name.clone(), cfg.clone());
        }
        info!("loaded {} custom tool(s)", tools.len());
        Self { tools }
    }

    /// Return MCP tool definitions for all loaded custom tools.
    pub fn list(&self) -> Vec<Tool> {
        self.tools
            .iter()
            .map(|(name, cfg)| Tool {
                name: format!("custom_{name}"),
                description: Some(cfg.description().into()),
                input_schema: Some(cfg.input_schema().clone()),
                original_name: name.clone(),
                backend_name: "custom".into(),
            })
            .collect()
    }

    pub fn has(&self, prefixed_name: &str) -> bool {
        let key = prefixed_name
            .strip_prefix("custom_")
            .unwrap_or(prefixed_name);
        self.tools.contains_key(key)
    }

    pub async fn call(&self, prefixed_name: &str, args: &Value) -> Result<Value> {
        let key = prefixed_name
            .strip_prefix("custom_")
            .unwrap_or(prefixed_name);
        let cfg = self
            .tools
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown custom tool: {prefixed_name}"))?;
        let args_map = args.as_object().cloned().unwrap_or_default();
        match cfg {
            CustomToolConfig::Shell { command, .. } => exec_shell(command, &args_map).await,
            CustomToolConfig::Http {
                url,
                method,
                headers,
                body,
                ..
            } => {
                exec_http(
                    url,
                    method.as_deref(),
                    headers.as_ref(),
                    body.as_ref(),
                    &args_map,
                )
                .await
            }
        }
    }
}

fn substitute(template: &str, args: &serde_json::Map<String, Value>) -> String {
    let mut out = template.to_string();
    while let Some(start) = out.find("${") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let key = &out[start + 2..start + end];
        let val = args
            .get(key)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .or_else(|| std::env::var(key).ok())
            .unwrap_or_default();
        out = format!("{}{}{}", &out[..start], val, &out[start + end + 1..]);
    }
    out
}

async fn exec_shell(command: &str, args: &serde_json::Map<String, Value>) -> Result<Value> {
    let cmd = substitute(command, args);
    eprintln!("[custom] exec: {cmd}");
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()
        .await?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    Ok(serde_json::json!({ "content": [{ "type": "text", "text": text }] }))
}

async fn exec_http(
    url_tpl: &str,
    method: Option<&str>,
    headers: Option<&HashMap<String, String>>,
    body: Option<&Value>,
    args: &serde_json::Map<String, Value>,
) -> Result<Value> {
    let url = substitute(url_tpl, args);
    let method = method.unwrap_or("GET");
    eprintln!("[custom] {method} {url}");

    let client = reqwest::Client::new();
    let mut req = client.request(method.parse()?, &url);
    if let Some(hdrs) = headers {
        for (k, v) in hdrs {
            req = req.header(k, substitute(v, args));
        }
    }
    if let Some(b) = body
        && method != "GET"
    {
        req = req.json(&substitute_value(b, args));
    }
    let resp = req.send().await?;
    let text = resp.text().await?;
    Ok(serde_json::json!({ "content": [{ "type": "text", "text": text }] }))
}

fn substitute_value(v: &Value, args: &serde_json::Map<String, Value>) -> Value {
    match v {
        Value::String(s) => Value::String(substitute(s, args)),
        Value::Object(m) => {
            let out: serde_json::Map<String, Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), substitute_value(v, args)))
                .collect();
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| substitute_value(v, args)).collect()),
        other => other.clone(),
    }
}
