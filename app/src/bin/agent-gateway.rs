//! Bundle-local wrapper for the gateway CLI.
//!
//! Zed invokes this binary from the installed App bundle. It links the
//! `gateway-cli` library directly, so `acp-bridge` does not need to discover a
//! separate source checkout or daemon executable.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gateway_cli::run().await
}
