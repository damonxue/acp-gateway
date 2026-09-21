//! The agent-side port.
//!
//! Agent 侧的端口。
//!
//! Everything the gateway needs from "a running coding agent" is expressed by
//! two traits. `gateway-acp` implements them on top of an ACP subprocess;
//! `gateway-remote` implements [`AgentSessionHandle`] on top of the control
//! WebSocket of an IDE bridge. The [`crate::SessionManager`] cannot tell the
//! difference, which is exactly why a phone can drive a session that Zed
//! started.
//!
//! Gateway 对“一个正在运行的 Coding Agent”的全部需求，都用两个 trait 表达。`gateway-acp`
//! 在 ACP 子进程之上实现它们；`gateway-remote` 则可以在 IDE Bridge 的控制 WebSocket 之上
//! 实现 [`AgentSessionHandle`]。[`crate::SessionManager`] 分辨不出两者区别——这正是手机
//! 能够驱动一个由 Zed 启动的 Session 的原因。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::event::EventDraft;
use crate::ids::{AgentId, PermissionId, SessionId};
use crate::permission::PermissionDecision;

/// A configured, launchable agent.
///
/// 一个已配置、可启动的 Agent。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescriptor {
    /// Stable id used in the API (`codex`). / API 中使用的稳定 id（如 `codex`）。
    pub id: AgentId,
    /// Display name (`Codex`). / 展示名称（如 `Codex`）。
    pub name: String,
    /// Executable to run. / 要执行的可执行文件。
    pub command: String,
    /// Arguments that put the executable into ACP mode. / 使其进入 ACP 模式的参数。
    pub args: Vec<String>,
    /// Extra environment variables. Never logged. / 额外的环境变量，永不进日志。
    pub env: BTreeMap<String, String>,
}

/// One piece of a prompt.
///
/// The gateway keeps this deliberately small: the remote clients it serves
/// (phones, browsers) send text and file references. Anything richer is
/// forwarded verbatim by the IDE bridge and never has to round-trip through
/// this type.
///
/// prompt 的一个组成块。
///
/// Gateway 故意把它保持很小：它服务的远程客户端（手机、浏览器）只会发送文本和文件引用。
/// 更丰富的内容由 IDE Bridge 原样转发，无需经过本类型往返转换。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptBlock {
    /// Plain text. / 纯文本。
    Text {
        /// The text. / 文本内容。
        text: String,
    },
    /// A reference to a file in the workspace. / 对工作区内某个文件的引用。
    ResourceLink {
        /// `file:///…` URI.
        uri: String,
        /// Display name. / 展示名称。
        name: String,
    },
}

impl PromptBlock {
    /// Shorthand for a text block.
    ///
    /// 构造文本块的简写。
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Concatenate the textual content of a prompt for logging/preview.
    ///
    /// 拼接 prompt 的文本内容，用于日志或预览。
    #[must_use]
    pub fn preview(blocks: &[Self]) -> String {
        blocks
            .iter()
            .map(|block| match block {
                Self::Text { text } => text.as_str(),
                Self::ResourceLink { name, .. } => name.as_str(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Where mapped agent activity is delivered.
///
/// Implemented by [`crate::SessionManager`]. Runtimes depend on this narrow
/// trait instead of the manager itself, which keeps the dependency acyclic and
/// makes runtimes trivially testable with a recording sink.
///
/// 映射后的 Agent 行为送往何处。
///
/// 由 [`crate::SessionManager`] 实现。运行时依赖这个很窄的 trait，而不是直接依赖
/// manager，既避开了循环依赖，也让运行时可以用一个记录用 sink 轻松测试。
#[async_trait]
pub trait EventSink: Send + Sync + std::fmt::Debug {
    /// Sequence, persist and broadcast one event.
    ///
    /// 为一个事件分配序号、持久化并广播。
    async fn emit(&self, session_id: &SessionId, draft: EventDraft) -> Result<()>;
}

/// Everything needed to bring up an agent for a session.
///
/// 为一个 Session 启动 Agent 所需的全部信息。
#[derive(Debug)]
pub struct LaunchRequest {
    /// Gateway session the agent will serve. / Agent 将服务的 Gateway Session。
    pub session_id: SessionId,
    /// Agent to launch. / 要启动的 Agent。
    pub descriptor: AgentDescriptor,
    /// Working directory passed to ACP `session/new`. / 传给 ACP `session/new` 的工作目录。
    pub cwd: PathBuf,
    /// Directories the agent may access beyond `cwd`. / 除 `cwd` 外 Agent 可访问的目录。
    pub additional_directories: Vec<PathBuf>,
    /// Where to deliver mapped events. / 映射后的事件送往哪里。
    pub sink: Arc<dyn EventSink>,
}

/// The result of a successful launch.
///
/// 启动成功后的结果。
#[derive(Debug)]
pub struct LaunchedAgent {
    /// Session id assigned by the agent over ACP. / Agent 通过 ACP 分配的 session id。
    pub acp_session_id: String,
    /// Handle used to drive the session for the rest of its life.
    /// 在 Session 剩余生命周期内用于驱动它的句柄。
    pub handle: Arc<dyn AgentSessionHandle>,
}

/// Session metadata returned by an agent's ACP `session/list` request.
///
/// The gateway keeps its own session identity and lifecycle state, so this
/// type contains only the ACP-owned fields that can be refreshed from the
/// live agent connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpSessionInfo {
    /// Identifier assigned by the ACP agent.
    pub acp_session_id: String,
    /// Working directory reported by the agent.
    pub cwd: PathBuf,
    /// Human-readable title, when the agent has one.
    #[serde(default)]
    pub title: Option<String>,
    /// Agent-provided last-activity timestamp, when available.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Starts agents. The *strategy* seam of the gateway.
///
/// 负责启动 Agent——Gateway 的*策略*接缝。
#[async_trait]
pub trait AgentRuntime: Send + Sync + std::fmt::Debug {
    /// Start an agent and open an ACP session against it.
    ///
    /// 启动一个 Agent，并对它建立一个 ACP Session。
    async fn launch(&self, request: LaunchRequest) -> Result<LaunchedAgent>;
}

/// Drives one live agent session.
///
/// Implementations must be cheap to clone-by-`Arc` and safe to call from any
/// task. Every method is fire-and-accept: they return as soon as the command
/// has been handed to the agent, and progress is reported through events.
///
/// 驱动一个活的 Agent Session。
///
/// 实现必须可以通过 `Arc` 廉价克隆，并且可以从任意 task 安全调用。所有方法都是
/// “发出即返回”：命令交给 Agent 后立即返回，后续进展通过事件上报。
#[async_trait]
pub trait AgentSessionHandle: Send + Sync + std::fmt::Debug {
    /// Submit a prompt turn.
    ///
    /// Returns once the agent has accepted the request. Completion arrives
    /// later as a `session_completed` / `session_failed` event.
    ///
    /// 提交一个 prompt 回合。Agent 接受请求后即返回；完成情况稍后以
    /// `session_completed` / `session_failed` 事件形式到达。
    async fn submit_prompt(&self, blocks: Vec<PromptBlock>) -> Result<()>;

    /// Ask the agent to stop the current turn.
    ///
    /// 请求 Agent 停止当前回合。
    async fn cancel(&self) -> Result<()>;

    /// Answer an outstanding permission request.
    ///
    /// 回答一个待处理的权限请求。
    async fn resolve_permission(
        &self,
        permission_id: &PermissionId,
        decision: PermissionDecision,
    ) -> Result<()>;

    /// Tear the agent connection down.
    ///
    /// 拆掉 Agent 连接。
    async fn shutdown(&self) -> Result<()>;

    /// Ask the live ACP connection for its current session list.
    ///
    /// Agents that do not advertise `session/list` may keep the default
    /// unsupported result; a refresh then leaves the persisted projection
    /// intact while still returning the other sessions.
    async fn refresh_sessions(&self) -> Result<Vec<AcpSessionInfo>> {
        Err(crate::error::GatewayError::AgentUnavailable(
            "agent does not support ACP session/list".to_owned(),
        ))
    }

    /// Whether the underlying connection is still usable.
    ///
    /// 底层连接是否仍可用。
    fn is_alive(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_preview_joins_text_and_names() {
        let blocks = vec![
            PromptBlock::text("check"),
            PromptBlock::ResourceLink {
                uri: "file:///a/b.rs".into(),
                name: "b.rs".into(),
            },
        ];
        assert_eq!(PromptBlock::preview(&blocks), "check b.rs");
    }
}
