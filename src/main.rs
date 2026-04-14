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
        cli::Cmd::Clients => cmd_clients(profile.as_deref()),
        cli::Cmd::Health { port } => cmd_health(port).await,
        cli::Cmd::Init => cmd_init(&args.config),
        cli::Cmd::Test { servers } => cmd_test(&args.config, &servers).await,
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
        tracing::info!("shutting down");
        hub2.shutdown().await;
        std::process::exit(0);
    });

    // Hot config reload: watch the config file for changes using OS events
    let hub3 = hub.clone();
    let cfg_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    spawn_config_watcher(hub3, cfg_path);

    match transport {
        "stdio" => transport::stdio::serve(hub).await,
        _ => transport::http::serve(hub, port).await,
    }
}

/// Watch the config file for changes using OS filesystem events (FSEvents on
/// macOS, inotify on Linux, ReadDirectoryChanges on Windows). When a write is
/// detected we debounce for 500ms (editors often do atomic save via tmp+rename)
/// then reload the config and apply it to the running hub.
fn spawn_config_watcher(hub: Arc<server::Hub>, config_path: std::path::PathBuf) {
    use notify::{EventKind, RecursiveMode, Watcher};

    // We watch the **parent directory** because many editors do atomic saves
    // (write tmp → rename) which removes the inode. Watching the dir catches
    // both in-place writes and renames.
    let watch_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let file_name = config_path.file_name().map(|n| n.to_os_string());

    let (tx, rx) = std::sync::mpsc::channel();

    // Use the recommended watcher for this OS (FSEvents on macOS, inotify on Linux)
    let mut watcher = notify::recommended_watcher(tx).expect("failed to create filesystem watcher");
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .expect("failed to watch config directory");

    // Spawn a dedicated thread for the blocking recv + a tokio task for the async reload.
    // The thread owns the `watcher` (dropping it would stop watching).
    std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || {
            let _watcher = watcher; // prevent drop
            let rt = tokio::runtime::Handle::current();
            let mut debounce: Option<std::time::Instant> = None;

            loop {
                // Block until an event or timeout
                let event = if debounce.is_some() {
                    // In debounce window — poll with timeout
                    match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(ev) => Some(ev),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    // Idle — block indefinitely
                    match rx.recv() {
                        Ok(ev) => Some(ev),
                        Err(_) => break,
                    }
                };

                // If we got an event, check if it's relevant
                if let Some(Ok(ev)) = event {
                    let dominated = matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_));
                    let relevant_file = file_name.as_ref().is_none_or(|target| {
                        ev.paths
                            .iter()
                            .any(|p| p.file_name().is_some_and(|n| n == target))
                    });
                    if dominated && relevant_file {
                        debounce = Some(std::time::Instant::now());
                        continue; // restart debounce window
                    }
                }

                // Debounce expired — time to reload
                if let Some(t) = debounce.take() {
                    if t.elapsed() >= std::time::Duration::from_millis(400) {
                        let hub = hub.clone();
                        let path = config_path.clone();
                        rt.spawn(async move {
                            tracing::info!("config file changed, reloading...");
                            match config::load(&path) {
                                Ok(new_cfg) => hub.reload(new_cfg).await,
                                Err(e) => tracing::warn!("config reload failed: {e}"),
                            }
                        });
                    } else {
                        // Not enough time elapsed, re-enter debounce
                        debounce = Some(t);
                    }
                }
            }
        })
        .expect("failed to spawn config watcher thread");
}

fn cmd_clients(profile: Option<&str>) -> Result<()> {
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
            clients::install(client, &self_exe, profile)?;
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
                    timeout_secs: None,
                    idle_timeout_secs: None,
                    auto_restart: true,
                    max_restarts: None,
                    include_tools: Vec::new(),
                    exclude_tools: Vec::new(),
                    tool_aliases: HashMap::new(),
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

// ---------------------------------------------------------------------------
// Test command
// ---------------------------------------------------------------------------

async fn cmd_test(config_path: &std::path::Path, filter_servers: &[String]) -> Result<()> {
    eprintln!("🔍 validating config: {}", config_path.display());
    let cfg = config::load(config_path)?;
    eprintln!("  ✅ config parsed ({} server(s))\n", cfg.servers.len());

    let empty_env = HashMap::new();
    let mut pass = 0u32;
    let mut fail = 0u32;

    for (name, srv) in &cfg.servers {
        if name.starts_with('_') {
            continue;
        }
        if !filter_servers.is_empty() && !filter_servers.contains(name) {
            continue;
        }
        eprint!("  testing {name}...");

        match backend::Backend::start(name.clone(), srv, &empty_env).await {
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
        std::process::exit(1);
    }
    Ok(())
}
