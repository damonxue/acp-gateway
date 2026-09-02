//! Permission requests and decisions.
//!
//! 权限请求与决定。
//!
//! ACP models permission as an agent → client *request* that blocks until the
//! client answers. The gateway sits in the middle: it turns that request into
//! an event any number of remote clients can see, and turns the first decision
//! it receives back into an ACP response.
//!
//! ACP 将权限建模为 Agent → Client 的*请求*，在客户端回答前一直阻塞。Gateway 站在中间：
//! 它把这个请求变成任意多个远程客户端都能看到的事件，并把收到的第一个决定变回
//! 一个 ACP 响应。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::PermissionId;

/// What kind of answer an option represents.
///
/// Mirrors ACP's `PermissionOptionKind` so a mobile client can render the
/// allow/reject affordances the agent actually offered instead of guessing.
///
/// 一个选项代表何种回答。
///
/// 与 ACP 的 `PermissionOptionKind` 一一对应，因此手机端可以渲染 Agent 真正提供的
/// 允许 / 拒绝选项，而不是靠猜。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow this one time. / 仅本次允许。
    AllowOnce,
    /// Allow and remember. / 允许并记住选择。
    AllowAlways,
    /// Reject this one time. / 仅本次拒绝。
    RejectOnce,
    /// Reject and remember. / 拒绝并记住选择。
    RejectAlways,
}

impl PermissionOptionKind {
    /// Whether picking this option lets the operation proceed.
    ///
    /// 选择该选项是否会让操作继续执行。
    #[must_use]
    pub fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// One choice offered by the agent.
///
/// Agent 提供的一个选项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// Agent-defined option id, echoed back verbatim in the ACP response.
    /// Agent 定义的选项 id，在 ACP 响应中原样回传。
    pub option_id: String,
    /// Label to show the user. / 展示给用户的文案。
    pub name: String,
    /// Semantic kind. / 语义类型。
    pub kind: PermissionOptionKind,
}

/// A permission request awaiting an answer.
///
/// 一个等待回答的权限请求。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Gateway-assigned id used by remote clients to answer.
    /// Gateway 分配的 id，远程客户端用它来回答。
    pub id: PermissionId,
    /// Title of the tool call needing approval, e.g. `Run cargo test`.
    /// 需要授权的 Tool Call 标题，例如 `Run cargo test`。
    pub title: Option<String>,
    /// Tool kind reported by the agent (`execute`, `edit`, …).
    /// Agent 上报的工具类型（`execute`、`edit` 等）。
    pub tool_kind: Option<String>,
    /// The raw ACP `toolCall` payload, so clients can render the exact command
    /// or diff instead of a gateway-flattened summary.
    ///
    /// 原始的 ACP `toolCall` payload，使客户端能渲染准确的命令或 diff，
    /// 而不是 Gateway 压平后的摘要。
    pub tool_call: serde_json::Value,
    /// Options the agent offered. / Agent 提供的选项。
    pub options: Vec<PermissionOption>,
    /// When the request arrived. / 请求到达的时间。
    pub requested_at: DateTime<Utc>,
}

impl PermissionRequest {
    /// Pick the option id that best matches a coarse allow/deny decision.
    ///
    /// Remote clients may answer with a specific `option_id` (preferred) or
    /// with a plain boolean, which every mobile permission sheet ultimately
    /// produces. This resolves the boolean against what the agent offered,
    /// preferring the *once* variants so a phone tap never silently grants a
    /// standing permission.
    ///
    /// 为粗粒度的允许 / 拒绝决定选出最匹配的选项 id。
    ///
    /// 远程客户端可以用具体的 `option_id` 回答（推荐），也可以用一个布尔值——手机上的
    /// 权限弹窗最终产生的就是布尔值。本方法把布尔值对齐到 Agent 实际提供的选项，
    /// 并优先选择*仅本次*的变体，避免手机上一个点击默默授出长期权限。
    #[must_use]
    pub fn resolve_option(&self, approved: bool) -> Option<&PermissionOption> {
        let wanted_once = if approved {
            PermissionOptionKind::AllowOnce
        } else {
            PermissionOptionKind::RejectOnce
        };
        self.options
            .iter()
            .find(|option| option.kind == wanted_once)
            .or_else(|| {
                self.options
                    .iter()
                    .find(|option| option.kind.is_allow() == approved)
            })
    }
}

/// A decision produced by a client (or by the gateway on cancellation).
///
/// 由客户端（或取消时由 Gateway）产生的决定。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum PermissionDecision {
    /// The user picked a concrete option. / 用户选择了一个具体选项。
    Selected {
        /// Agent-defined option id. / Agent 定义的选项 id。
        option_id: String,
    },
    /// The user answered with a plain allow/deny; the gateway maps it onto the
    /// agent's option list.
    ///
    /// 用户只给了允许 / 拒绝；Gateway 负责把它映射到 Agent 的选项列表。
    Approved {
        /// `true` = allow, `false` = deny. / `true` 为允许，`false` 为拒绝。
        approved: bool,
    },
    /// The request is void — the turn was cancelled or the session went away.
    /// 请求已作废——回合被取消，或 Session 已消失。
    Cancelled,
}

impl PermissionDecision {
    /// Convenience constructor for the boolean form.
    ///
    /// 布尔形式的便捷构造函数。
    #[must_use]
    pub fn approved(approved: bool) -> Self {
        Self::Approved { approved }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(options: Vec<PermissionOption>) -> PermissionRequest {
        PermissionRequest {
            id: PermissionId::new("perm_1"),
            title: None,
            tool_kind: None,
            tool_call: serde_json::json!({}),
            options,
            requested_at: Utc::now(),
        }
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            option_id: id.to_owned(),
            name: id.to_owned(),
            kind,
        }
    }

    #[test]
    fn boolean_approval_prefers_the_once_variant() {
        let req = request(vec![
            option("always", PermissionOptionKind::AllowAlways),
            option("once", PermissionOptionKind::AllowOnce),
        ]);
        assert_eq!(req.resolve_option(true).unwrap().option_id, "once");
    }

    #[test]
    fn boolean_approval_falls_back_to_any_matching_kind() {
        let req = request(vec![option("always", PermissionOptionKind::AllowAlways)]);
        assert_eq!(req.resolve_option(true).unwrap().option_id, "always");
        assert!(req.resolve_option(false).is_none());
    }
}
