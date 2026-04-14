//! CLI command implementations.
//!
//! Each subcommand (`serve`, `server`, `profile`, etc.) lives in its own
//! module, keeping `main.rs` thin and focused on argument parsing + dispatch.

mod clients;
mod health;
mod init;
mod profile;
mod serve;
mod server;
mod test;

pub use clients::run as cmd_clients;
pub use health::run as cmd_health;
pub use init::run as cmd_init;
pub use profile::run as cmd_profile;
pub use serve::run as cmd_serve;
pub use server::run as cmd_server;
pub use test::run as cmd_test;
