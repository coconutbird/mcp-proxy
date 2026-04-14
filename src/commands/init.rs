//! `mcp-proxy init` — create a starter config file.

use anyhow::Result;

pub fn run(config_path: &std::path::Path) -> Result<()> {
    match crate::config::init_config(config_path)? {
        Some(p) => eprintln!("created {}", p.display()),
        None => eprintln!("config already exists: {}", config_path.display()),
    }
    Ok(())
}
