//! `mcp-proxy profile` — manage profiles and per-profile overrides.

use std::collections::HashMap;

use anyhow::Result;

use crate::cli::ProfileCmd;
use crate::config;

pub fn run(config_path: &std::path::Path, action: ProfileCmd) -> Result<()> {
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
