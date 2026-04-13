use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mcp-proxy",
    version,
    about = "MCP proxy that aggregates multiple MCP servers"
)]
pub struct Cli {
    /// Path to servers.json config file
    #[arg(short, long, env = "CONFIG_PATH", default_value_os_t = crate::config::default_config_path())]
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
        #[arg(long, value_delimiter = ',')]
        forward_env: Vec<String>,
    },
    /// Manage servers in the config
    Server {
        #[command(subcommand)]
        action: ServerCmd,
    },
    /// Manage profiles and per-profile overrides
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
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
    /// Remove mcp-proxy from all clients
    Uninstall,
    /// Check health of a running hub
    Health {
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
    },
    /// Create a starter servers.json config
    Init,
}

// ---------------------------------------------------------------------------
// Server subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ServerCmd {
    /// Add a new server
    Add {
        /// Server name (used as tool prefix)
        name: String,
        /// Command to run the server
        #[arg(long)]
        command: String,
        /// Arguments for the command
        #[arg(long, num_args = 1..)]
        args: Vec<String>,
        /// Environment variables (KEY=VALUE)
        #[arg(long, short, value_parser = parse_key_val, num_args = 1..)]
        env: Vec<(String, String)>,
        /// Install via npm package
        #[arg(long, group = "install_method")]
        npm: Option<String>,
        /// Install via pip package
        #[arg(long, group = "install_method")]
        pip: Option<String>,
        /// Runtime: docker or local (default: docker when install is set)
        #[arg(long)]
        runtime: Option<String>,
    },
    /// Remove a server
    Remove {
        /// Server name to remove
        name: String,
    },
    /// Edit an existing server
    Edit {
        /// Server name to edit
        name: String,
        /// New command
        #[arg(long)]
        command: Option<String>,
        /// New arguments (replaces existing)
        #[arg(long, num_args = 1..)]
        args: Option<Vec<String>>,
        /// Environment variables to set/update (KEY=VALUE)
        #[arg(long, short, value_parser = parse_key_val, num_args = 1..)]
        env: Vec<(String, String)>,
        /// Remove environment variables by key
        #[arg(long, num_args = 1..)]
        remove_env: Vec<String>,
        /// Runtime: docker or local
        #[arg(long)]
        runtime: Option<String>,
    },
    /// List all servers
    List,
}

// ---------------------------------------------------------------------------
// Profile subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ProfileCmd {
    /// List all profiles
    List,
    /// Create a new profile
    Add {
        /// Profile name
        name: String,
        /// Description
        #[arg(long)]
        description: Option<String>,
    },
    /// Remove a profile
    Remove {
        /// Profile name
        name: String,
    },
    /// Set per-profile overrides for a server
    Set {
        /// Profile name
        profile: String,
        /// Server name to override
        server: String,
        /// Override command
        #[arg(long)]
        command: Option<String>,
        /// Override arguments (replaces base)
        #[arg(long, num_args = 1..)]
        args: Option<Vec<String>>,
        /// Override/add env vars (KEY=VALUE)
        #[arg(long, short, value_parser = parse_key_val, num_args = 1..)]
        env: Vec<(String, String)>,
        /// Runtime override: docker or local
        #[arg(long)]
        runtime: Option<String>,
    },
    /// Remove per-profile override for a server
    Unset {
        /// Profile name
        profile: String,
        /// Server name to remove override for
        server: String,
    },
    /// Switch the active profile
    Switch {
        /// Profile name (omit to clear)
        name: Option<String>,
    },
}

/// Parse KEY=VALUE pairs for --env flags.
fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid KEY=VALUE: no '=' in '{s}'"))?;
    Ok((s[..pos].to_string(), s[pos + 1..].to_string()))
}
