//! The single error type crossing gateway layer boundaries.
//!
//! 贯穿 Gateway 各层边界的唯一错误类型。

use std::borrow::Cow;

use crate::ids::{DeviceId, PermissionId, SessionId};

/// Convenience alias used throughout the workspace.
///
/// 整个 workspace 通用的便捷别名。
pub type Result<T, E = GatewayError> = std::result::Result<T, E>;

/// Errors that the gateway domain can produce.
///
/// Adapters translate this into their own vocabulary — HTTP status codes in
/// `gateway-remote`, JSON-RPC error codes in `gateway-acp` — via
/// [`GatewayError::kind`], so the mapping lives in exactly one place per
/// transport instead of being re-derived at every call site.
///
/// Gateway 领域层可能产生的错误。
///
/// 适配器通过 [`GatewayError::kind`] 将其翻译成自己的词汇——`gateway-remote` 中是 HTTP
/// 状态码，`gateway-acp` 中是 JSON-RPC 错误码——因此每种传输层的映射只存在一处，
/// 而不是在每个调用点重复推导。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// The caller could not be authenticated at all.
    ///
    /// 调用方完全无法通过认证。
    #[error("authentication failed: {0}")]
    AuthenticationFailed(Cow<'static, str>),

    /// The caller is known but not allowed to perform this action.
    ///
    /// 调用方身份已知，但无权执行此操作。
    #[error("permission denied: {0}")]
    PermissionDenied(Cow<'static, str>),

    /// A credential (ticket, pairing code) is expired or already consumed.
    ///
    /// 凭证（ticket、配对码）已过期或已被使用。
    #[error("credential expired or already used: {0}")]
    Expired(Cow<'static, str>),

    /// Too many requests from this caller.
    ///
    /// 该调用方请求过于频繁。
    #[error("rate limited: {0}")]
    RateLimited(Cow<'static, str>),

    /// No such device.
    ///
    /// 设备不存在。
    #[error("device not found: {0}")]
    DeviceNotFound(DeviceId),

    /// No such machine.
    ///
    /// 机器不存在。
    #[error("machine not found: {0}")]
    MachineNotFound(String),

    /// No such session.
    ///
    /// Session 不存在。
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// No such permission request, or it was already answered.
    ///
    /// 权限请求不存在，或已被回答。
    #[error("permission request not found: {0}")]
    PermissionNotFound(PermissionId),

    /// The requested state transition is not legal for the session's status.
    ///
    /// 当前 Session 状态下不允许该状态转换。
    #[error("session {session} cannot {action} while {status}")]
    InvalidSessionState {
        /// Session the caller addressed. / 调用方指定的 Session。
        session: SessionId,
        /// Attempted action, e.g. `prompt`. / 尝试的操作，例如 `prompt`。
        action: &'static str,
        /// Current status. / 当前状态。
        status: &'static str,
    },

    /// The configured agent is unknown or its process could not be started.
    ///
    /// 配置的 Agent 未知，或其进程无法启动。
    #[error("agent unavailable: {0}")]
    AgentUnavailable(String),

    /// The agent spoke ACP incorrectly, or the connection died mid-conversation.
    ///
    /// Agent 违反了 ACP，或连接在会话中途断开。
    #[error("agent protocol error: {0}")]
    AgentProtocol(String),

    /// The event store failed.
    ///
    /// 事件存储失败。
    #[error("event store error: {0}")]
    EventStore(String),

    /// A transport (HTTP, WebSocket, tunnel, relay) failed.
    ///
    /// 传输层（HTTP、WebSocket、tunnel、relay）失败。
    #[error("transport error: {0}")]
    Transport(String),

    /// The caller sent something the gateway cannot interpret.
    ///
    /// 调用方发来了 Gateway 无法解释的内容。
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Something the gateway is responsible for went wrong.
    ///
    /// Gateway 自身负责的环节出错。
    #[error("internal error: {0}")]
    Internal(String),
}

/// Coarse classification used by adapters to pick a status code.
///
/// Keeping this separate from the variants means adding a [`GatewayError`]
/// variant does not force every transport to grow a new arm — only the one
/// `match` below. `ErrorKind` itself is deliberately *not* `non_exhaustive`:
/// adding a kind should break every transport's mapping table until a human
/// decides what status code it deserves.
///
/// 供适配器选择状态码的粗粒度分类。
///
/// 将它与具体变体分开，意味着新增 [`GatewayError`] 变体不会迫使每个传输层多写一个
/// 分支，只需改下面那一个 `match`。而 `ErrorKind` 本身故意*不*标记 `non_exhaustive`：
/// 新增一种 kind 应该让所有传输层的映射表编译失败，直到有人决定它对应哪个状态码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400 — malformed or semantically invalid request. / 请求格式错误或语义无效。
    InvalidRequest,
    /// 401 — no valid credential. / 没有有效凭证。
    Unauthenticated,
    /// 403 — valid credential, insufficient rights. / 凭证有效但权限不足。
    Forbidden,
    /// 404 — addressed resource does not exist. / 目标资源不存在。
    NotFound,
    /// 409 — the resource exists but is in the wrong state. / 资源存在但状态不对。
    Conflict,
    /// 410 — the credential/resource has expired. / 凭证或资源已过期。
    Expired,
    /// 429 — throttled. / 被限流。
    RateLimited,
    /// 503 — a dependency (agent process) is unavailable. / 依赖（Agent 进程）不可用。
    Unavailable,
    /// 500 — everything else. / 其他一切。
    Internal,
}

impl GatewayError {
    /// Classify this error for transport-level mapping.
    ///
    /// 对错误分类，供传输层映射使用。
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::AuthenticationFailed(_) => ErrorKind::Unauthenticated,
            Self::PermissionDenied(_) => ErrorKind::Forbidden,
            Self::Expired(_) => ErrorKind::Expired,
            Self::RateLimited(_) => ErrorKind::RateLimited,
            Self::DeviceNotFound(_)
            | Self::MachineNotFound(_)
            | Self::SessionNotFound(_)
            | Self::PermissionNotFound(_) => ErrorKind::NotFound,
            Self::InvalidSessionState { .. } => ErrorKind::Conflict,
            Self::AgentUnavailable(_) => ErrorKind::Unavailable,
            Self::InvalidRequest(_) => ErrorKind::InvalidRequest,
            Self::AgentProtocol(_)
            | Self::EventStore(_)
            | Self::Transport(_)
            | Self::Internal(_) => ErrorKind::Internal,
        }
    }

    /// Stable machine-readable code, safe to expose to remote clients.
    ///
    /// 稳定的机器可读错误码，可安全地暴露给远程客户端。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::AuthenticationFailed(_) => "authentication_failed",
            Self::PermissionDenied(_) => "permission_denied",
            Self::Expired(_) => "expired",
            Self::RateLimited(_) => "rate_limited",
            Self::DeviceNotFound(_) => "device_not_found",
            Self::MachineNotFound(_) => "machine_not_found",
            Self::SessionNotFound(_) => "session_not_found",
            Self::PermissionNotFound(_) => "permission_not_found",
            Self::InvalidSessionState { .. } => "invalid_session_state",
            Self::AgentUnavailable(_) => "agent_unavailable",
            Self::AgentProtocol(_) => "agent_protocol_error",
            Self::EventStore(_) => "event_store_error",
            Self::Transport(_) => "transport_error",
            Self::InvalidRequest(_) => "invalid_request",
            Self::Internal(_) => "internal_error",
        }
    }

    /// Shorthand for [`GatewayError::Internal`].
    ///
    /// [`GatewayError::Internal`] 的简写。
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::Internal(message.to_string())
    }

    /// Shorthand for [`GatewayError::InvalidRequest`].
    ///
    /// [`GatewayError::InvalidRequest`] 的简写。
    pub fn invalid_request(message: impl std::fmt::Display) -> Self {
        Self::InvalidRequest(message.to_string())
    }

    /// Shorthand for [`GatewayError::Transport`].
    ///
    /// [`GatewayError::Transport`] 的简写。
    pub fn transport(message: impl std::fmt::Display) -> Self {
        Self::Transport(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_the_documented_http_families() {
        assert_eq!(
            GatewayError::SessionNotFound(SessionId::new("sess_x")).kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            GatewayError::Expired("ticket".into()).kind(),
            ErrorKind::Expired
        );
        assert_eq!(
            GatewayError::InvalidSessionState {
                session: SessionId::new("sess_x"),
                action: "prompt",
                status: "cancelling",
            }
            .kind(),
            ErrorKind::Conflict
        );
    }
}
