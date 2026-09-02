//! A thin client for the daemon's local API.
//!
//! Subcommands drive the *running* gateway rather than opening its database:
//! two processes writing the same SQLite file and minting the same sequence
//! numbers is exactly the race the event log is designed to prevent. If the
//! daemon is not running, the command says so.

use anyhow::{Context, Result, bail};
use gateway_config::GatewayConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Talks to `agent-gateway run` over loopback.
pub(crate) struct LocalClient {
    base: String,
    http: reqwest::Client,
}

impl LocalClient {
    /// Build a client for the address in `config`.
    #[must_use]
    pub(crate) fn new(config: &GatewayConfig) -> Self {
        Self {
            base: format!("http://{}", config.bind_addr()),
            http: reqwest::Client::new(),
        }
    }

    /// The WebSocket URL for a path on the local API.
    #[must_use]
    pub(crate) fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.base.trim_start_matches("http://"))
    }

    /// `GET path`
    ///
    /// # Errors
    /// Fails if the daemon is unreachable or answers with an error status.
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .with_context(|| self.unreachable(path))?;
        Self::decode(response).await
    }

    /// `POST path`
    ///
    /// # Errors
    /// As [`Self::get`].
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .with_context(|| self.unreachable(path))?;
        Self::decode(response).await
    }

    fn unreachable(&self, path: &str) -> String {
        format!(
            "cannot reach the gateway at {}{path}; is `agent-gateway run` started?",
            self.base
        )
    }

    async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // The API's error body is `{"code":…,"message":…}`; show the
            // message rather than the raw JSON.
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value["message"].as_str().map(ToOwned::to_owned))
                .unwrap_or(body);
            bail!("gateway returned {status}: {message}");
        }
        serde_json::from_str(&body).context("the gateway returned an unexpected response")
    }
}
