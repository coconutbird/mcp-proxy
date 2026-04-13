use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mcp-hub",
    version,
    about = "MCP proxy that aggregates multiple MCP servers"
)]
pub struct Cli {
    /// Path to servers.json config file
    #[arg(
        short,
        long,
        env = "CONFIG_PATH",
        default_value = "config/servers.json"
    )]
    pub config: PathBuf,

    /// Profile to use (overrides saved active profile)
    #[arg(long, env = "MCP_PROFILE")]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Start the aggregator (default mode)
    Serve {
        /// Transport: http or stdio
        #[arg(short, long, env = "MCP_TRANSPORT", default_value = "http")]
        transport: String,
        /// Port for HTTP transport
        #[arg(short, long, env = "MCP_PORT", default_value_t = 3000)]
        port: u16,
    },
    /// Stdio-to-HTTP bridge for clients that only support stdio
    Bridge {
        /// URL of the hub HTTP endpoint
        #[arg(long, default_value = "http://localhost:3000/mcp")]
        url: String,
        /// Profile override (passed by client config)
        #[arg(long)]
        profile: Option<String>,
        /// Comma-separated env var names to forward to the hub.
        /// The bridge reads these from its own process env and sends
        /// them as X-MCP-Env header so the hub can apply them to backends.
        #[arg(long, value_delimiter = ',')]
        forward_env: Vec<String>,
    },
    /// Generate Dockerfile and .env.example from config
    Generate {
        /// What to generate: all, dockerfile, env
        #[arg(default_value = "all")]
        target: String,
        /// Project directory (where Dockerfile is written)
        #[arg(short, long, default_value = ".")]
        dir: PathBuf,
    },
    /// Interactive: select which clients to install to
    Clients {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
    },
    /// Show status of client installations
    Status,
    /// Sync installed clients to current config
    Sync {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
    },
    /// Remove mcp-hub from all clients
    Uninstall,
    /// Check health of a running hub
    Health {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
    },
    /// List available profiles
    Profiles,
    /// Switch the active profile
    Switch {
        /// Profile name to activate (omit to clear/use all servers)
        profile: Option<String>,
    },
}
