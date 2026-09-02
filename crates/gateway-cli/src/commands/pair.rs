//! `agent-gateway pair`

use anyhow::Result;
use clap::Args;
use gateway_auth::PairingOffer;
use gateway_config::GatewayConfig;
use qrcode::QrCode;
use qrcode::render::unicode;

use crate::client::LocalClient;

#[derive(Debug, Args)]
pub(crate) struct PairArgs {
    /// Endpoint to put in the QR payload. Defaults to the tunnel's public URL,
    /// or the relay endpoint when there is no tunnel.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Print the payload as JSON instead of a QR code.
    #[arg(long)]
    pub json: bool,
}

/// Mint a pairing code and show it.
pub(crate) async fn run(config: &GatewayConfig, args: PairArgs) -> Result<()> {
    let client = LocalClient::new(config);
    let endpoint = args.endpoint.or_else(|| {
        config
            .tunnel
            .as_ref()
            .and_then(|tunnel| tunnel.hostname.as_ref())
            .map(|hostname| format!("https://{hostname}"))
            .or_else(|| config.relay.as_ref().map(|relay| relay.endpoint.clone()))
    });

    let offer: PairingOffer = client
        .post(
            "/pairing/begin",
            &serde_json::json!({ "endpoint": endpoint }),
        )
        .await?;
    let payload = offer.to_qr_payload()?;

    if args.json {
        println!("{payload}");
        return Ok(());
    }

    // A QR code the phone can scan, plus the same information in text for the
    // cases where a camera is not an option.
    let code = QrCode::new(payload.as_bytes())?;
    println!(
        "{}",
        code.render::<unicode::Dense1x2>().quiet_zone(true).build()
    );
    println!("pairing code   {}", offer.pairing_code);
    println!("machine        {}", offer.machine_id);
    println!(
        "endpoint       {}",
        offer.endpoint.as_deref().unwrap_or("(local only)")
    );
    println!("expires at     {}", offer.expires_at.to_rfc3339());
    println!();
    println!("Scan this within the validity window. The code is single-use.");
    Ok(())
}
