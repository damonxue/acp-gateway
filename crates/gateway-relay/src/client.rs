//! The gateway's client for a relay control plane.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gateway_auth::MachineIdentity;
use gateway_core::error::{GatewayError, Result};
use gateway_core::machine::Machine;
use serde::Serialize;
use tracing::debug;
use url::Url;

use crate::protocol::{HeartbeatRequest, PushEvent, PushRequest, RegisterRequest, challenge};

/// Talks to the relay on behalf of one machine.
#[derive(Clone, Debug)]
pub struct RelayClient {
    http: reqwest::Client,
    endpoint: Url,
    identity: Arc<MachineIdentity>,
    token: Option<String>,
}

impl RelayClient {
    /// Build a client for `endpoint`.
    ///
    /// # Errors
    /// Fails if the endpoint is not a valid URL or the HTTP client cannot be
    /// constructed.
    pub fn new(
        endpoint: &str,
        identity: Arc<MachineIdentity>,
        token: Option<String>,
    ) -> Result<Self> {
        let endpoint = Url::parse(endpoint)
            .map_err(|error| GatewayError::invalid_request(format!("relay endpoint: {error}")))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("agent-gateway/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| GatewayError::transport(format!("http client: {error}")))?;
        Ok(Self {
            http,
            endpoint,
            identity,
            token,
        })
    }

    /// Announce this machine and its public key.
    ///
    /// # Errors
    /// [`GatewayError::Transport`] if the relay is unreachable or refuses.
    pub async fn register(&self, machine: &Machine) -> Result<()> {
        let issued_at = Utc::now();
        let body = RegisterRequest {
            machine_id: machine.id.clone(),
            public_key: self.identity.public_key_base64(),
            name: machine.name.clone(),
            platform: machine.platform.clone(),
            version: machine.version.clone(),
            endpoint: machine.public_endpoint.clone(),
            issued_at,
            signature: self.sign("register", issued_at),
        };
        self.post("machines/register", &body).await
    }

    /// Report that this machine is still online.
    ///
    /// # Errors
    /// As [`Self::register`].
    pub async fn heartbeat(&self, endpoint: Option<String>) -> Result<()> {
        let issued_at = Utc::now();
        let body = HeartbeatRequest {
            endpoint,
            issued_at,
            signature: self.sign("heartbeat", issued_at),
        };
        let path = format!("machines/{}/heartbeat", self.identity.machine_id());
        self.post(&path, &body).await
    }

    /// Hand the relay a batch of content-free notifications.
    ///
    /// # Errors
    /// As [`Self::register`].
    pub async fn push_events(&self, events: Vec<PushEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let issued_at = Utc::now();
        let body = PushRequest {
            events,
            issued_at,
            signature: self.sign("push", issued_at),
        };
        let path = format!("machines/{}/push-events", self.identity.machine_id());
        self.post(&path, &body).await
    }

    fn sign(&self, action: &str, issued_at: chrono::DateTime<Utc>) -> String {
        self.identity
            .sign_base64(challenge(action, self.identity.machine_id(), issued_at).as_bytes())
    }

    async fn post(&self, path: &str, body: &impl Serialize) -> Result<()> {
        let url = self
            .endpoint
            .join(path)
            .map_err(|error| GatewayError::invalid_request(format!("relay path: {error}")))?;
        let mut request = self.http.post(url.clone()).json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| GatewayError::transport(format!("relay {path}: {error}")))?;
        if response.status().is_success() {
            debug!(%path, "relay call succeeded");
            return Ok(());
        }
        let status = response.status();
        // The body may explain the refusal; it never contains our secrets.
        let detail = response.text().await.unwrap_or_default();
        Err(GatewayError::transport(format!(
            "relay {path} returned {status}: {detail}"
        )))
    }
}
