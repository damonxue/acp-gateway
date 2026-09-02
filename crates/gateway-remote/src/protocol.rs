//! The remote WebSocket protocol spoken by phones and browsers.
//!
//! This is **not** ACP. ACP is how the gateway talks to agents; this is how
//! remote clients talk to the gateway. Keeping them separate is what lets the
//! gateway upgrade its ACP SDK without shipping a new mobile app, and what
//! lets the mobile protocol carry gateway concepts (tickets, machines,
//! replay cursors) that ACP has no opinion about.
//!
//! ## Shape
//!
//! Every message is a JSON object with a `type` discriminator.
//!
//! ```text
//! client ──► {"type":"subscribe","session_id":"sess_1","after_seq":41}
//! gateway ─► {"type":"agent_message_chunk","session_id":"sess_1","seq":42,"payload":{…}}
//! ```
//!
//! Pushed events are serialised as the event itself — `type` *is* the event
//! type — which is what the product documentation specifies and what makes a
//! replayed event and a live event literally the same bytes.
//!
//! ## Telling the two apart
//!
//! Because events keep their own `type`, two names appear in both directions:
//! [`EventType::SessionCreated`] and [`EventType::Error`] are also
//! [`ServerMessage::SessionCreated`] and [`ServerMessage::Error`]. The
//! discriminator a client must use is therefore **`seq`**, not `type`:
//!
//! > Every event carries `seq` and `session_id`. No control message carries
//! > `seq`.
//!
//! A client that switches on `type` first will misread a replayed
//! `session_created` *event* as the answer to its `create_session` *command*.
//! `web/src/api/types.ts::classify` implements the rule, and
//! `docs/remote-protocol.md` states it for other implementations.
//!
//! [`EventType::SessionCreated`]: gateway_core::EventType::SessionCreated
//! [`EventType::Error`]: gateway_core::EventType::Error

use gateway_core::agent::PromptBlock;
use gateway_core::event::AgentEvent;
use gateway_core::ids::{PermissionId, SessionId};
use gateway_core::machine::Machine;
use gateway_core::manager::SessionSnapshot;
use gateway_core::permission::PermissionDecision;
use gateway_core::session::AgentSession;
use serde::{Deserialize, Serialize};

/// Version of this protocol, announced in [`ServerMessage::Hello`].
pub const PROTOCOL_VERSION: u32 = 1;

/// A prompt as remote clients send it.
///
/// Phones send a string; richer clients send blocks. Accepting both here means
/// no handler has to branch on it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PromptInput {
    /// `"prompt": "fix the failing test"`
    Text(String),
    /// `"prompt": [{"type":"text","text":"..."}]`
    Blocks(Vec<PromptBlock>),
}

impl PromptInput {
    /// Convert into domain prompt blocks.
    #[must_use]
    pub fn into_blocks(self) -> Vec<PromptBlock> {
        match self {
            Self::Text(text) => vec![PromptBlock::text(text)],
            Self::Blocks(blocks) => blocks,
        }
    }
}

/// How a client answers a permission request.
///
/// `option_id` is preferred (it names one of the agent's own options);
/// `approved` is the boolean form every mobile permission sheet produces.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionAnswer {
    /// The permission request being answered.
    pub request_id: PermissionId,
    /// Exact option chosen, when the UI showed the agent's options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    /// Coarse allow/deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
}

impl PermissionAnswer {
    /// The decision this answer represents.
    ///
    /// An answer with neither field set is a *denial*: an ambiguous message
    /// must never be able to approve `rm -rf`.
    #[must_use]
    pub fn decision(&self) -> PermissionDecision {
        match (&self.option_id, self.approved) {
            (Some(option_id), _) => PermissionDecision::Selected {
                option_id: option_id.clone(),
            },
            (None, Some(approved)) => PermissionDecision::approved(approved),
            (None, None) => PermissionDecision::approved(false),
        }
    }
}

/// Messages a remote client sends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientMessage {
    /// Start receiving a session's events, replaying everything after
    /// `after_seq` first.
    Subscribe {
        /// Session to watch.
        session_id: SessionId,
        /// Last sequence number the client already has. `None` means "from the
        /// beginning".
        #[serde(default)]
        after_seq: Option<u64>,
    },
    /// Stop receiving a session's events.
    Unsubscribe {
        /// Session to stop watching.
        session_id: SessionId,
    },
    /// Send a prompt turn.
    Prompt {
        /// Target session.
        session_id: SessionId,
        /// The prompt.
        prompt: PromptInput,
    },
    /// Cancel the running turn.
    Cancel {
        /// Target session.
        session_id: SessionId,
    },
    /// Answer a permission request.
    PermissionResponse {
        /// Target session.
        session_id: SessionId,
        /// The answer.
        #[serde(flatten)]
        answer: PermissionAnswer,
    },
    /// Ask for the current session list.
    ListSessions,
    /// Create a session and launch its agent.
    CreateSession {
        /// Configured agent id.
        agent_id: String,
        /// Project root.
        workspace: String,
        /// Working directory; defaults to `workspace`.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Close a session and stop its agent.
    CloseSession {
        /// Target session.
        session_id: SessionId,
    },
    /// Keep-alive.
    Ping,
}

/// A configured agent, as advertised to clients.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentSummary {
    /// Configured id (`codex`).
    pub id: String,
    /// Display name (`Codex`).
    pub name: String,
}

/// Messages the gateway sends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerMessage {
    /// First message on every connection.
    Hello {
        /// Protocol version.
        protocol_version: u32,
        /// The machine the client reached.
        machine: Machine,
        /// Agents this gateway can launch.
        agents: Vec<AgentSummary>,
        /// Current sessions, newest activity first.
        sessions: Vec<AgentSession>,
    },
    /// Answer to [`ClientMessage::ListSessions`].
    SessionList {
        /// Sessions, newest activity first.
        sessions: Vec<AgentSession>,
    },
    /// Answer to [`ClientMessage::CreateSession`].
    SessionCreated {
        /// The new session.
        session: AgentSession,
    },
    /// Replay finished; everything after this is live.
    Subscribed {
        /// Session now being streamed.
        session_id: SessionId,
        /// Highest sequence number delivered during replay.
        last_seq: u64,
        /// Snapshot including permissions still awaiting an answer, so a phone
        /// that reconnects mid-prompt can render the sheet immediately.
        snapshot: SessionSnapshot,
    },
    /// A command failed. The connection stays open.
    Error {
        /// Stable error code.
        code: String,
        /// Human-readable message.
        message: String,
        /// Session the command referred to, when applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// Answer to [`ClientMessage::Ping`].
    Pong,
}

/// Anything the gateway can put on the wire.
///
/// Untagged, because an event already carries its own `type`. See the module
/// docs for how a client tells the two apart.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Outgoing {
    /// A control message.
    Control(ServerMessage),
    /// A session event, live or replayed.
    Event(AgentEvent),
}

impl From<ServerMessage> for Outgoing {
    fn from(message: ServerMessage) -> Self {
        Self::Control(message)
    }
}

impl From<AgentEvent> for Outgoing {
    fn from(event: AgentEvent) -> Self {
        Self::Event(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gateway_core::event::EventType;
    use gateway_core::ids::EventId;

    #[test]
    fn the_documented_client_messages_parse() {
        let subscribe: ClientMessage =
            serde_json::from_str(r#"{"type":"subscribe","session_id":"sess_123","after_seq":41}"#)
                .unwrap();
        assert!(matches!(
            subscribe,
            ClientMessage::Subscribe {
                after_seq: Some(41),
                ..
            }
        ));

        let prompt: ClientMessage = serde_json::from_str(
            r#"{"type":"prompt","session_id":"sess_123","prompt":"keep going"}"#,
        )
        .unwrap();
        match prompt {
            ClientMessage::Prompt { prompt, .. } => {
                assert_eq!(prompt.into_blocks(), vec![PromptBlock::text("keep going")]);
            }
            other => panic!("unexpected {other:?}"),
        }

        let permission: ClientMessage = serde_json::from_str(
            r#"{"type":"permission_response","session_id":"sess_123","request_id":"perm_1","approved":false}"#,
        )
        .unwrap();
        match permission {
            ClientMessage::PermissionResponse { answer, .. } => {
                assert_eq!(answer.decision(), PermissionDecision::approved(false));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_ambiguous_permission_answer_denies() {
        let answer = PermissionAnswer {
            request_id: PermissionId::new("perm_1"),
            option_id: None,
            approved: None,
        };
        assert_eq!(answer.decision(), PermissionDecision::approved(false));
    }

    #[test]
    fn pushed_events_use_the_documented_envelope() {
        let event = AgentEvent {
            id: EventId::new("evt_1"),
            session_id: SessionId::new("sess_123"),
            seq: 43,
            timestamp: Utc::now(),
            event_type: EventType::TerminalOutput.as_str().to_owned(),
            payload: serde_json::json!({ "stream": "stdout", "content": "running 12 tests" }),
        };
        let json = serde_json::to_value(Outgoing::from(event)).unwrap();
        assert_eq!(json["type"], "terminal_output");
        assert_eq!(json["session_id"], "sess_123");
        assert_eq!(json["seq"], 43);
        assert_eq!(json["payload"]["stream"], "stdout");
    }
}
