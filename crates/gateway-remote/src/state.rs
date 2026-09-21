//! Shared state and the access policy every request is judged against.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use gateway_ahp::AhpSupervisor;
use gateway_auth::AuthService;
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::SessionId;
use gateway_core::machine::Machine;
use gateway_core::manager::SessionManager;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// Health of one subsystem, as reported by `/health`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Subsystem name, e.g. `tunnel`.
    pub name: String,
    /// `up`, `down`, `starting`, `disabled`, …
    pub state: String,
    /// Optional human-readable detail. Must never contain a secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Something that can report its health to `/health`.
///
/// Declared here rather than in the subsystems themselves so that
/// `gateway-tunnel` and `gateway-relay` do not have to know an HTTP server
/// exists; the CLI adapts them at wiring time.
pub trait HealthSource: Send + Sync + std::fmt::Debug {
    /// Current health.
    fn health(&self) -> ComponentHealth;
}

/// A local channel that can switch its active Gateway session.
#[async_trait::async_trait]
pub trait SessionBinder: Send + Sync + std::fmt::Debug {
    /// Bind the channel to an existing session.
    async fn bind(&self, session_id: SessionId) -> std::result::Result<(), String>;

    /// Remove the active binding while retaining channel credentials.
    async fn unbind(&self) -> std::result::Result<(), String>;
}

/// Everything the HTTP and WebSocket layers need.
#[derive(Clone, Debug)]
pub struct AppState {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    manager: Arc<SessionManager>,
    auth: Arc<AuthService>,
    machine: RwLock<Machine>,
    trust_loopback: bool,
    health_sources: Vec<Arc<dyn HealthSource>>,
    ahp: Option<Arc<AhpSupervisor>>,
    wechat_binder: Option<Arc<dyn SessionBinder>>,
}

impl AppState {
    /// Assemble the state.
    #[must_use]
    pub fn new(
        manager: Arc<SessionManager>,
        auth: Arc<AuthService>,
        machine: Machine,
        trust_loopback: bool,
        health_sources: Vec<Arc<dyn HealthSource>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                manager,
                auth,
                machine: RwLock::new(machine),
                trust_loopback,
                health_sources,
                ahp: None,
                wechat_binder: None,
            }),
        }
    }

    /// Session orchestration.
    #[must_use]
    pub fn manager(&self) -> &Arc<SessionManager> {
        &self.inner.manager
    }

    /// Pairing, devices and tickets.
    #[must_use]
    pub fn auth(&self) -> &Arc<AuthService> {
        &self.inner.auth
    }

    /// The machine record, as currently known.
    #[must_use]
    pub fn machine(&self) -> Machine {
        self.inner.machine.read().expect("machine lock").clone()
    }

    /// Record the public endpoint once a tunnel is up.
    pub fn set_public_endpoint(&self, endpoint: Option<String>) {
        let mut machine = self.inner.machine.write().expect("machine lock");
        machine.public_endpoint = endpoint;
        machine.updated_at = chrono::Utc::now();
    }

    /// Health of every registered subsystem.
    #[must_use]
    pub fn component_health(&self) -> Vec<ComponentHealth> {
        self.inner
            .health_sources
            .iter()
            .map(|source| source.health())
            .collect()
    }

    /// Whether loopback callers may skip credentials.
    #[must_use]
    pub fn trust_loopback(&self) -> bool {
        self.inner.trust_loopback
    }

    /// Attach the optional outbound AHP channel after the base state is built.
    #[must_use]
    pub fn with_ahp(mut self, ahp: Arc<AhpSupervisor>) -> Self {
        let inner =
            Arc::get_mut(&mut self.inner).expect("AHP must be attached before cloning AppState");
        inner.ahp = Some(ahp);
        self
    }

    /// Configured AHP channel, if enabled.
    #[must_use]
    pub fn ahp(&self) -> Option<&Arc<AhpSupervisor>> {
        self.inner.ahp.as_ref()
    }

    /// Attach the local WeChat binding adapter after the base state is built.
    #[must_use]
    pub fn with_wechat_binder(mut self, binder: Arc<dyn SessionBinder>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("WeChat binder must be attached before cloning AppState");
        inner.wechat_binder = Some(binder);
        self
    }

    /// Configured local WeChat session binder, if any.
    #[must_use]
    pub fn wechat_binder(&self) -> Option<&Arc<dyn SessionBinder>> {
        self.inner.wechat_binder.as_ref()
    }
}

/// Where a request came from.
///
/// The gateway binds to loopback and is reached from the outside through
/// `cloudflared`, which *also* connects over loopback. So "the peer address is
/// 127.0.0.1" is not enough to call a request local: a request carrying
/// Cloudflare's forwarding headers is remote no matter which socket it arrived
/// on, and must present a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Peer address is loopback.
    pub loopback: bool,
    /// Request carries tunnel/proxy headers.
    pub forwarded: bool,
}

/// Headers that mean "this request was forwarded by something".
const FORWARDED_HEADERS: [&str; 4] = ["cf-connecting-ip", "cf-ray", "x-forwarded-for", "forwarded"];

impl Access {
    /// Whether this request may use loopback-only endpoints.
    #[must_use]
    pub fn is_local(self) -> bool {
        self.loopback && !self.forwarded
    }

    /// Fail unless the request is genuinely local.
    ///
    /// # Errors
    /// [`GatewayError::PermissionDenied`] for anything arriving through a
    /// tunnel or a non-loopback interface.
    pub fn require_local(self) -> Result<()> {
        if self.is_local() {
            Ok(())
        } else {
            Err(GatewayError::PermissionDenied(
                "this endpoint is only available to local callers".into(),
            ))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Access {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, ApiError> {
        let loopback = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
        let forwarded = FORWARDED_HEADERS
            .iter()
            .any(|name| parts.headers.contains_key(*name));
        Ok(Self {
            loopback,
            forwarded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tunnelled_request_is_never_treated_as_local() {
        assert!(
            Access {
                loopback: true,
                forwarded: false
            }
            .is_local()
        );
        assert!(
            !Access {
                loopback: true,
                forwarded: true
            }
            .is_local()
        );
        assert!(
            !Access {
                loopback: false,
                forwarded: false
            }
            .is_local()
        );
    }
}
