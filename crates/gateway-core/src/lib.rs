//! # gateway-core
//!
//! The **domain layer** of Agent Gateway.
//!
//! Agent Gateway 的**领域层**。
//!
//! This crate owns the vocabulary of the product — machines, sessions, events,
//! permissions — plus the orchestration rules that must hold no matter which
//! transport, database or agent implementation is plugged in.
//!
//! 本 crate 定义产品的词汇表——机器、Session、事件、权限——以及无论接入哪种传输层、
//! 数据库或 Agent 实现都必须成立的编排规则。
//!
//! ## Architecture / 架构
//!
//! `gateway-core` follows the *ports and adapters* (hexagonal) style:
//!
//! `gateway-core` 采用*端口与适配器*（六边形）风格：
//!
//! ```text
//!            ┌──────────────────────── adapters ────────────────────────┐
//!            │  gateway-remote   gateway-store   gateway-acp   ...      │
//!            └───────▲───────────────▲───────────────▲──────────────────┘
//!                    │ drives        │ implements    │ implements
//!            ┌───────┴───────────────┴───────────────┴──────────────────┐
//!            │                    gateway-core                          │
//!            │  SessionManager · EventBus · ports::* · domain types     │
//!            └──────────────────────────────────────────────────────────┘
//! ```
//!
//! * **Driving side** — [`SessionManager`] is the single entry point used by
//!   HTTP handlers, WebSocket handlers and the CLI.
//! * **Driven side** — every external dependency is expressed as a trait in
//!   [`ports`] ([`ports::SessionRepository`], [`ports::EventStore`], …) or as
//!   [`agent::AgentRuntime`]. The domain never names a database, an HTTP
//!   framework, or a wire protocol.
//!
//! * **驱动侧**——[`SessionManager`] 是 HTTP handler、WebSocket handler 和 CLI 唯一的入口。
//! * **被驱动侧**——每个外部依赖都表达为 [`ports`] 中的 trait（[`ports::SessionRepository`]、
//!   [`ports::EventStore`] 等）或 [`agent::AgentRuntime`]。领域层从不提及具体的数据库、
//!   HTTP 框架或线上协议。
//!
//! That inversion is what makes the two very different session sources —
//! a gateway-spawned ACP subprocess and an IDE-owned session proxied through
//! `acp-bridge` — behave identically for every remote client: both are just an
//! [`agent::AgentSessionHandle`].
//!
//! 正是这个依赖倒置，让两种截然不同的 Session 来源——Gateway 自己启动的 ACP 子进程，
//! 以及经 `acp-bridge` 代理的 IDE 自有 Session——对所有远程客户端表现完全一致：
//! 两者都只是一个 [`agent::AgentSessionHandle`]。
//!
//! ## Invariants enforced here / 在此处强制的不变式
//!
//! 1. `seq` is strictly monotonic and gap-free per session.
//! 2. An event is persisted **before** it is broadcast, so a reconnecting
//!    client can never observe an event that replay would not return.
//! 3. Remote clients disconnecting never affects agent processes.
//!
//! 1. 每个 Session 内的 `seq` 严格递增且无空洞。
//! 2. 事件先落库、**再**广播，因此重连的客户端绝不会看到一个回放拿不到的事件。
//! 3. 远程客户端断开永远不会影响 Agent 进程。

#![forbid(unsafe_code)]

pub mod agent;
pub mod bridge;
pub mod bus;
pub mod credential;
pub mod device;
pub mod error;
pub mod event;
pub mod ids;
pub mod machine;
pub mod manager;
pub mod metrics;
pub mod permission;
pub mod ports;
pub mod session;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transcript;

pub use agent::{
    AcpSessionInfo, AgentDescriptor, AgentRuntime, AgentSessionHandle, EventSink, LaunchRequest,
    LaunchedAgent, PromptBlock,
};
pub use bridge::{BridgeMessage, DaemonMessage};
pub use bus::{EventBus, EventSubscription};
pub use credential::{PairingCode, WsTicket};
pub use device::Device;
pub use error::{ErrorKind, GatewayError, Result};
pub use event::{AgentEvent, EventDraft, EventType};
pub use ids::{AgentId, DeviceId, EventId, MachineId, PermissionId, SessionId};
pub use machine::Machine;
pub use manager::{
    AdoptSessionSpec, AgentCatalog, CreateSessionSpec, SessionManager, SessionManagerConfig,
    SessionSnapshot,
};
pub use permission::{
    PermissionDecision, PermissionOption, PermissionOptionKind, PermissionRequest,
};
pub use session::{AgentSession, SessionOrigin, SessionStatus};
pub use transcript::{Transcript, TranscriptItem, fold_transcript, tool_summary};
