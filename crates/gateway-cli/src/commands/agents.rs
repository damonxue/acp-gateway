//! `agent-gateway agents`

use anyhow::Result;
use gateway_config::GatewayConfig;
use serde::Deserialize;

use crate::client::LocalClient;

#[derive(Debug, Deserialize)]
struct AgentSummary {
    id: String,
    name: String,
}

/// List the agents the running gateway can launch.
///
/// Read from the daemon rather than the config file, so what is printed is
/// what the daemon actually loaded — including after the file changed on disk.
pub(crate) async fn run(config: &GatewayConfig) -> Result<()> {
    let client = LocalClient::new(config);
    let agents: Vec<AgentSummary> = client.get("/agents").await?;
    if agents.is_empty() {
        println!("no agents configured; add an [[agents]] entry to your config");
        return Ok(());
    }
    for agent in agents {
        println!("{:<12} {}", agent.id, agent.name);
    }
    Ok(())
}
