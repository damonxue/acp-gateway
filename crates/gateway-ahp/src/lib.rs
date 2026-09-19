//! Agent Host Protocol (AHP) channel support.
//!
//! AHP is an outbound channel from the local gateway to a WeChat service. It
//! deliberately does not expose the gateway's HTTP port, so it can replace a
//! Cloudflare tunnel and relay for this use case. The gateway still owns the
//! ACP session; AHP only carries binding control and user-facing messages.
//!
//! The wire adapter is isolated in [`protocol`]. If an AHP service changes its
//! envelope, that module is the only part that needs to change.

#![forbid(unsafe_code)]

mod server;
mod worker;

pub mod protocol;

pub use server::serve_connection;
pub use worker::{AhpStatus, AhpSupervisor};
