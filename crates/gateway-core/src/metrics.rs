//! Metric names, declared once.
//!
//! 指标名称，只声明一次。
//!
//! The gateway emits metrics through the `metrics` facade from the adapter
//! crates; this module exists so the *names* live in the domain and cannot
//! drift between the code that records them and the docs that describe them.
//!
//! Gateway 在各适配器 crate 里通过 `metrics` 门面上报指标；本模块的存在是为了让
//! 指标*名称*住在领域层，避免记录代码与文档描述之间出现偏差。

/// Total sessions ever created on this machine.
/// 本机累计创建的 Session 数。
pub const SESSIONS_TOTAL: &str = "gateway_sessions_total";
/// Sessions currently held in memory by the manager.
/// manager 当前在内存中持有的 Session 数。
pub const SESSIONS_ACTIVE: &str = "gateway_sessions_active";
/// Live remote WebSocket connections.
/// 存活的远程 WebSocket 连接数。
pub const WS_CONNECTIONS: &str = "gateway_ws_connections";
/// Events appended to the store.
/// 已追加到存储的事件数。
pub const EVENT_COUNT: &str = "gateway_event_count";
/// Events served through replay rather than live broadcast.
/// 通过回放（而非实时广播）送出的事件数。
pub const EVENT_REPLAY_COUNT: &str = "gateway_event_replay_count";
/// Agent launch or protocol failures.
/// Agent 启动或协议失败次数。
pub const AGENT_ERRORS: &str = "gateway_agent_errors";
/// Permission requests raised by agents.
/// Agent 发起的权限请求数。
pub const PERMISSION_REQUESTS: &str = "gateway_permission_requests";
/// Reconnect attempts against the relay/tunnel.
/// 对 relay / tunnel 的重连尝试次数。
pub const REMOTE_RECONNECTS: &str = "gateway_remote_reconnects";
