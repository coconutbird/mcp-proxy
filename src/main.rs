//! Entry point for the `mcp-proxy` binary.
//!
//! This module only handles argument parsing and dispatching to the
//! appropriate command implementation in [`commands`].

mod backend;
mod cli;
mod clients;
mod commands;
mod config;
mod custom_tools;
mod docker;
mod jsonrpc;
mod pool;
mod server;
mod transport;
mod util;

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
            commands::cmd_serve(&args.config, transport, port).await
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
        cli::Cmd::Server { action } => commands::cmd_server(&args.config, action),
        cli::Cmd::Profile { action } => commands::cmd_profile(&args.config, action),
        cli::Cmd::Clients => commands::cmd_clients(profile.as_deref()),
        cli::Cmd::Health { port } => commands::cmd_health(port).await,
        cli::Cmd::Init => commands::cmd_init(&args.config),
        cli::Cmd::Test { servers } => commands::cmd_test(&args.config, &servers).await,
    }
}
