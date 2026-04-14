//! `mcp-proxy test` — validate config and test each backend.

use std::collections::HashMap;

use anyhow::Result;
use tokio::task::JoinSet;

pub async fn run(config_path: &std::path::Path, filter_servers: &[String]) -> Result<()> {
    eprintln!("🔍 validating config: {}", config_path.display());
    let cfg = crate::config::load(config_path)?;
    eprintln!("  ✅ config parsed ({} server(s))\n", cfg.servers.len());

    let empty_env: HashMap<String, String> = HashMap::new();

    // Collect servers to test
    let to_test: Vec<_> = cfg
        .servers
        .iter()
        .filter(|(name, srv)| {
            !srv.is_disabled(name) && (filter_servers.is_empty() || filter_servers.contains(name))
        })
        .map(|(name, srv)| (name.clone(), srv.clone()))
        .collect();

    // Start all backends concurrently
    let mut set = JoinSet::new();
    for (name, srv) in to_test {
        let env = empty_env.clone();
        set.spawn(async move {
            let result = crate::backend::Backend::start(name.clone(), &srv, &env).await;
            (name, result)
        });
    }

    let mut pass = 0u32;
    let mut fail = 0u32;

    while let Some(res) = set.join_next().await {
        let (name, result) = res.expect("test task panicked");
        match result {
            Ok(mut be) => {
                let tool_count = be.tools.len();
                eprintln!("  {name}: ✅ {} tool(s)", tool_count);
                for t in &be.tools {
                    eprintln!("    • {}", t.name);
                }
                be.kill();
                pass += 1;
            }
            Err(e) => {
                eprintln!("  {name}: ❌ {e}");
                fail += 1;
            }
        }
    }

    eprintln!("\n{pass} passed, {fail} failed");
    if fail > 0 {
        anyhow::bail!("{fail} server(s) failed validation");
    }
    Ok(())
}
