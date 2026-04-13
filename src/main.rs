mod backend;
mod cli;
mod clients;
mod config;
mod custom_tools;
mod generate;
mod server;
mod transport;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("mcp_hub=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let args = cli::Cli::parse();

    // Resolve active profile: CLI flag > env > saved state
    let profile = args
        .profile
        .as_deref()
        .map(String::from)
        .or_else(config::read_active_profile);

    match args.command {
        cli::Cmd::Serve { transport, port } => {
            cmd_serve(&args.config, profile.as_deref(), &transport, port).await
        }
        cli::Cmd::Bridge {
            url, forward_env, ..
        } => transport::bridge::run(&url, &forward_env).await,
        cli::Cmd::Generate { target, dir } => {
            cmd_generate(&args.config, profile.as_deref(), &target, &dir)
        }
        cli::Cmd::Clients { port } => cmd_clients(port, profile.as_deref()),
        cli::Cmd::Status => cmd_status(),
        cli::Cmd::Sync { port } => cmd_sync(port, profile.as_deref()),
        cli::Cmd::Uninstall => cmd_uninstall(),
        cli::Cmd::Health { port } => cmd_health(port).await,
        cli::Cmd::Profiles => cmd_profiles(&args.config),
        cli::Cmd::Switch { profile: p } => cmd_switch(&args.config, p.as_deref()),
    }
}

async fn cmd_serve(
    config_path: &std::path::Path,
    profile: Option<&str>,
    transport: &str,
    port: u16,
) -> Result<()> {
    let cfg = config::load_with_profile(config_path, profile)?;
    let hub = Arc::new(server::Hub::from_config(&cfg).await?);

    // Shutdown on ctrl-c
    let hub2 = hub.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\nshutting down...");
        hub2.shutdown().await;
        std::process::exit(0);
    });

    match transport {
        "stdio" => transport::stdio::serve(hub).await,
        _ => transport::http::serve(hub, port).await,
    }
}

fn cmd_generate(
    config_path: &std::path::Path,
    profile: Option<&str>,
    target: &str,
    dir: &std::path::Path,
) -> Result<()> {
    // Generate uses the full (unresolved) config for Dockerfile since it needs all servers
    let cfg = config::load(config_path)?;
    let _ = profile; // profiles don't affect generation (we want all servers in Docker)
    match target {
        "dockerfile" => generate::dockerfile(&cfg, &dir.join("Dockerfile")),
        "env" => generate::env_example(&cfg, &dir.join(".env.example")),
        _ => {
            generate::dockerfile(&cfg, &dir.join("Dockerfile"))?;
            generate::env_example(&cfg, &dir.join(".env.example"))
        }
    }
}

fn cmd_clients(port: u16, profile: Option<&str>) -> Result<()> {
    let all = clients::known_clients();
    let available: Vec<_> = all.iter().filter(|c| clients::is_available(c)).collect();
    if available.is_empty() {
        eprintln!("no supported clients found");
        return Ok(());
    }

    let labels: Vec<String> = available
        .iter()
        .map(|c| {
            let tag = if clients::is_installed(c) {
                " (installed)"
            } else {
                ""
            };
            format!("{}{tag}", c.name)
        })
        .collect();

    let defaults: Vec<bool> = available.iter().map(|c| clients::is_installed(c)).collect();
    let selections = dialoguer::MultiSelect::new()
        .with_prompt("Select clients to install mcp-hub to")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;

    let self_exe = std::env::current_exe()?.display().to_string();
    let selected_set: std::collections::HashSet<usize> = selections.into_iter().collect();

    for (i, client) in available.iter().enumerate() {
        if selected_set.contains(&i) {
            if !clients::is_installed(client) {
                clients::install(client, port, &self_exe, profile, &[])?;
                eprintln!("  ✅ installed to {}", client.name);
            }
        } else if clients::is_installed(client) {
            clients::uninstall(client)?;
            eprintln!("  🗑️  removed from {}", client.name);
        }
    }
    eprintln!("\nrestart clients to pick up changes.");
    Ok(())
}

fn cmd_status() -> Result<()> {
    eprintln!("\n📊 mcp-hub status\n");
    for client in &clients::known_clients() {
        let avail = clients::is_available(client);
        let inst = avail && clients::is_installed(client);
        let icon = if !avail {
            "⚪ not found"
        } else if inst {
            "✅ installed"
        } else {
            "❌ not installed"
        };
        eprintln!("  {:<20} {icon}", client.name);
    }
    Ok(())
}

fn cmd_sync(port: u16, profile: Option<&str>) -> Result<()> {
    let self_exe = std::env::current_exe()?.display().to_string();
    for client in &clients::known_clients() {
        if !clients::is_installed(client) {
            continue;
        }
        clients::install(client, port, &self_exe, profile, &[])?;
        eprintln!("  🔧 synced {}", client.name);
    }
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    for client in &clients::known_clients() {
        if clients::uninstall(client)? {
            eprintln!("  ✅ removed from {}", client.name);
        }
    }
    Ok(())
}

async fn cmd_health(port: u16) -> Result<()> {
    let url = format!("http://localhost:{port}/health");
    let resp = reqwest::get(&url).await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let data: serde_json::Value = r.json().await?;
            eprintln!("✅ hub healthy\n{}", serde_json::to_string_pretty(&data)?);
        }
        Ok(r) => eprintln!("❌ HTTP {}", r.status()),
        Err(e) => eprintln!("❌ cannot reach {url}: {e}"),
    }
    Ok(())
}

fn cmd_profiles(config_path: &std::path::Path) -> Result<()> {
    let cfg = config::load(config_path)?;
    let active = config::read_active_profile();
    let profiles = config::list_profiles(&cfg);

    if profiles.is_empty() {
        eprintln!("no profiles defined in {}", config_path.display());
        eprintln!("\nadd a \"profiles\" section to your config, e.g.:");
        eprintln!("  \"profiles\": {{");
        eprintln!("    \"work\": {{ \"include\": [\"github\", \"memory\"] }},");
        eprintln!("    \"home\": {{ \"exclude\": [\"github\"] }}");
        eprintln!("  }}");
        return Ok(());
    }

    eprintln!("\n📋 Available profiles:\n");
    for (name, desc) in &profiles {
        let marker = if active.as_deref() == Some(*name) {
            " ◀ active"
        } else {
            ""
        };
        let description = desc.unwrap_or("(no description)");
        eprintln!("  {name:<16} {description}{marker}");
    }

    if let Some(ref a) = active {
        eprintln!("\nactive profile: {a}");
    } else {
        eprintln!("\nno active profile (using all servers)");
    }
    Ok(())
}

fn cmd_switch(config_path: &std::path::Path, profile: Option<&str>) -> Result<()> {
    let cfg = config::load(config_path)?;

    match profile {
        Some(name) => {
            // Validate the profile exists
            let resolved = config::resolve(&cfg, Some(name))?;
            config::write_active_profile(Some(name))?;
            eprintln!("✅ switched to profile: {name}");
            eprintln!(
                "   {} server(s), {} custom tool(s)",
                resolved.servers.len(),
                resolved.custom_tools.len()
            );
            eprintln!("\nrestart the hub and clients to apply.");
        }
        None => {
            config::write_active_profile(None)?;
            eprintln!("✅ cleared active profile (using all servers)");
            eprintln!("\nrestart the hub and clients to apply.");
        }
    }
    Ok(())
}
