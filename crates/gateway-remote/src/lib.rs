//! # gateway-remote
//!
//! The gateway's front door: a loopback HTTP API and the WebSocket protocol
//! remote clients speak.
//!
//! ```text
//!   phone  ──https───────────► /app     ┐  (the web client itself)
//!   phone  ──wss (ticket)──► /remote   │
//!   IDE    ──http (loopback)─► /sessions ├─► AppState ─► SessionManager
//!   CLI    ──http (loopback)─► /devices  ┘
//! ```
//!
//! The crate contains no session logic. Handlers translate a request into a
//! [`gateway_core::SessionManager`] call and translate the result back; every
//! rule about what a session may do lives in the domain, where both transports
//! get it for free.
//!
//! See [`protocol`] for the remote message schema and [`ws`] for the replay /
//! reconnect guarantees.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod error;
pub mod http;
pub mod protocol;
pub mod state;
pub mod web_ui;
pub mod ws;

use std::net::SocketAddr;

use gateway_core::error::{GatewayError, Result};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::info;

pub use error::ApiError;
pub use state::{Access, AppState, ComponentHealth, HealthSource, SessionBinder};

/// A bound, not-yet-serving HTTP server.
///
/// Binding is separated from serving so the caller can learn the real port
/// (useful when `bind` used port 0) and log the URL before the first request.
#[derive(Debug)]
pub struct RemoteServer {
    listener: TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
}

impl RemoteServer {
    /// Bind the API to `addr`.
    ///
    /// # Errors
    /// Fails if the address is already in use or not permitted.
    pub async fn bind(addr: SocketAddr, state: AppState) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|error| GatewayError::transport(format!("cannot bind {addr}: {error}")))?;
        let local_addr = listener.local_addr().map_err(|error| {
            GatewayError::transport(format!("cannot read bound address: {error}"))
        })?;
        let router = http::router(state).layer(TraceLayer::new_for_http());
        Ok(Self {
            listener,
            router,
            local_addr,
        })
    }

    /// The address actually bound.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until `shutdown` resolves.
    ///
    /// # Errors
    /// Fails if the server stops with an I/O error.
    pub async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        info!(address = %self.local_addr, "gateway api listening");
        axum::serve(
            self.listener,
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| GatewayError::transport(format!("http server stopped: {error}")))
    }
}
