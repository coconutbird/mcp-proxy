//! `mcp-proxy health` — check health of a running hub.

use anyhow::Result;

pub async fn run(port: u16) -> Result<()> {
    let url = format!("http://localhost:{port}/health");
    let resp = reqwest::get(&url).await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let data: serde_json::Value = r.json().await?;

            eprintln!("✅ hub is running (port {port})\n");

            if let Some(backends) = data.get("backends").and_then(|b| b.as_array()) {
                if backends.is_empty() {
                    eprintln!("  no backends running");
                } else {
                    eprintln!(
                        "  {:<20} {:<12} {:<8} {:<6} IDLE",
                        "NAME", "SCOPE", "READY", "TOOLS"
                    );
                    eprintln!("  {}", "─".repeat(60));
                    for be in backends {
                        let name = be.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let scope = be.get("scope").and_then(|v| v.as_str()).unwrap_or("?");
                        let ready = if be.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
                            "✓"
                        } else {
                            "✗"
                        };
                        let tools = be.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
                        let idle = match be.get("idle_secs").and_then(|v| v.as_u64()) {
                            Some(s) if s >= 60 => format!("{}m", s / 60),
                            Some(s) => format!("{s}s"),
                            None => "—".to_string(),
                        };
                        eprintln!("  {name:<20} {scope:<12} {ready:<8} {tools:<6} {idle}");
                    }
                }
            }

            let sessions = data
                .get("session_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            eprintln!("\n  active sessions: {sessions}");
        }
        Ok(r) => eprintln!("❌ HTTP {}", r.status()),
        Err(e) => eprintln!("❌ cannot reach hub at {url}: {e}"),
    }
    Ok(())
}
