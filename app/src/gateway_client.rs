use anyhow::{Result, anyhow};
use gateway_auth::PairingOffer;
use gateway_config::{GatewayConfig, default_config_path};
use gateway_core::device::Device;
use gateway_core::event::AgentEvent;
use gateway_core::manager::SessionSnapshot;
use gateway_core::session::AgentSession;
use gateway_remote::protocol::{AgentSummary, PermissionAnswer, PromptInput, ServerMessage};
use gateway_remote::state::ComponentHealth;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use gateway_core::machine::Machine;

#[derive(Clone, Debug)]
pub struct GatewayClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GatewayHealth {
    pub status: String,
    pub version: String,
    pub machine: Machine,
    pub sessions: SessionCounts,
    pub components: Vec<ComponentHealth>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionCounts {
    pub active: usize,
    pub created_total: usize,
}

/// Local channel status exposed by the gateway.  The payload is deliberately
/// kept small so the UI never needs to know channel credentials.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AhpStatusSnapshot {
    pub enabled: bool,
    pub status: Option<AhpChannelStatus>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AhpChannelStatus {
    #[serde(rename = "state")]
    pub state: String,
    #[serde(default)]
    pub qr_code: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionList {
    sessions: Vec<AgentSession>,
}

#[derive(Debug, Deserialize)]
struct DeviceList {
    devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
struct AgentList {
    agents: Vec<AgentSummary>,
}

#[derive(Debug, Deserialize)]
struct SessionIdResponse {
    id: String,
}

impl GatewayClient {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    #[must_use]
    pub fn discover_base_url() -> String {
        let path = default_config_path();
        if let Ok(config) = GatewayConfig::load(&path) {
            format!("http://{}", config.bind_addr())
        } else {
            "http://127.0.0.1:48100".to_owned()
        }
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn session_stream_url(&self, session_id: &str, after_seq: u64) -> String {
        let base = self
            .base_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        format!(
            "{}/sessions/{}/stream?after_seq={after_seq}",
            base.trim_end_matches('/'),
            encode_path_segment(session_id)
        )
    }

    #[must_use]
    pub fn remote_socket_url(&self, ticket: &str) -> String {
        let base = self
            .base_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        format!(
            "{}/remote?ticket={}",
            base.trim_end_matches('/'),
            encode_path_segment(ticket)
        )
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.http.get(self.url(path)).send().await?;
        self.decode(response).await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<T> {
        let response = self.http.post(self.url(path)).json(&body).send().await?;
        self.decode(response).await
    }

    async fn decode<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|body| {
                    body.get("message")
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or(text);
            return Err(anyhow!("{message}"));
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    pub async fn health(&self) -> Result<GatewayHealth> {
        self.get_json("/health").await
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentSummary>> {
        let AgentList { agents } = self.get_json("/agents").await?;
        Ok(agents)
    }

    pub async fn list_sessions(&self) -> Result<Vec<AgentSession>> {
        let SessionList { sessions } = self.get_json("/sessions").await?;
        Ok(sessions)
    }

    pub async fn refresh_sessions(&self) -> Result<Vec<AgentSession>> {
        let SessionList { sessions } = self
            .post_json("/sessions/refresh", serde_json::json!({}))
            .await?;
        Ok(sessions)
    }

    pub async fn get_session(&self, session_id: &str) -> Result<SessionSnapshot> {
        self.get_json(&format!("/sessions/{}", encode_path_segment(session_id)))
            .await
    }

    pub async fn list_devices(&self) -> Result<Vec<Device>> {
        let DeviceList { devices } = self.get_json("/devices").await?;
        Ok(devices)
    }

    pub async fn ahp_status(&self) -> Result<AhpStatusSnapshot> {
        self.get_json("/ahp/status").await
    }

    pub async fn ahp_bind(&self, session_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json("/ahp/bind", serde_json::json!({"session_id": session_id}))
            .await?;
        Ok(())
    }

    pub async fn ahp_unbind(&self) -> Result<()> {
        let _: serde_json::Value = self.post_json("/ahp/unbind", serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn create_session(
        &self,
        agent_id: &str,
        workspace: impl Into<String>,
        cwd: Option<impl Into<String>>,
        additional_directories: Vec<String>,
    ) -> Result<AgentSession> {
        #[derive(serde::Serialize)]
        struct CreateSessionBody<'a> {
            agent_id: &'a str,
            workspace: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            cwd: Option<String>,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            additional_directories: Vec<String>,
        }

        self.post_json(
            "/sessions",
            CreateSessionBody {
                agent_id,
                workspace: workspace.into(),
                cwd: cwd.map(Into::into),
                additional_directories,
            },
        )
        .await
    }

    pub async fn prompt(&self, session_id: &str, prompt: PromptInput) -> Result<()> {
        #[derive(serde::Serialize)]
        struct PromptBody {
            prompt: PromptInput,
        }

        let _: serde_json::Value = self
            .post_json(
                &format!("/sessions/{}/prompt", encode_path_segment(session_id)),
                PromptBody { prompt },
            )
            .await?;
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                &format!("/sessions/{}/cancel", encode_path_segment(session_id)),
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    pub async fn permission(&self, session_id: &str, answer: PermissionAnswer) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                &format!("/sessions/{}/permission", encode_path_segment(session_id)),
                answer,
            )
            .await?;
        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                &format!("/sessions/{}/close", encode_path_segment(session_id)),
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    pub async fn begin_pairing(&self) -> Result<PairingOffer> {
        self.post_json("/pairing/begin", serde_json::json!({}))
            .await
    }

    pub async fn revoke_device(&self, device_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(
                &format!("/devices/{}/revoke", encode_path_segment(device_id)),
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    pub async fn subscribe_message(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        let (socket, _) =
            tokio_tungstenite::connect_async(self.session_stream_url(session_id, after_seq))
                .await?;
        Ok(socket)
    }

    pub async fn parse_remote_frame(value: serde_json::Value) -> Result<RemoteFrame> {
        if value.get("seq").and_then(|seq| seq.as_u64()).is_some()
            && value
                .get("session_id")
                .and_then(|session_id| session_id.as_str())
                .is_some()
        {
            Ok(RemoteFrame::Event(serde_json::from_value(value)?))
        } else {
            Ok(RemoteFrame::Control(serde_json::from_value(value)?))
        }
    }
}

#[derive(Clone, Debug)]
pub enum RemoteFrame {
    Control(ServerMessage),
    Event(AgentEvent),
}

fn encode_path_segment(raw: &str) -> String {
    raw.chars()
        .flat_map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => vec![ch],
            _ => format!("%{:02X}", ch as u32).chars().collect(),
        })
        .collect()
}
