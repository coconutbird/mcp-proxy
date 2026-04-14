//! `mcp-proxy clients` — install/sync mcp-proxy to editor clients.

use anyhow::Result;

pub fn run(profile: Option<&str>) -> Result<()> {
    let all = crate::clients::known_clients();
    let available: Vec<_> = all
        .iter()
        .filter(|c| crate::clients::is_available(c))
        .collect();
    if available.is_empty() {
        eprintln!("no supported clients found");
        return Ok(());
    }

    let labels: Vec<String> = available
        .iter()
        .map(|c| {
            let tag = if crate::clients::is_installed(c) {
                " (installed)"
            } else {
                ""
            };
            format!("{}{tag}", c.name)
        })
        .collect();

    let defaults: Vec<bool> = available
        .iter()
        .map(|c| crate::clients::is_installed(c))
        .collect();
    let selections = dialoguer::MultiSelect::new()
        .with_prompt("Select clients to install mcp-proxy to")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;

    let self_exe = std::env::current_exe()?.display().to_string();
    let selected_set: std::collections::HashSet<usize> = selections.into_iter().collect();

    for (i, client) in available.iter().enumerate() {
        if selected_set.contains(&i) {
            let was_installed = crate::clients::is_installed(client);
            crate::clients::install(client, &self_exe, profile)?;
            if was_installed {
                eprintln!("  🔄 synced {}", client.name);
            } else {
                eprintln!("  ✅ installed to {}", client.name);
            }
        } else if crate::clients::is_installed(client) {
            crate::clients::uninstall(client)?;
            eprintln!("  🗑️  removed from {}", client.name);
        }
    }
    eprintln!("\nrestart clients to pick up changes.");
    Ok(())
}
