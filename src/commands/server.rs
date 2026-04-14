//! `mcp-proxy server` — manage servers in the config.

use anyhow::Result;

use crate::cli::ServerCmd;
use crate::config;

pub fn run(config_path: &std::path::Path, action: ServerCmd) -> Result<()> {
    match action {
        ServerCmd::List => {
            let cfg = config::load(config_path)?;
            if cfg.servers.is_empty() {
                eprintln!("no servers configured");
                return Ok(());
            }

            for (name, srv) in &cfg.servers {
                let install_info = srv
                    .install
                    .as_ref()
                    .map(|i| format!(" ({i})"))
                    .unwrap_or_default();
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

            let rt = runtime
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|e: String| anyhow::anyhow!(e))?
                .unwrap_or_default();

            cfg.servers.insert(
                name.clone(),
                config::ServerConfig {
                    install,
                    runtime: rt,
                    command,
                    args,
                    env: env.into_iter().collect(),
                    ..Default::default()
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
                srv.runtime = rt.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            }

            config::save(config_path, &cfg)?;
            eprintln!("✅ updated server '{name}'");
            Ok(())
        }
    }
}
