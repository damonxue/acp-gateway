//! `agent-gateway devices …`

use anyhow::Result;
use clap::Subcommand;
use gateway_config::GatewayConfig;
use gateway_core::device::Device;
use serde::Deserialize;

use crate::client::LocalClient;

#[derive(Debug, Subcommand)]
pub(crate) enum DeviceCommand {
    /// List paired devices.
    List,
    /// Revoke a device. It loses access immediately, including live tickets.
    Revoke {
        /// Device id, as shown by `devices list`.
        device_id: String,
    },
}

#[derive(Debug, Deserialize)]
struct DeviceList {
    devices: Vec<Device>,
}

/// Execute a `devices` subcommand.
pub(crate) async fn run(config: &GatewayConfig, command: DeviceCommand) -> Result<()> {
    let client = LocalClient::new(config);
    match command {
        DeviceCommand::List => {
            let list: DeviceList = client.get("/devices").await?;
            if list.devices.is_empty() {
                println!("no paired devices; run `agent-gateway pair`");
                return Ok(());
            }
            println!(
                "{:<26} {:<16} {:<8} {:<8} LAST SEEN",
                "ID", "NAME", "PLATFORM", "STATE"
            );
            for device in list.devices {
                println!(
                    "{:<26} {:<16} {:<8} {:<8} {}",
                    device.id,
                    device.name,
                    device.platform,
                    if device.revoked { "revoked" } else { "active" },
                    device
                        .last_seen_at
                        .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339())
                );
            }
            Ok(())
        }
        DeviceCommand::Revoke { device_id } => {
            let _: serde_json::Value = client
                .post(
                    &format!("/devices/{device_id}/revoke"),
                    &serde_json::json!({}),
                )
                .await?;
            println!("revoked {device_id}");
            Ok(())
        }
    }
}
