//! `mcp-proxy serve` — start the aggregator server.
//!
//! Boots the [`Hub`], spawns a filesystem watcher for hot config reloading,
//! then runs the selected transport (stdio or HTTP) until shutdown.

use std::sync::Arc;

use anyhow::Result;

use crate::server::Hub;

/// Entry point for the `serve` subcommand.
pub async fn run(config_path: &std::path::Path, transport: &str, port: u16) -> Result<()> {
    // Auto-create config if it doesn't exist
    if let Some(p) = crate::config::init_config(config_path)? {
        eprintln!("created starter config: {}", p.display());
        eprintln!("edit it to add your MCP servers, then restart.\n");
    }
    let raw = crate::config::load(config_path)?;
    let hub = Arc::new(Hub::new(raw).await?);

    // Hot config reload: watch the config file for changes using OS events
    let cfg_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    spawn_config_watcher(hub.clone(), cfg_path);

    // Run the transport, racing against ctrl+c for graceful shutdown.
    let result = tokio::select! {
        r = async {
            match transport {
                "stdio" => crate::transport::stdio::serve(hub.clone()).await,
                _ => crate::transport::http::serve(hub.clone(), port).await,
            }
        } => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };
    hub.shutdown().await;
    result
}

/// Watch the config file for changes using OS filesystem events (FSEvents on
/// macOS, inotify on Linux, ReadDirectoryChanges on Windows). When a write is
/// detected we debounce for 500ms (editors often do atomic save via tmp+rename)
/// then reload the config and apply it to the running hub.
fn spawn_config_watcher(hub: Arc<Hub>, config_path: std::path::PathBuf) {
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

    let mut watcher = notify::recommended_watcher(tx).expect("failed to create filesystem watcher");
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .expect("failed to watch config directory");

    std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || {
            let _watcher = watcher; // prevent drop
            let rt = tokio::runtime::Handle::current();
            let mut debounce: Option<std::time::Instant> = None;

            loop {
                let event = if debounce.is_some() {
                    match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(ev) => Some(ev),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(ev) => Some(ev),
                        Err(_) => break,
                    }
                };

                if let Some(Ok(ev)) = event {
                    let dominated = matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_));
                    let relevant_file = file_name.as_ref().is_none_or(|target| {
                        ev.paths
                            .iter()
                            .any(|p| p.file_name().is_some_and(|n| n == target))
                    });
                    if dominated && relevant_file {
                        debounce = Some(std::time::Instant::now());
                        continue;
                    }
                }

                if let Some(t) = debounce.take() {
                    if t.elapsed() >= std::time::Duration::from_millis(400) {
                        let hub = hub.clone();
                        let path = config_path.clone();
                        rt.spawn(async move {
                            tracing::info!("config file changed, reloading...");
                            match crate::config::load(&path) {
                                Ok(new_cfg) => hub.reload(new_cfg).await,
                                Err(e) => tracing::warn!("config reload failed: {e}"),
                            }
                        });
                    } else {
                        debounce = Some(t);
                    }
                }
            }
        })
        .expect("failed to spawn config watcher thread");
}
