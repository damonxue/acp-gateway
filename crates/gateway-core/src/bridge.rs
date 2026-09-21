//! The wire form of [`crate::AgentSessionHandle`] and [`crate::EventSink`].
//!
//! When an IDE spawns `agent-gateway acp-bridge`, the bridge is a *separate
//! process* from the daemon. The two halves of the agent port therefore have to
//! cross a socket:
//!
//! ```text
//!   Zed ──ACP──► acp-bridge ──ACP──► codex
//!                    │
//!                    │  BridgeMessage  (events, i.e. EventSink)
//!                    │  DaemonMessage  (commands, i.e. AgentSessionHandle)
//!                    ▼
//!               agent-gateway run
//! ```
//!
//! These types live in the domain crate rather than in a transport crate
//! because they are exactly the two domain ports written down as JSON: the
//! bridge implements the sink side, the daemon implements the handle side, and
//! neither needs to know anything else about the other.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::PromptBlock;
use crate::event::EventType;
use crate::ids::{AgentId, PermissionId, SessionId};
use crate::permission::PermissionDecision;

/// Path of the daemon's bridge WebSocket. Loopback only.
pub const BRIDGE_PATH: &str = "/bridge";

/// Messages the bridge sends to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BridgeMessage {
    /// Announce a session the IDE has already created with the agent.
    ///
    /// Sent once, immediately after the bridge observes the agent's answer to
    /// `session/new`. The daemon replies with [`DaemonMessage::Adopted`].
    Adopt {
        /// Configured agent id the bridge was started with.
        agent_id: AgentId,
        /// Display name for UIs.
        agent_name: String,
        /// Session id the agent assigned.
        acp_session_id: String,
        /// Project root, as the IDE sees it.
        workspace: PathBuf,
        /// Working directory the IDE passed to `session/new`.
        cwd: PathBuf,
    },
    /// Mirror one observed event into the session's log.
    Event {
        /// Event type.
        event_type: EventType,
        /// Event payload, already in the shape the remote protocol documents.
        payload: serde_json::Value,
    },
    /// The IDE or the agent went away.
    Detach {
        /// Why, for the session's history.
        reason: String,
    },
    /// Response to a daemon-triggered ACP `session/list` refresh.
    SessionList {
        /// Correlates this response with [`DaemonMessage::RefreshSessions`].
        request_id: String,
        /// Sessions reported by the proxied ACP agent.
        sessions: Vec<crate::agent::AcpSessionInfo>,
        /// Safe error text when the agent does not support session listing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Messages the daemon sends to the bridge.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DaemonMessage {
    /// The session is now visible to remote clients under this id.
    Adopted {
        /// Gateway session id.
        session_id: SessionId,
    },
    /// A remote client submitted a prompt.
    Prompt {
        /// The prompt.
        blocks: Vec<PromptBlock>,
    },
    /// A remote client cancelled the running turn.
    Cancel,
    /// A remote client answered a permission request.
    Permission {
        /// Request being answered.
        permission_id: PermissionId,
        /// The decision.
        decision: PermissionDecision,
    },
    /// The daemon refused something; the bridge keeps proxying regardless.
    ///
    /// A bridge must never take the IDE's session down because the gateway is
    /// unhappy: the developer's editor keeps working even if remote access does
    /// not.
    Error {
        /// Human-readable reason.
        message: String,
    },
    /// Ask the ACP bridge to query its real agent with `session/list`.
    RefreshSessions {
        /// Correlation id returned in [`BridgeMessage::SessionList`].
        request_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let adopt = BridgeMessage::Adopt {
            agent_id: AgentId::new("codex"),
            agent_name: "Codex".to_owned(),
            acp_session_id: "acp-1".to_owned(),
            workspace: PathBuf::from("/work"),
            cwd: PathBuf::from("/work"),
        };
        let json = serde_json::to_string(&adopt).unwrap();
        assert!(json.contains("\"type\":\"adopt\""));
        assert!(matches!(
            serde_json::from_str::<BridgeMessage>(&json).unwrap(),
            BridgeMessage::Adopt { .. }
        ));

        let prompt = DaemonMessage::Prompt {
            blocks: vec![PromptBlock::text("go")],
        };
        let json = serde_json::to_string(&prompt).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["type"],
            "prompt"
        );
    }

    #[test]
    fn an_event_keeps_the_documented_type_names() {
        let event = BridgeMessage::Event {
            event_type: EventType::AgentMessageChunk,
            payload: serde_json::json!({ "content": { "text": "hi" } }),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event_type"], "agent_message_chunk");
    }
}
