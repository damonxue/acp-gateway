//! # gateway-relay
//!
//! The control plane: how a gateway announces itself, stays visible, and wakes
//! a phone — plus a reference implementation of the other end.
//!
//! ```text
//!   gateway ──register/heartbeat/push──► relay ◄──list machines── phone
//!      ▲                                   │
//!      └───── pairing & ticket, forwarded ─┘
//! ```
//!
//! ## The rule that shapes this crate
//!
//! > The relay is a connection layer. The gateway is the authority.
//!
//! Concretely: the relay learns machine names, public keys, endpoints and
//! presence. It never sees a prompt, agent output, terminal output or source
//! code, and it cannot mint a credential — pairing and ticket requests are
//! forwarded to the gateway, which owns the device registry. A compromised
//! relay can therefore deny service, but it cannot read a session or open one.
//!
//! [`protocol`] is the contract, [`client`] is the gateway side, [`worker`]
//! keeps presence and push current, and [`server`] is a runnable reference
//! relay.

#![forbid(unsafe_code)]

pub mod client;
pub mod protocol;
pub mod server;
pub mod worker;

pub use client::RelayClient;
pub use protocol::{MachineSummary, PushEvent, PushKind};
pub use server::{RelayState, router as relay_router, serve as serve_relay};
pub use worker::{RelayStatus, RelayWorker};
