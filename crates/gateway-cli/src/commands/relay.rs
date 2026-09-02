//! `agent-gateway relay`

use std::net::SocketAddr;

use anyhow::Result;
use clap::Args;
use gateway_relay::RelayState;

#[derive(Debug, Args)]
pub(crate) struct RelayArgs {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:48200")]
    pub bind: SocketAddr,
}

/// Run the reference relay.
///
/// This is the control plane a phone talks to when it is not on the same
/// network. It stores machine directory entries and presence — never prompts,
/// agent output or credentials — and it is deliberately in-memory: restarting
/// it costs a re-registration, which every gateway does automatically.
pub(crate) async fn run(args: RelayArgs) -> Result<()> {
    // The relay is a server in its own right; give it real logs.
    tracing::info!(address = %args.bind, "starting the reference relay");
    eprintln!("reference relay listening on http://{}", args.bind);
    eprintln!(
        "point gateways at it with:\n\n[relay]\nendpoint = \"http://{}\"\n",
        args.bind
    );
    gateway_relay::serve_relay(args.bind, RelayState::new(), shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    tokio::signal::ctrl_c().await.ok();
}
