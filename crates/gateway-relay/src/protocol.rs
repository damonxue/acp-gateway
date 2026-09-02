//! The relay control-plane contract.
//!
//! The relay is a *connection* layer, not a data layer. That boundary is
//! enforced by these types: there is no field anywhere in this module that can
//! carry a prompt, a diff, agent output, terminal output or source code. A
//! push notification says "Claude is waiting for your permission", never what
//! it wants permission for.
//!
//! Requests from a gateway are signed with its Ed25519 machine key, so a relay
//! never has to hold a credential that could impersonate a machine — it stores
//! public keys only.

use chrono::{DateTime, Utc};
use gateway_core::ids::{MachineId, SessionId};
use serde::{Deserialize, Serialize};

/// Registration of a machine with the relay.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegisterRequest {
    /// Machine being registered.
    pub machine_id: MachineId,
    /// Base64 Ed25519 public key. The relay stores this and verifies every
    /// later request against it.
    pub public_key: String,
    /// Display name.
    pub name: String,
    /// `macos`, `linux`, …
    pub platform: String,
    /// Gateway version.
    pub version: String,
    /// Public HTTPS endpoint of the gateway's tunnel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// When the request was signed.
    pub issued_at: DateTime<Utc>,
    /// Signature over [`challenge`].
    pub signature: String,
}

/// A heartbeat, which also refreshes the endpoint after a tunnel restart.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HeartbeatRequest {
    /// Current public endpoint, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// When the request was signed.
    pub issued_at: DateTime<Utc>,
    /// Signature over [`challenge`].
    pub signature: String,
}

/// Why a device should be woken up.
///
/// Deliberately a closed set of *states*: adding a variant that carried text
/// would be the moment the relay started holding user content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    /// An agent is blocked on a permission decision.
    PermissionRequired,
    /// A prompt turn finished.
    TurnCompleted,
    /// A session failed.
    SessionFailed,
    /// The agent connection was lost.
    SessionDisconnected,
}

impl PushKind {
    /// The notification text a device shows. Contains no agent content.
    #[must_use]
    pub fn message(self, agent_name: &str) -> String {
        match self {
            Self::PermissionRequired => format!("{agent_name} is waiting for your permission."),
            Self::TurnCompleted => format!("{agent_name} finished."),
            Self::SessionFailed => format!("{agent_name} failed."),
            Self::SessionDisconnected => format!("{agent_name} disconnected."),
        }
    }
}

/// One push-worthy state change.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PushEvent {
    /// Session that changed.
    pub session_id: SessionId,
    /// Agent display name, so the phone can say "Claude" and not "sess_9f…".
    pub agent_name: String,
    /// What happened.
    pub kind: PushKind,
    /// When it happened.
    pub occurred_at: DateTime<Utc>,
}

/// A batch of push events.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PushRequest {
    /// The events.
    pub events: Vec<PushEvent>,
    /// When the request was signed.
    pub issued_at: DateTime<Utc>,
    /// Signature over [`challenge`].
    pub signature: String,
}

/// A machine as the relay describes it to mobile clients.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MachineSummary {
    /// Machine id.
    pub machine_id: MachineId,
    /// Display name.
    pub name: String,
    /// Platform.
    pub platform: String,
    /// Public endpoint of the gateway, when it has a tunnel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Whether a heartbeat arrived recently.
    pub online: bool,
    /// Last heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// The exact bytes a gateway signs for a relay request.
///
/// Including the action and the timestamp means a captured signature cannot be
/// replayed against a different endpoint or an hour later.
#[must_use]
pub fn challenge(action: &str, machine_id: &MachineId, issued_at: DateTime<Utc>) -> String {
    format!(
        "relay:{action}:{machine_id}:{}",
        issued_at.timestamp_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_messages_never_contain_agent_output() {
        let message = PushKind::PermissionRequired.message("Claude Code");
        assert_eq!(message, "Claude Code is waiting for your permission.");
    }

    #[test]
    fn the_challenge_binds_action_machine_and_time() {
        let at = DateTime::from_timestamp_millis(1_780_000_000_000).unwrap();
        assert_eq!(
            challenge("register", &MachineId::new("machine_1"), at),
            "relay:register:machine_1:1780000000000"
        );
    }
}
