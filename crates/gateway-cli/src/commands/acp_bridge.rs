//! `agent-gateway acp-bridge`
//!
//! This command is intentionally stdout-clean: stdout is the ACP transport
//! Zed reads. Diagnostics go through tracing/stderr only.

use anyhow::{Context, Result, bail};
use clap::Args;
use gateway_acp::{AcpBridgeConfig, BridgeControlMode};
use gateway_config::GatewayConfig;
use gateway_core::AgentId;

#[derive(Debug, Args)]
pub(crate) struct AcpBridgeArgs {
    /// Configured agent id to proxy, for example `codex`.
    #[arg(long)]
    pub agent: String,
    /// Override the daemon bridge WebSocket URL.
    #[arg(long)]
    pub daemon_url: Option<String>,
    /// Mirror the IDE session but reject remote prompts/cancels/permission decisions.
    #[arg(long)]
    pub read_only: bool,
}

/// Run an ACP proxy suitable for Zed `agent_servers`.
pub(crate) async fn run(config: &GatewayConfig, args: AcpBridgeArgs) -> Result<()> {
    let agent_id = AgentId::new(args.agent);
    let agent = config
        .agent_descriptors()
        .into_iter()
        .find(|candidate| candidate.id == agent_id)
        .with_context(|| format!("agent `{agent_id}` is not configured"))?;

    let mut bridge = AcpBridgeConfig::new(agent, config.bind_addr());
    if let Some(url) = args.daemon_url {
        if !url.starts_with("ws://127.0.0.1:")
            && !url.starts_with("ws://[::1]:")
            && !url.starts_with("ws://localhost:")
        {
            bail!("acp-bridge daemon URL must be loopback, got `{url}`");
        }
        bridge.daemon_url = url;
    }
    if args.read_only {
        bridge.control_mode = BridgeControlMode::ReadOnly;
    }

    gateway_acp::run_bridge(bridge)
        .await
        .map_err(anyhow::Error::from)
}
