//! `agent-gateway` — the command line entry point.
//!
//! This crate is the composition root: it is the only place that knows every
//! other crate exists, and the only place where concrete adapters are chosen.
//! Everything below it depends on traits.
//!
//! ```text
//!   config ──► store ──► SessionManager ◄── acp runtime
//!      │        │            ▲
//!      │        └─► auth ────┘
//!      └──────────────► remote server ──► tunnel + relay
//! ```

#![forbid(unsafe_code)]

mod client;
mod commands;
mod daemon;
mod logging;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Run coding agents locally; drive them from anywhere.
#[derive(Debug, Parser)]
#[command(name = "agent-gateway", version, about, long_about = None)]
struct Cli {
    /// Configuration file. Defaults to `$AGENT_GATEWAY_CONFIG` or
    /// `~/.agent-gateway/config.toml`.
    #[arg(long, short, global = true, env = "AGENT_GATEWAY_CONFIG")]
    config: Option<PathBuf>,

    /// Override the log filter (`info`, `debug`, `gateway_acp=trace`, …).
    #[arg(long, global = true, env = "AGENT_GATEWAY_LOG")]
    log: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the gateway daemon. This is the default.
    Run,
    /// Inspect and create configuration.
    #[command(subcommand)]
    Config(commands::config::ConfigCommand),
    /// Pair a phone or browser with this machine.
    Pair(commands::pair::PairArgs),
    /// Manage paired devices.
    #[command(subcommand)]
    Devices(commands::devices::DeviceCommand),
    /// List the agents this gateway can launch.
    Agents,
    /// Inspect and drive sessions through the running daemon.
    #[command(subcommand)]
    Sessions(commands::sessions::SessionCommand),
    /// Run the reference relay (development and self-hosting).
    Relay(commands::relay::RelayArgs),
    /// Proxy an IDE-owned ACP session into the gateway daemon.
    AcpBridge(commands::acp_bridge::AcpBridgeArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Run);

    // Commands that do not act on a local gateway are dispatched before the
    // configuration is loaded: `config` must work when the file is missing or
    // broken, and the reference relay is a standalone server that has no
    // gateway configuration of its own.
    match command {
        Command::Config(command) => {
            logging::init_cli(cli.log.as_deref());
            return commands::config::run(command, cli.config).await;
        }
        Command::Relay(args) => {
            logging::init_daemon(cli.log.as_deref().unwrap_or("info"));
            return commands::relay::run(args).await;
        }
        _ => {}
    }

    let path = cli
        .config
        .unwrap_or_else(gateway_config::default_config_path);
    let config = commands::config::load_or_explain(&path)?;

    match command {
        Command::Run => {
            logging::init_daemon(cli.log.as_deref().unwrap_or(&config.gateway.log_level));
            daemon::run(config).await
        }
        other => {
            logging::init_cli(cli.log.as_deref());
            match other {
                Command::Pair(args) => commands::pair::run(&config, args).await,
                Command::Devices(command) => commands::devices::run(&config, command).await,
                Command::Agents => commands::agents::run(&config).await,
                Command::Sessions(command) => commands::sessions::run(&config, command).await,
                Command::AcpBridge(args) => commands::acp_bridge::run(&config, args).await,
                Command::Run | Command::Config(_) | Command::Relay(_) => {
                    unreachable!("handled above")
                }
            }
        }
    }
}
