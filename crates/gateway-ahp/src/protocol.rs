//! JSON frames exchanged with the AHP channel service.
//!
//! These frames model the behavior shared by the WeChat AHP host: a hello
//! handshake, QR login state, explicit session binding, inbound user messages,
//! and outbound user/agent messages. Keeping the schema here makes the service
//! specific compatibility work explicit and testable.

use gateway_core::ids::SessionId;
use serde::{Deserialize, Serialize};

/// Protocol version implemented by this adapter.
pub const AHP_VERSION: u32 = 1;

/// Frames sent by the local gateway to the AHP service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Start or resume a channel login.
    Hello {
        version: u32,
        machine_id: String,
    },
    /// Bind the channel to one already running local session.
    Bind {
        session_id: SessionId,
    },
    /// Stop forwarding content for the current session.
    Unbind,
    /// A message entered in the local VS Code/Agent Host chat.
    UserMessage {
        text: String,
    },
    /// The final text answer from the Agent.
    AgentMessage {
        text: String,
    },
    /// A permission request or an operational error which the channel can show.
    Notice {
        text: String,
    },
    Pong,
}

/// Frames received by the local gateway from the AHP service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The service accepted the hello and may return a QR login payload.
    Hello {
        version: u32,
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        qr_code: Option<String>,
    },
    /// A WeChat private message received by the bound channel.
    UserMessage {
        text: String,
    },
    /// The service confirmed the requested binding.
    Bound {
        session_id: SessionId,
    },
    Unbound,
    Error {
        message: String,
    },
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_stable_type_names() {
        let frame = ClientFrame::Bind {
            session_id: SessionId::new("sess_1"),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["type"], "bind");
        assert_eq!(serde_json::from_value::<ClientFrame>(value).unwrap(), frame);
    }

    #[test]
    fn hello_can_carry_qr_state() {
        let frame = ServerFrame::Hello {
            version: AHP_VERSION,
            channel_id: Some("channel_1".into()),
            qr_code: Some("https://wechat.example/qr/1".into()),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("qr_code"));
        assert_eq!(serde_json::from_str::<ServerFrame>(&json).unwrap(), frame);
    }
}
