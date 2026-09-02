//! The event model.
//!
//! 事件模型。
//!
//! Every observable thing an agent does becomes an [`AgentEvent`] with a
//! session-scoped, gap-free `seq`. That single decision buys streaming,
//! reconnect, history, audit and replay from one mechanism instead of four.
//!
//! Agent 每一个可观测的行为都会变成一个 [`AgentEvent`]，带有 Session 内无空洞的 `seq`。
//! 就这一个决定，让实时流式、断线重连、历史记录、审计和回放用一套机制就全部具备，
//! 而不是四套。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{EventId, SessionId};

/// Canonical event type names.
///
/// These strings are part of the remote protocol contract, so they are defined
/// once here and never spelled inline. [`EventType::as_str`] is the only way
/// to obtain one.
///
/// 规范的事件类型名。
///
/// 这些字符串是远程协议契约的一部分，因此只在此处定义一次，绝不在其他地方写字面量。
/// [`EventType::as_str`] 是获取它们的唯一途径。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventType {
    /// A session was created and is ready to accept prompts.
    /// Session 已创建，可以接收 prompt。
    SessionCreated,
    /// The session's [`crate::SessionStatus`] changed.
    /// Session 的 [`crate::SessionStatus`] 发生变化。
    SessionStatus,
    /// A prompt was accepted from a client (echoed so every observer sees it).
    /// 已接收客户端的 prompt（回显出来，让所有观察者都能看到）。
    UserMessage,
    /// A streamed chunk of the user's message (ACP `user_message_chunk`).
    /// 用户消息的流式分片（ACP 的 `user_message_chunk`）。
    UserMessageChunk,
    /// The assembled agent message for one turn.
    /// 一个回合中拼接完成的 Agent 消息。
    AgentMessage,
    /// A streamed chunk of the agent's answer.
    /// Agent 回答的流式分片。
    AgentMessageChunk,
    /// A streamed chunk of the agent's internal reasoning.
    /// Agent 内部推理过程的流式分片。
    AgentThoughtChunk,
    /// A tool call started.
    /// 一个 Tool Call 开始。
    ToolCall,
    /// A tool call changed status or produced content.
    /// Tool Call 状态变化或产生了内容。
    ToolCallUpdate,
    /// Output from a terminal the gateway runs on the agent's behalf.
    /// Gateway 代 Agent 运行的终端输出。
    TerminalOutput,
    /// The agent asked for a permission decision.
    /// Agent 请求一个权限决定。
    PermissionRequest,
    /// A permission decision was recorded.
    /// 权限决定已记录。
    PermissionResponse,
    /// The agent published or refreshed its plan.
    /// Agent 发布或更新了它的计划。
    PlanUpdate,
    /// The set of available slash-commands changed.
    /// 可用斜杠命令集合发生变化。
    AvailableCommandsUpdate,
    /// The agent's session mode changed.
    /// Agent 的 Session 模式发生变化。
    CurrentModeUpdate,
    /// Token/cost usage update.
    /// Token / 费用用量更新。
    UsageUpdate,
    /// A prompt turn finished normally.
    /// 一个 prompt 回合正常结束。
    SessionCompleted,
    /// A prompt turn or the session itself failed.
    /// 一个回合或整个 Session 失败。
    SessionFailed,
    /// An ACP `session/update` the gateway does not model explicitly. The raw
    /// payload is preserved so no information is lost across the proxy.
    ///
    /// Gateway 未显式建模的 ACP `session/update`。原始 payload 会原样保留，
    /// 保证经过代理后不丢信息。
    SessionUpdate,
    /// A non-fatal error worth surfacing to clients.
    /// 值得告知客户端的非致命错误。
    Error,
}

impl EventType {
    /// Wire/database representation.
    ///
    /// 线上 / 数据库表示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session_created",
            Self::SessionStatus => "session_status",
            Self::UserMessage => "user_message",
            Self::UserMessageChunk => "user_message_chunk",
            Self::AgentMessage => "agent_message",
            Self::AgentMessageChunk => "agent_message_chunk",
            Self::AgentThoughtChunk => "agent_thought_chunk",
            Self::ToolCall => "tool_call",
            Self::ToolCallUpdate => "tool_call_update",
            Self::TerminalOutput => "terminal_output",
            Self::PermissionRequest => "permission_request",
            Self::PermissionResponse => "permission_response",
            Self::PlanUpdate => "plan_update",
            Self::AvailableCommandsUpdate => "available_commands_update",
            Self::CurrentModeUpdate => "current_mode_update",
            Self::UsageUpdate => "usage_update",
            Self::SessionCompleted => "session_completed",
            Self::SessionFailed => "session_failed",
            Self::SessionUpdate => "session_update",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for EventType {
    /// `pad`, not `write_str`, so `{:<24}` lines up in tabular output.
    ///
    /// 用 `pad` 而不是 `write_str`，使 `{:<24}` 在表格输出中能对齐。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// An event that has not been sequenced or persisted yet.
///
/// Producers build a draft; only [`crate::SessionManager::append_event`] can
/// turn one into an [`AgentEvent`]. That keeps sequence assignment in a single
/// place and makes "persist before broadcast" impossible to bypass.
///
/// 尚未分配序号、尚未持久化的事件。
///
/// 生产者只构造 draft；只有 [`crate::SessionManager::append_event`] 能把它变成
/// [`AgentEvent`]。这使序号分配集中在一处，也让“先落库后广播”无法被绕过。
#[derive(Clone, Debug, PartialEq)]
pub struct EventDraft {
    /// Event type. / 事件类型。
    pub event_type: EventType,
    /// Type-specific body. Always a JSON object. / 类型相关的主体，总是 JSON 对象。
    pub payload: serde_json::Value,
}

impl EventDraft {
    /// Build a draft from any serialisable payload.
    ///
    /// 用任意可序列化的 payload 构造 draft。
    ///
    /// # Errors
    /// Returns an error if `payload` cannot be serialised to JSON.
    /// 当 `payload` 无法序列化为 JSON 时返回错误。
    pub fn new(event_type: EventType, payload: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            event_type,
            payload: serde_json::to_value(payload)?,
        })
    }

    /// Build a draft from an already-materialised JSON value.
    ///
    /// 用已经构造好的 JSON 值构造 draft。
    #[must_use]
    pub fn from_value(event_type: EventType, payload: serde_json::Value) -> Self {
        Self {
            event_type,
            payload,
        }
    }

    /// Build a draft with an empty object payload.
    ///
    /// 构造 payload 为空对象的 draft。
    #[must_use]
    pub fn empty(event_type: EventType) -> Self {
        Self {
            event_type,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// A persisted, sequenced event.
///
/// 已持久化、已分配序号的事件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    /// Globally unique event id. / 全局唯一的事件 id。
    pub id: EventId,
    /// Session the event belongs to. / 事件所属的 Session。
    pub session_id: SessionId,
    /// Strictly increasing, gap-free within the session, starting at 1.
    /// Session 内严格递增、无空洞，从 1 开始。
    pub seq: u64,
    /// When the gateway sequenced the event. / Gateway 分配序号的时间。
    pub timestamp: DateTime<Utc>,
    /// Event type. / 事件类型。
    #[serde(rename = "type")]
    pub event_type: String,
    /// Type-specific body. / 类型相关的主体。
    pub payload: serde_json::Value,
}

impl AgentEvent {
    /// Whether this event indicates the session reached a terminal state.
    ///
    /// 该事件是否表示 Session 进入了终止状态。
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.event_type == EventType::SessionFailed.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialises_type_under_the_documented_key() {
        let event = AgentEvent {
            id: EventId::new("evt_1"),
            session_id: SessionId::new("sess_1"),
            seq: 42,
            timestamp: DateTime::from_timestamp(1_780_000_000, 0).unwrap(),
            event_type: EventType::AgentMessageChunk.as_str().to_owned(),
            payload: serde_json::json!({ "content": "hi" }),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "agent_message_chunk");
        assert_eq!(json["seq"], 42);
    }
}
