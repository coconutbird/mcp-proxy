//! `mcp-proxy profile` — manage profiles and per-profile overrides.

use std::collections::HashMap;

use anyhow::Result;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::cli::ProfileCmd;
use crate::config;
use crate::config::ServerOverride;

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
                ovr.runtime = Some(rt.parse().map_err(|e: String| anyhow::anyhow!(e))?);
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
        ProfileCmd::Edit { name } => {
            run_interactive_edit(config_path, name.as_deref())?;
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
// Interactive profile editor
// ---------------------------------------------------------------------------

/// Resolve the target profile name. If `name` is None, use the active profile.
/// If there's no active profile, let the user pick from the list.
fn resolve_profile_name(config_path: &std::path::Path, name: Option<&str>) -> Result<String> {
    if let Some(n) = name {
        return Ok(n.to_string());
    }
    if let Some(active) = config::read_active_profile() {
        return Ok(active);
    }
    // No active profile — prompt the user to pick one
    let pf = config::load_profiles(config_path)?;
    let names: Vec<&String> = pf.profiles.keys().collect();
    if names.is_empty() {
        anyhow::bail!("no profiles exist — create one first with `profile add`");
    }
    let labels: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
    let idx = Select::new()
        .with_prompt("Select a profile to edit")
        .items(&labels)
        .interact()?;
    Ok(names[idx].clone())
}

fn run_interactive_edit(config_path: &std::path::Path, name: Option<&str>) -> Result<()> {
    let profile_name = resolve_profile_name(config_path, name)?;

    let cfg = config::load(config_path)?;
    let mut pf = config::load_profiles(config_path)?;

    // Ensure the profile exists (auto-create if it doesn't)
    if !pf.profiles.contains_key(&profile_name) {
        if Confirm::new()
            .with_prompt(format!(
                "Profile '{profile_name}' doesn't exist. Create it?"
            ))
            .default(true)
            .interact()?
        {
            pf.profiles
                .insert(profile_name.clone(), config::ProfileConfig::default());
        } else {
            return Ok(());
        }
    }

    let all_servers: Vec<String> = {
        let mut names: Vec<String> = cfg.servers.keys().cloned().collect();
        names.sort();
        names
    };

    if all_servers.is_empty() {
        eprintln!("no servers defined in config");
        return Ok(());
    }

    loop {
        let profile = pf.profiles.get(&profile_name).unwrap();

        // Build display labels with current state
        let labels: Vec<String> = all_servers
            .iter()
            .map(|name| {
                let included = profile.include.is_empty() || profile.include.contains(name);
                let has_override = profile.servers.contains_key(name);
                let status = if included { "✅" } else { "⬚ " };
                let ovr_tag = if has_override { " ⚙" } else { "" };
                format!("{status} {name}{ovr_tag}")
            })
            .collect();

        // Show menu: toggle servers, edit overrides, or done
        let mut menu = labels.clone();
        menu.push("─────────────────".into());
        menu.push("💾 Save & exit".into());
        menu.push("❌ Discard & exit".into());

        eprintln!("\n── Profile: {profile_name} ──");
        if !profile.include.is_empty() {
            eprintln!(
                "   {} of {} servers enabled",
                profile.include.len(),
                all_servers.len()
            );
        } else {
            eprintln!(
                "   all {} servers enabled (no include filter)",
                all_servers.len()
            );
        }

        let choice = Select::new()
            .with_prompt("Select a server to toggle/edit, or save/discard")
            .items(&menu)
            .default(0)
            .interact()?;

        let separator_idx = labels.len();
        let save_idx = separator_idx + 1;
        let discard_idx = separator_idx + 2;

        if choice == discard_idx {
            eprintln!("discarded changes");
            return Ok(());
        }
        if choice == save_idx {
            config::save_profiles(config_path, &pf)?;
            eprintln!("✅ saved profile '{profile_name}'");
            return Ok(());
        }
        if choice == separator_idx {
            continue;
        }

        // A server was selected
        let server_name = &all_servers[choice];
        edit_server_in_profile(&mut pf, &profile_name, server_name, &cfg)?;
    }
}

/// Sub-menu for a single server within a profile.
fn edit_server_in_profile(
    pf: &mut config::ProfilesFile,
    profile_name: &str,
    server_name: &str,
    cfg: &config::Config,
) -> Result<()> {
    let profile = pf.profiles.get(profile_name).unwrap();
    let included = profile.include.is_empty() || profile.include.contains(&server_name.to_string());
    let base_srv = cfg.servers.get(server_name);
    let has_override = profile.servers.contains_key(server_name);

    // Build sub-menu
    let toggle_label = if included {
        "Disable (remove from include list)"
    } else {
        "Enable (add to include list)"
    };

    let mut items = vec![toggle_label.to_string()];
    items.push("Edit env overrides".into());
    items.push("Edit args override".into());
    items.push("Edit command override".into());
    if has_override {
        items.push("Clear all overrides".into());
    }
    items.push("← Back".into());

    eprintln!();
    if let Some(srv) = base_srv {
        eprintln!("  server: {server_name}");
        eprintln!("  command: {} {}", srv.command, srv.args.join(" "));
        if !srv.env.is_empty() {
            let keys: Vec<&str> = srv.env.keys().map(|k| k.as_str()).collect();
            eprintln!("  env: {}", keys.join(", "));
        }
    }
    if let Some(ovr) = profile.servers.get(server_name)
        && (ovr.command.is_some() || ovr.args.is_some() || ovr.env.is_some())
    {
        eprintln!("  overrides:");
        if let Some(ref cmd) = ovr.command {
            eprintln!("    command: {cmd}");
        }
        if let Some(ref args) = ovr.args {
            eprintln!("    args: {}", args.join(" "));
        }
        if let Some(ref env) = ovr.env {
            for (k, v) in env {
                eprintln!("    {k}={v}");
            }
        }
    }

    let choice = Select::new()
        .with_prompt(format!("[{server_name}]"))
        .items(&items)
        .default(0)
        .interact()?;

    let back_idx = items.len() - 1;
    let clear_idx = if has_override {
        back_idx - 1
    } else {
        usize::MAX
    };

    if choice == back_idx {
        return Ok(());
    }

    let profile = pf.profiles.get_mut(profile_name).unwrap();

    match choice {
        // Toggle
        0 => {
            if included {
                // If include list is empty (meaning "all"), populate it with all-but-this
                if profile.include.is_empty() {
                    let all: Vec<String> = cfg
                        .servers
                        .keys()
                        .filter(|n| *n != server_name)
                        .cloned()
                        .collect();
                    profile.include = all;
                } else {
                    profile.include.retain(|n| n != server_name);
                }
                eprintln!("  ⬚  disabled {server_name}");
            } else {
                profile.include.push(server_name.to_string());
                profile.include.sort();
                eprintln!("  ✅ enabled {server_name}");
            }
        }
        // Edit env
        1 => {
            edit_env_overrides(profile, server_name)?;
        }
        // Edit args
        2 => {
            edit_args_override(profile, server_name)?;
        }
        // Edit command
        3 => {
            edit_command_override(profile, server_name)?;
        }
        // Clear overrides
        c if c == clear_idx => {
            profile.servers.remove(server_name);
            eprintln!("  cleared overrides for {server_name}");
        }
        _ => {}
    }

    Ok(())
}

fn edit_env_overrides(profile: &mut config::ProfileConfig, server_name: &str) -> Result<()> {
    let ovr = profile.servers.entry(server_name.to_string()).or_default();
    let env = ovr.env.get_or_insert_with(HashMap::new);

    // Show existing env vars and let user edit them
    if !env.is_empty() {
        eprintln!("\n  current env overrides:");
        let keys: Vec<String> = env.keys().cloned().collect();
        for k in &keys {
            eprintln!("    {k}={}", env[k]);
        }

        // Ask if they want to remove any
        let remove_labels: Vec<String> = keys.iter().map(|k| format!("{k}={}", env[k])).collect();
        let to_remove = MultiSelect::new()
            .with_prompt("Select env vars to remove (space to toggle, enter to confirm)")
            .items(&remove_labels)
            .interact()?;
        for idx in to_remove.iter().rev() {
            env.remove(&keys[*idx]);
        }
    }

    // Add new env vars
    loop {
        let key: String = Input::new()
            .with_prompt("New env var name (empty to finish)")
            .allow_empty(true)
            .interact_text()?;
        if key.is_empty() {
            break;
        }
        let value: String = Input::new()
            .with_prompt(format!("Value for {key}"))
            .allow_empty(true)
            .interact_text()?;
        env.insert(key, value);
    }

    // Clean up empty override
    if env.is_empty() {
        ovr.env = None;
    }
    cleanup_empty_override(profile, server_name);
    Ok(())
}

fn edit_args_override(profile: &mut config::ProfileConfig, server_name: &str) -> Result<()> {
    let ovr = profile.servers.entry(server_name.to_string()).or_default();

    let current = ovr.args.as_ref().map(|a| a.join(" ")).unwrap_or_default();

    let input: String = Input::new()
        .with_prompt("Args (space-separated, empty to clear)")
        .with_initial_text(current)
        .allow_empty(true)
        .interact_text()?;

    if input.is_empty() {
        ovr.args = None;
        eprintln!("  cleared args override");
    } else {
        let args: Vec<String> = input.split_whitespace().map(String::from).collect();
        ovr.args = Some(args);
        eprintln!("  set args override");
    }

    cleanup_empty_override(profile, server_name);
    Ok(())
}

fn edit_command_override(profile: &mut config::ProfileConfig, server_name: &str) -> Result<()> {
    let ovr = profile.servers.entry(server_name.to_string()).or_default();

    let current = ovr.command.as_deref().unwrap_or("").to_string();

    let input: String = Input::new()
        .with_prompt("Command (empty to clear)")
        .with_initial_text(current)
        .allow_empty(true)
        .interact_text()?;

    if input.is_empty() {
        ovr.command = None;
        eprintln!("  cleared command override");
    } else {
        ovr.command = Some(input);
        eprintln!("  set command override");
    }

    cleanup_empty_override(profile, server_name);
    Ok(())
}

/// Remove the server override entry if all fields are empty/None.
fn cleanup_empty_override(profile: &mut config::ProfileConfig, server_name: &str) {
    if let Some(ovr) = profile.servers.get(server_name) {
        let empty = ServerOverride::default();
        if *ovr == empty {
            profile.servers.remove(server_name);
        }
    }
}
