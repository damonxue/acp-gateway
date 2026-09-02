//! # gateway-acp
//!
//! The ACP side of the gateway: launching agents, speaking the protocol, and
//! turning what agents say into gateway events.
//!
//! This crate is the only place in the workspace that depends on
//! `agent-client-protocol`. Everything above it works with
//! [`gateway_core::AgentSessionHandle`] and [`gateway_core::AgentEvent`], so
//! an ACP SDK upgrade cannot ripple into the HTTP layer or the mobile
//! protocol.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`runtime`] | spawn agents, own the connection, execute commands |
//! | [`mapper`] | ACP types ↔ gateway events (the whole translation layer) |
//! | [`workspace_fs`] | the `fs/*` methods the gateway answers for agents |
//!
//! ## Protocol version
//!
//! The gateway negotiates ACP `v1` — the version Zed, Codex, Claude Code and
//! OpenCode all speak today — through SDK 2.0. Agents that answer with a
//! different version are rejected by the SDK during `initialize`, which
//! surfaces as [`gateway_core::GatewayError::AgentUnavailable`].

#![forbid(unsafe_code)]

pub mod bridge;
pub mod mapper;
pub mod runtime;
pub mod workspace_fs;

pub use bridge::{AcpBridgeConfig, BridgeControlMode, run_bridge};
pub use runtime::{AcpAgentRuntime, AcpRuntimeConfig};
pub use workspace_fs::WorkspaceScope;
