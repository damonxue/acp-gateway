//! # gateway-auth
//!
//! Who is allowed to talk to this gateway, and for how long.
//!
//! Three concerns, one service:
//!
//! | Concern | Type | Lifetime |
//! |---|---|---|
//! | "which machine is this?" | [`MachineIdentity`] | forever, private key stays local |
//! | "which phone is this?" | [`gateway_core::Device`] | until revoked |
//! | "may this socket open?" | [`gateway_core::WsTicket`] | seconds, single use |
//!
//! The security rules the PRD states — short-lived, scoped, one-time,
//! audience-bound, replay-protected tickets — are enforced in
//! [`AuthService::redeem_ticket`] and nowhere else, so the HTTP layer cannot
//! forget one of them.

#![forbid(unsafe_code)]

mod identity;
mod random;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gateway_core::credential::{PairingCode, WsTicket};
use gateway_core::device::Device;
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::{DeviceId, MachineId};
use gateway_core::ports::{Clock, DeviceRepository, PairingRepository, TicketRepository};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

pub use identity::MachineIdentity;
pub use random::{numeric_code, random_token};

/// Credential lifetimes.
#[derive(Clone, Copy, Debug)]
pub struct AuthConfig {
    /// How long a WebSocket ticket stays redeemable.
    pub ticket_ttl: Duration,
    /// How long a pairing code stays redeemable.
    pub pairing_ttl: Duration,
}

/// How far a signed ticket request's timestamp may be from the gateway's clock.
pub const TICKET_SIGNATURE_WINDOW: Duration = Duration::from_secs(120);

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            ticket_ttl: Duration::from_secs(60),
            pairing_ttl: Duration::from_secs(300),
        }
    }
}

/// What the CLI renders as a QR code and the phone scans.
///
/// Field names match the PRD's QR payload exactly, because a mobile client is
/// written against that document, not against this struct.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingOffer {
    /// Machine being paired.
    pub machine_id: MachineId,
    /// Short code the user can also type.
    pub pairing_code: String,
    /// Random nonce the device must echo.
    pub nonce: String,
    /// Where the device should send the pairing request (relay or tunnel URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The machine's public key, so the device can pin it.
    pub machine_public_key: String,
    /// Expiry, as an RFC 3339 timestamp.
    pub expires_at: chrono::DateTime<Utc>,
}

impl PairingOffer {
    /// The JSON a QR code carries.
    ///
    /// # Errors
    /// Never in practice; the struct is plain data.
    pub fn to_qr_payload(&self) -> Result<String> {
        serde_json::to_string(self).map_err(GatewayError::internal)
    }
}

/// What a device sends to finish pairing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingRequest {
    /// Code from the QR payload.
    pub pairing_code: String,
    /// Nonce from the QR payload.
    pub nonce: String,
    /// Device display name.
    pub device_name: String,
    /// `ios`, `android`, `web`, …
    pub platform: String,
    /// The device's base64 Ed25519 public key.
    pub public_key: String,
}

/// Pairing, device management and ticket issuance.
#[derive(Debug)]
pub struct AuthService {
    identity: MachineIdentity,
    devices: Arc<dyn DeviceRepository>,
    pairing: Arc<dyn PairingRepository>,
    tickets: Arc<dyn TicketRepository>,
    clock: Arc<dyn Clock>,
    config: AuthConfig,
}

impl AuthService {
    /// Assemble the service from its ports.
    #[must_use]
    pub fn new(
        identity: MachineIdentity,
        devices: Arc<dyn DeviceRepository>,
        pairing: Arc<dyn PairingRepository>,
        tickets: Arc<dyn TicketRepository>,
        clock: Arc<dyn Clock>,
        config: AuthConfig,
    ) -> Self {
        Self {
            identity,
            devices,
            pairing,
            tickets,
            clock,
            config,
        }
    }

    /// This machine's identity.
    #[must_use]
    pub fn identity(&self) -> &MachineIdentity {
        &self.identity
    }

    /// Mint a pairing code and the payload to show as a QR code.
    ///
    /// # Errors
    /// Fails if the code cannot be persisted.
    pub async fn begin_pairing(&self, endpoint: Option<String>) -> Result<PairingOffer> {
        let now = self.clock.now();
        let code = PairingCode {
            code: numeric_code(6),
            nonce: random_token(24),
            expires_at: now + chrono_duration(self.config.pairing_ttl),
            consumed_at: None,
        };
        self.pairing.insert(&code).await?;
        info!(expires_at = %code.expires_at, "pairing code issued");
        Ok(PairingOffer {
            machine_id: self.identity.machine_id().clone(),
            pairing_code: code.code,
            nonce: code.nonce,
            endpoint,
            machine_public_key: self.identity.public_key_base64(),
            expires_at: code.expires_at,
        })
    }

    /// Redeem a pairing code and register the device.
    ///
    /// # Errors
    /// [`GatewayError::Expired`] if the code is unknown, expired or already
    /// used; [`GatewayError::AuthenticationFailed`] if the nonce does not match
    /// or the public key is malformed.
    pub async fn complete_pairing(&self, request: PairingRequest) -> Result<Device> {
        let now = self.clock.now();
        let Some(code) = self.pairing.consume(&request.pairing_code, now).await? else {
            warn!("pairing attempt with an unknown, expired or used code");
            return Err(GatewayError::Expired(
                "pairing code is unknown, expired or already used".into(),
            ));
        };

        // The code alone is six digits; the nonce is what makes a shoulder-surfed
        // code useless. Compared in constant time so the check cannot be turned
        // into an oracle.
        if code.nonce.as_bytes().ct_eq(request.nonce.as_bytes()).into() {
            // fall through
        } else {
            warn!("pairing attempt with a mismatched nonce");
            return Err(GatewayError::AuthenticationFailed(
                "pairing nonce does not match".into(),
            ));
        }

        validate_public_key(&request.public_key)?;

        let device = Device {
            id: DeviceId::generate(),
            name: request.device_name,
            public_key: request.public_key,
            platform: request.platform,
            revoked: false,
            created_at: now,
            updated_at: now,
            last_seen_at: Some(now),
        };
        self.devices.upsert(&device).await?;
        info!(device_id = %device.id, platform = %device.platform, "device paired");
        Ok(device)
    }

    /// Issue a single-use ticket for an active device.
    ///
    /// # Errors
    /// [`GatewayError::DeviceNotFound`] for unknown devices,
    /// [`GatewayError::PermissionDenied`] for revoked ones.
    pub async fn issue_ticket(&self, device_id: &DeviceId) -> Result<WsTicket> {
        let device = self
            .devices
            .get(device_id)
            .await?
            .ok_or_else(|| GatewayError::DeviceNotFound(device_id.clone()))?;
        if !device.is_active() {
            return Err(GatewayError::PermissionDenied("device is revoked".into()));
        }
        let now = self.clock.now();
        let ticket = WsTicket {
            id: format!("tkt_{}", random_token(24)),
            device_id: device.id.clone(),
            machine_id: self.identity.machine_id().clone(),
            nonce: random_token(16),
            expires_at: now + chrono_duration(self.config.ticket_ttl),
            used_at: None,
        };
        self.tickets.insert(&ticket).await?;
        info!(device_id = %device.id, "ws ticket issued");
        Ok(ticket)
    }

    /// Issue a ticket to a device that proves possession of its private key.
    ///
    /// This is the endpoint a phone reaches through the tunnel, so it cannot
    /// rely on network position for authentication. The device signs
    /// [`Self::ticket_challenge`], which binds the request to one machine, one
    /// device and one moment: a captured signature is useless once the
    /// [`TICKET_SIGNATURE_WINDOW`] passes.
    ///
    /// # Errors
    /// [`GatewayError::AuthenticationFailed`] if the signature is invalid or
    /// stale; otherwise as [`Self::issue_ticket`].
    pub async fn issue_ticket_for_signed_request(
        &self,
        device_id: &DeviceId,
        issued_at: chrono::DateTime<Utc>,
        signature_base64: &str,
    ) -> Result<WsTicket> {
        let now = self.clock.now();
        let skew = (now - issued_at).num_seconds().abs();
        if skew > TICKET_SIGNATURE_WINDOW.as_secs() as i64 {
            return Err(GatewayError::AuthenticationFailed(
                "signed ticket request is too old or too far in the future".into(),
            ));
        }
        let device = self
            .devices
            .get(device_id)
            .await?
            .ok_or_else(|| GatewayError::DeviceNotFound(device_id.clone()))?;
        let challenge = self.ticket_challenge(device_id, issued_at);
        verify_signature(&device.public_key, challenge.as_bytes(), signature_base64)?;
        self.issue_ticket(device_id).await
    }

    /// The exact bytes a device must sign to request a ticket.
    #[must_use]
    pub fn ticket_challenge(
        &self,
        device_id: &DeviceId,
        issued_at: chrono::DateTime<Utc>,
    ) -> String {
        format!(
            "ws-ticket:{}:{}:{}",
            self.identity.machine_id(),
            device_id,
            issued_at.timestamp_millis()
        )
    }

    /// Redeem a ticket, returning the device it belongs to.
    ///
    /// Enforces every ticket rule in one place: single use and unexpired (both
    /// atomically, in the repository), bound to this machine, and belonging to
    /// a device that still exists and is not revoked.
    ///
    /// # Errors
    /// [`GatewayError::Expired`] for unknown/used/expired tickets,
    /// [`GatewayError::AuthenticationFailed`] for a machine mismatch,
    /// [`GatewayError::PermissionDenied`] for a revoked device.
    pub async fn redeem_ticket(&self, ticket_id: &str) -> Result<Device> {
        let now = self.clock.now();
        let Some(ticket) = self.tickets.consume(ticket_id, now).await? else {
            warn!("ws ticket rejected: unknown, expired or already used");
            return Err(GatewayError::Expired(
                "ticket is unknown, expired or already used".into(),
            ));
        };
        if &ticket.machine_id != self.identity.machine_id() {
            warn!("ws ticket rejected: issued for another machine");
            return Err(GatewayError::AuthenticationFailed(
                "ticket was issued for another machine".into(),
            ));
        }
        let device = self
            .devices
            .get(&ticket.device_id)
            .await?
            .ok_or_else(|| GatewayError::DeviceNotFound(ticket.device_id.clone()))?;
        if !device.is_active() {
            warn!(device_id = %device.id, "ws ticket rejected: device revoked");
            return Err(GatewayError::PermissionDenied("device is revoked".into()));
        }
        self.devices.touch(&device.id, now).await?;
        Ok(device)
    }

    /// Every known device, newest first.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_devices(&self) -> Result<Vec<Device>> {
        self.devices.list().await
    }

    /// Revoke a device. Idempotent for already-revoked devices.
    ///
    /// # Errors
    /// [`GatewayError::DeviceNotFound`] if the device does not exist.
    pub async fn revoke_device(&self, device_id: &DeviceId) -> Result<()> {
        let now = self.clock.now();
        if self.devices.set_revoked(device_id, true, now).await? {
            info!(device_id = %device_id, "device revoked");
            Ok(())
        } else {
            Err(GatewayError::DeviceNotFound(device_id.clone()))
        }
    }

    /// Delete expired pairing codes and tickets. Returns how many rows went.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn purge_expired(&self) -> Result<u64> {
        let now = self.clock.now();
        Ok(self.pairing.purge_expired(now).await? + self.tickets.purge_expired(now).await?)
    }
}

fn chrono_duration(value: Duration) -> chrono::Duration {
    chrono::Duration::from_std(value).unwrap_or_else(|_| chrono::Duration::seconds(60))
}

fn verify_signature(public_key: &str, message: &[u8], signature_base64: &str) -> Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let engine = base64::engine::general_purpose::STANDARD;
    let key_bytes: [u8; 32] = engine
        .decode(public_key)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| GatewayError::AuthenticationFailed("stored public key is invalid".into()))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| GatewayError::AuthenticationFailed("stored public key is invalid".into()))?;
    let signature = engine
        .decode(signature_base64)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or_else(|| GatewayError::AuthenticationFailed("signature is malformed".into()))?;
    key.verify(message, &signature)
        .map_err(|_| GatewayError::AuthenticationFailed("signature does not verify".into()))
}

fn validate_public_key(encoded: &str) -> Result<()> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| GatewayError::AuthenticationFailed("public key is not base64".into()))?;
    if bytes.len() != 32 {
        return Err(GatewayError::AuthenticationFailed(
            "public key must be a 32-byte Ed25519 key".into(),
        ));
    }
    Ok(())
}
