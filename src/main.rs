mod backend;
mod cli;
mod clients;
mod config;
mod custom_tools;
mod docker;
mod server;
mod transport;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("mcp_proxy=info".parse()?))
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
            url,
            profile: bridge_profile,
            servers,
            forward_env,
        } => {
            let p = bridge_profile.as_deref().or(profile.as_deref());
            transport::bridge::run(&url, p, &servers, &forward_env, &args.config).await
        }
        cli::Cmd::Server { action } => cmd_server(&args.config, action),
        cli::Cmd::Profile { action } => cmd_profile(&args.config, action),
        cli::Cmd::Clients { port } => cmd_clients(port, profile.as_deref()),
        cli::Cmd::Health { port } => cmd_health(port).await,
        cli::Cmd::Init => cmd_init(&args.config),
    }
}

fn cmd_init(config_path: &std::path::Path) -> Result<()> {
    match config::init_config(config_path)? {
        Some(p) => eprintln!("created {}", p.display()),
        None => eprintln!("config already exists: {}", config_path.display()),
    }
    Ok(())
}

async fn cmd_serve(
    config_path: &std::path::Path,
    _profile: Option<&str>,
    transport: &str,
    port: u16,
) -> Result<()> {
    // Auto-create config if it doesn't exist
    if let Some(p) = config::init_config(config_path)? {
        eprintln!("created starter config: {}", p.display());
        eprintln!("edit it to add your MCP servers, then restart.\n");
    }
    let raw = config::load(config_path)?;
    let hub = Arc::new(server::Hub::new(raw).await?);

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
        .with_prompt("Select clients to install mcp-proxy to")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;

    let self_exe = std::env::current_exe()?.display().to_string();
    let selected_set: std::collections::HashSet<usize> = selections.into_iter().collect();

    for (i, client) in available.iter().enumerate() {
        if selected_set.contains(&i) {
            let was_installed = clients::is_installed(client);
            clients::install(client, port, &self_exe, profile, &[])?;
            if was_installed {
                eprintln!("  🔄 synced {}", client.name);
            } else {
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

async fn cmd_health(port: u16) -> Result<()> {
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

// ---------------------------------------------------------------------------
// Server management
// ---------------------------------------------------------------------------

fn cmd_server(config_path: &std::path::Path, action: cli::ServerCmd) -> Result<()> {
    use cli::ServerCmd;

    match action {
        ServerCmd::List => {
            let cfg = config::load(config_path)?;
            if cfg.servers.is_empty() {
                eprintln!("no servers configured");
                return Ok(());
            }
            for (name, srv) in &cfg.servers {
                let install_info = match &srv.install {
                    Some(config::InstallConfig::Npm { package }) => {
                        format!(" (npm: {package})")
                    }
                    Some(config::InstallConfig::Pip { package }) => {
                        format!(" (pip: {package})")
                    }
                    Some(config::InstallConfig::Binary { binary, .. }) => {
                        format!(" (binary: {binary})")
                    }
                    Some(config::InstallConfig::Npx) => " (npx)".to_string(),
                    None => String::new(),
                };
                let rt = if srv.install.is_some() {
                    match srv.runtime {
                        config::Runtime::Docker => " [docker]",
                        config::Runtime::Local => " [local]",
                    }
                } else {
                    ""
                };
                eprintln!("  {name:<20} {}{install_info}{rt}", srv.command);
            }
            Ok(())
        }
        ServerCmd::Add {
            name,
            command,
            args,
            env,
            npm,
            pip,
            runtime,
        } => {
            let mut cfg = config::load(config_path)?;
            if cfg.servers.contains_key(&name) {
                anyhow::bail!("server '{name}' already exists — use `server edit` to modify it");
            }

            let install = match (npm, pip) {
                (Some(pkg), _) => Some(config::InstallConfig::Npm { package: pkg }),
                (_, Some(pkg)) => Some(config::InstallConfig::Pip { package: pkg }),
                _ => None,
            };

            let rt = match runtime.as_deref() {
                Some("local") => config::Runtime::Local,
                _ => config::Runtime::Docker,
            };

            cfg.servers.insert(
                name.clone(),
                config::ServerConfig {
                    install,
                    runtime: rt,
                    command,
                    args,
                    env: env.into_iter().collect(),
                    env_toggle: None,
                    shared: Default::default(),
                },
            );

            config::save(config_path, &cfg)?;
            eprintln!("✅ added server '{name}'");
            Ok(())
        }
        ServerCmd::Remove { name } => {
            let mut cfg = config::load(config_path)?;
            if cfg.servers.remove(&name).is_none() {
                anyhow::bail!("server '{name}' not found");
            }
            config::save(config_path, &cfg)?;
            eprintln!("✅ removed server '{name}'");
            Ok(())
        }
        ServerCmd::Edit {
            name,
            command,
            args,
            env,
            remove_env,
            runtime,
        } => {
            let mut cfg = config::load(config_path)?;
            let srv = cfg
                .servers
                .get_mut(&name)
                .ok_or_else(|| anyhow::anyhow!("server '{name}' not found"))?;

            if let Some(cmd) = command {
                srv.command = cmd;
            }
            if let Some(a) = args {
                srv.args = a;
            }
            for (k, v) in env {
                srv.env.insert(k, v);
            }
            for k in remove_env {
                srv.env.remove(&k);
            }
            if let Some(rt) = runtime {
                srv.runtime = match rt.as_str() {
                    "local" => config::Runtime::Local,
                    _ => config::Runtime::Docker,
                };
            }

            config::save(config_path, &cfg)?;
            eprintln!("✅ updated server '{name}'");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Profile management
// ---------------------------------------------------------------------------

fn cmd_profile(config_path: &std::path::Path, action: cli::ProfileCmd) -> Result<()> {
    use cli::ProfileCmd;

    match action {
        ProfileCmd::List => {
            let pf = config::load_profiles(config_path)?;
            let active = config::read_active_profile();
            let profiles = config::list_profiles(&pf);

            if profiles.is_empty() {
                eprintln!("no profiles defined");
                return Ok(());
            }

            for (name, desc) in &profiles {
                let marker = if active.as_deref() == Some(*name) {
                    " ◀ active"
                } else {
                    ""
                };
                let description = desc.unwrap_or("(no description)");
                eprintln!("  {name:<16} {description}{marker}");

                if let Some(p) = pf.profiles.get(*name) {
                    for srv in p.servers.keys() {
                        eprintln!("    └─ {srv} (override)");
                    }
                }
            }

            if let Some(ref a) = active {
                eprintln!("\nactive profile: {a}");
            } else {
                eprintln!("\nno active profile (using all servers)");
            }
            Ok(())
        }
        ProfileCmd::Add { name, description } => {
            let mut pf = config::load_profiles(config_path)?;
            if pf.profiles.contains_key(&name) {
                anyhow::bail!("profile '{name}' already exists");
            }
            pf.profiles.insert(
                name.clone(),
                config::ProfileConfig {
                    description,
                    ..Default::default()
                },
            );
            config::save_profiles(config_path, &pf)?;
            eprintln!("✅ created profile '{name}'");
            Ok(())
        }
        ProfileCmd::Remove { name } => {
            let mut pf = config::load_profiles(config_path)?;
            if pf.profiles.remove(&name).is_none() {
                anyhow::bail!("profile '{name}' not found");
            }
            if config::read_active_profile().as_deref() == Some(&name) {
                config::write_active_profile(None)?;
            }
            config::save_profiles(config_path, &pf)?;
            eprintln!("✅ removed profile '{name}'");
            Ok(())
        }
        ProfileCmd::Set {
            profile,
            server,
            command,
            args,
            env,
            runtime,
        } => {
            let mut pf = config::load_profiles(config_path)?;
            let p = pf
                .profiles
                .get_mut(&profile)
                .ok_or_else(|| anyhow::anyhow!("profile '{profile}' not found"))?;

            let ovr = p.servers.entry(server.clone()).or_default();

            if let Some(cmd) = command {
                ovr.command = Some(cmd);
            }
            if let Some(a) = args {
                ovr.args = Some(a);
            }
            if !env.is_empty() {
                let e = ovr.env.get_or_insert_with(HashMap::new);
                for (k, v) in env {
                    e.insert(k, v);
                }
            }
            if let Some(rt) = runtime {
                ovr.runtime = Some(match rt.as_str() {
                    "local" => config::Runtime::Local,
                    _ => config::Runtime::Docker,
                });
            }

            config::save_profiles(config_path, &pf)?;
            eprintln!("✅ set override for '{server}' in profile '{profile}'");
            Ok(())
        }
        ProfileCmd::Unset { profile, server } => {
            let mut pf = config::load_profiles(config_path)?;
            let p = pf
                .profiles
                .get_mut(&profile)
                .ok_or_else(|| anyhow::anyhow!("profile '{profile}' not found"))?;

            if p.servers.remove(&server).is_none() {
                anyhow::bail!("no override for '{server}' in profile '{profile}'");
            }

            config::save_profiles(config_path, &pf)?;
            eprintln!("✅ removed override for '{server}' from profile '{profile}'");
            Ok(())
        }
        ProfileCmd::Switch { name } => {
            let pf = config::load_profiles(config_path)?;
            match name {
                Some(ref n) => {
                    if !pf.profiles.contains_key(n.as_str()) {
                        anyhow::bail!("profile '{n}' not found");
                    }
                    config::write_active_profile(Some(n))?;
                    eprintln!("✅ switched to profile: {n}");
                }
                None => {
                    config::write_active_profile(None)?;
                    eprintln!("✅ cleared active profile (using all servers)");
                }
            }
            Ok(())
        }
    }
}
