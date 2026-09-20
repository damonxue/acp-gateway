//! Standalone `agent-gateway` CLI binary.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gateway_cli::run().await
}
