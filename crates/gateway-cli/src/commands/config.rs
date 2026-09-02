//! `agent-gateway config …`

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use gateway_config::GatewayConfig;

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Write a starter configuration file.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Validate the configuration and print what the daemon would use.
    Check,
    /// Print the configuration path in use.
    Path,
}

/// Execute a `config` subcommand.
pub(crate) async fn run(command: ConfigCommand, path: Option<PathBuf>) -> Result<()> {
    let path = path.unwrap_or_else(gateway_config::default_config_path);
    match command {
        ConfigCommand::Init { force } => init(&path, force),
        ConfigCommand::Check => check(&path),
        ConfigCommand::Path => {
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, gateway_config::config_template())
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("wrote {}", path.display());
    println!("edit the [[agents]] entries, then run: agent-gateway run");
    Ok(())
}

fn check(path: &Path) -> Result<()> {
    let config = load_or_explain(path)?;
    println!("config      {}", path.display());
    println!("bind        {}", config.bind_addr());
    println!("data dir    {}", config.data_dir().display());
    println!("database    {}", config.database_path().display());
    println!(
        "relay       {}",
        config
            .relay
            .as_ref()
            .map_or_else(|| "disabled".to_owned(), |relay| relay.endpoint.clone())
    );
    println!(
        "tunnel      {}",
        config.tunnel.as_ref().map_or_else(
            || "disabled".to_owned(),
            |tunnel| format!(
                "{:?} -> {}",
                tunnel.mode,
                tunnel.hostname.as_deref().unwrap_or("(no hostname)")
            )
        )
    );
    println!("agents");
    for agent in &config.agents {
        println!(
            "  {:<10} {:<16} {} {}",
            agent.id,
            agent.name,
            agent.command,
            agent.args.join(" ")
        );
    }
    Ok(())
}

/// Load configuration, turning a missing file into an actionable message.
pub(crate) fn load_or_explain(path: &Path) -> Result<GatewayConfig> {
    if !path.exists() {
        bail!(
            "no configuration at {}; run `agent-gateway config init` to create one",
            path.display()
        );
    }
    GatewayConfig::load(path).map_err(Into::into)
}
