//! `mcp-proxy test` — validate config and test each backend.

use std::collections::HashMap;

use anyhow::Result;

pub async fn run(config_path: &std::path::Path, filter_servers: &[String]) -> Result<()> {
    eprintln!("🔍 validating config: {}", config_path.display());
    let cfg = crate::config::load(config_path)?;
    eprintln!("  ✅ config parsed ({} server(s))\n", cfg.servers.len());

    let empty_env = HashMap::new();
    let mut pass = 0u32;
    let mut fail = 0u32;

    for (name, srv) in &cfg.servers {
        if srv.is_disabled(name) {
            continue;
        }
        if !filter_servers.is_empty() && !filter_servers.contains(name) {
            continue;
        }
        eprint!("  testing {name}...");

        match crate::backend::Backend::start(name.clone(), srv, &empty_env).await {
            Ok(mut be) => {
                let tool_count = be.tools.len();
                eprintln!(" ✅ {} tool(s)", tool_count);
                for t in &be.tools {
                    eprintln!("    • {}", t.name);
                }
                be.kill();
                pass += 1;
            }
            Err(e) => {
                eprintln!(" ❌ {e}");
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
