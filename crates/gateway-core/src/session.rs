//! Session aggregate: the central resource of the gateway.
//!
//! Session 聚合根：Gateway 的核心资源。
//!
//! A session belongs to the *gateway*, never to a remote client. That is the
//! rule that makes "close the app, the agent keeps working" true, and it is
//! encoded here: nothing in this module refers to a connection.
//!
//! Session 属于 *Gateway*，永远不属于远程客户端。正是这条规则让“关掉 App，Agent
//! 继续干活”成立；它就编组在这里——本模块里没有任何东西提到“连接”。

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, MachineId, SessionId};

/// Lifecycle state of a session.
///
/// Session 的生命周期状态。
///
/// ```text
///                     ┌──────────────► Failed
///                     │
///  Idle ──prompt──► Running ──permission──► WaitingPermission
///   ▲                 │  ▲                        │
///   └────turn end─────┘  └────────decision────────┘
///                     │
///                  cancel
///                     │
///                 Cancelling ──► Idle
///
///  any ──agent gone──► Disconnected ──► (terminal until resumed)
///  any ──closed─────► Completed
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Connected to an agent, no turn in flight.
    /// 已连接 Agent，当前没有进行中的回合。
    Idle,
    /// A prompt turn is in flight.
    /// 有一个 prompt 回合正在进行。
    Running,
    /// The agent asked for a permission decision and is blocked on it.
    /// Agent 请求了权限决定，并因此阻塞。
    WaitingPermission,
    /// A cancel has been sent; waiting for the agent to wind the turn down.
    /// 已发送取消，等待 Agent 结束当前回合。
    Cancelling,
    /// The session was closed cleanly and will not run again.
    /// Session 已正常关闭，不会再运行。
    Completed,
    /// The session ended because of an error.
    /// Session 因错误而结束。
    Failed,
    /// The agent connection is gone (gateway restart, IDE quit) but history is
    /// retained. A session in this state can be listed and replayed, not driven.
    ///
    /// Agent 连接已丢失（Gateway 重启、IDE 退出），但历史保留。该状态下的 Session
    /// 可以被列出和回放，但不能被驱动。
    Disconnected,
}

impl SessionStatus {
    /// Wire/database representation.
    ///
    /// 线上 / 数据库表示。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingPermission => "waiting_permission",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Disconnected => "disconnected",
        }
    }

    /// Parse the wire/database representation.
    ///
    /// 解析线上 / 数据库表示。
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "running" => Self::Running,
            "waiting_permission" => Self::WaitingPermission,
            "cancelling" => Self::Cancelling,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "disconnected" => Self::Disconnected,
            _ => return None,
        })
    }

    /// A terminal status can never transition again.
    ///
    /// 终止状态不会再发生任何转换。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Disconnected)
    }

    /// Whether the session can still accept prompts / cancels / decisions.
    ///
    /// Session 是否仍能接收 prompt / 取消 / 权限决定。
    #[must_use]
    pub fn is_drivable(self) -> bool {
        !self.is_terminal()
    }
}

/// Who owns the agent connection behind a session.
///
/// Both origins expose the same [`crate::AgentSessionHandle`], so remote
/// clients cannot tell them apart — but operators and the UI can, and restart
/// policy differs.
///
/// Session 背后的 Agent 连接归谁所有。
///
/// 两种来源暴露同一个 [`crate::AgentSessionHandle`]，因此远程客户端分辨不出区别——
/// 但运维人员和 UI 可以，而且重启策略不同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// The gateway daemon spawned the agent process itself.
    /// Gateway 守护进程自己启动了 Agent 进程。
    Gateway,
    /// An IDE spawned `agent-gateway acp-bridge`, which proxies to the agent.
    /// The gateway observes and can drive the session, but does not own the
    /// process: when the IDE quits, the session becomes `Disconnected`.
    ///
    /// IDE 启动了 `agent-gateway acp-bridge`，由它代理到真正的 Agent。Gateway 可以
    /// 观察并驱动这个 Session，但不拥有进程：IDE 退出时 Session 变为 `Disconnected`。
    IdeBridge,
}

impl SessionOrigin {
    /// Wire/database representation.
    ///
    /// 线上 / 数据库表示。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::IdeBridge => "ide_bridge",
        }
    }

    /// Parse the wire/database representation.
    ///
    /// 解析线上 / 数据库表示。
    #[must_use]
    pub fn from_str_opt(value: &str) -> Option<Self> {
        Some(match value {
            "gateway" => Self::Gateway,
            "ide_bridge" => Self::IdeBridge,
            _ => return None,
        })
    }
}

/// Persistent projection of a session.
///
/// This is the shape stored in SQLite and returned by the API. Runtime-only
/// state (channels, the agent handle) lives in
/// [`crate::manager::ManagedSession`] and is deliberately not part of this type.
///
/// Session 的持久化投影。
///
/// 这就是存入 SQLite、并由 API 返回的结构。仅运行时存在的状态（channel、Agent 句柄）
/// 住在 [`crate::manager::ManagedSession`]，故意不属于本类型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Gateway-assigned id. / Gateway 分配的 id。
    pub id: SessionId,
    /// Machine the session runs on. / Session 所在的机器。
    pub machine_id: MachineId,
    /// Configured agent id (`codex`, `claude`, …). / 配置的 Agent id。
    pub agent_id: AgentId,
    /// Human-readable agent name for UIs. / 供 UI 展示的 Agent 名称。
    pub agent_name: String,
    /// Session id assigned by the agent over ACP, when known.
    /// Agent 通过 ACP 分配的 session id（已知时）。
    pub acp_session_id: Option<String>,
    /// Root directory the user opened. / 用户打开的项目根目录。
    pub workspace: PathBuf,
    /// Working directory handed to the agent in `session/new`.
    /// `session/new` 传给 Agent 的工作目录。
    pub cwd: PathBuf,
    /// Optional short title (set from ACP `session_info_update`).
    /// 可选的短标题（来自 ACP 的 `session_info_update`）。
    pub title: Option<String>,
    /// Who owns the agent connection. / Agent 连接的归属。
    pub origin: SessionOrigin,
    /// Current lifecycle state. / 当前生命周期状态。
    pub status: SessionStatus,
    /// Highest sequence number written for this session. / 该 Session 已写入的最大序号。
    pub last_seq: u64,
    /// Creation timestamp. / 创建时间。
    pub created_at: DateTime<Utc>,
    /// Timestamp of the last state or event change. / 最后一次状态或事件变化的时间。
    pub updated_at: DateTime<Utc>,
}

impl AgentSession {
    /// Whether a prompt may be submitted right now.
    ///
    /// 当前是否允许提交 prompt。
    #[must_use]
    pub fn accepts_prompt(&self) -> bool {
        matches!(self.status, SessionStatus::Idle | SessionStatus::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_its_wire_form() {
        for status in [
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::WaitingPermission,
            SessionStatus::Cancelling,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Disconnected,
        ] {
            assert_eq!(SessionStatus::from_str_opt(status.as_str()), Some(status));
        }
    }

    #[test]
    fn terminal_statuses_are_not_drivable() {
        assert!(!SessionStatus::Completed.is_drivable());
        assert!(!SessionStatus::Disconnected.is_drivable());
        assert!(SessionStatus::WaitingPermission.is_drivable());
    }
}
