use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use gateway_core::agent::PromptBlock;
use gateway_core::event::AgentEvent;
use gateway_core::ids::{MachineId, SessionId};
use gateway_core::manager::SessionManager;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::Url;

use crate::protocol::{AHP_VERSION, ClientFrame, ServerFrame};

const DEFAULT_RECONNECT: Duration = Duration::from_secs(3);
const OUTGOING_QUEUE: usize = 32;

/// Public AHP connection state, safe to expose in `/health` and the desktop UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AhpStatus {
    Disabled,
    Connecting,
    AwaitingQr { qr_code: String },
    Ready { channel_id: Option<String> },
    Bound { session_id: SessionId },
    Reconnecting { reason: String },
    Failed { reason: String },
}

impl AhpStatus {
    /// Compact health state name.
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::AwaitingQr { .. } => "awaiting_qr",
            Self::Ready { .. } => "ready",
            Self::Bound { .. } => "bound",
            Self::Reconnecting { .. } => "reconnecting",
            Self::Failed { .. } => "failed",
        }
    }

    /// Safe detail for health output.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            // The QR payload is a login credential and must only be returned
            // by the loopback `/ahp/status` endpoint, never public `/health`.
            Self::AwaitingQr { .. } => Some("scan_required".to_owned()),
            Self::Reconnecting { reason } | Self::Failed { reason } => Some(reason.clone()),
            Self::Bound { session_id } => Some(session_id.to_string()),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Command {
    Bind { session_id: SessionId },
    Unbind,
}

/// Supervises one outbound AHP channel.
#[derive(Debug)]
pub struct AhpSupervisor {
    status: Arc<RwLock<AhpStatus>>,
    commands: Option<mpsc::UnboundedSender<Command>>,
    manager: Option<Arc<SessionManager>>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl AhpSupervisor {
    /// A disabled channel, used when `[ahp]` is absent.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            status: Arc::new(RwLock::new(AhpStatus::Disabled)),
            commands: None,
            manager: None,
            cancel: CancellationToken::new(),
            task: None,
        }
    }

    /// Start an outbound channel supervisor.
    pub fn start(
        endpoint: String,
        token: Option<String>,
        machine_id: MachineId,
        manager: Arc<SessionManager>,
        reconnect: Duration,
    ) -> Result<Self, String> {
        let endpoint = Url::parse(&endpoint).map_err(|error| format!("AHP endpoint: {error}"))?;
        if !matches!(endpoint.scheme(), "ws" | "wss") {
            return Err("AHP endpoint must use ws:// or wss://".into());
        }
        let status = Arc::new(RwLock::new(AhpStatus::Connecting));
        let cancel = CancellationToken::new();
        let (commands, rx) = mpsc::unbounded_channel();
        let task_status = Arc::clone(&status);
        let task_cancel = cancel.clone();
        let task = tokio::spawn(run(
            endpoint,
            token,
            machine_id,
            Arc::clone(&manager),
            rx,
            task_status,
            task_cancel,
            if reconnect.is_zero() {
                DEFAULT_RECONNECT
            } else {
                reconnect
            },
        ));
        Ok(Self {
            status,
            commands: Some(commands),
            manager: Some(manager),
            cancel,
            task: Some(task),
        })
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> AhpStatus {
        self.status.read().expect("AHP status lock").clone()
    }

    /// Explicitly bind an existing gateway session to the channel.
    pub async fn bind(&self, session_id: SessionId) -> Result<(), String> {
        let Some(commands) = &self.commands else {
            return Err("AHP is disabled".into());
        };
        if let Some(manager) = &self.manager
            && manager.get_session(&session_id).await.is_err()
        {
            return Err(format!("session `{session_id}` was not found"));
        }
        commands
            .send(Command::Bind { session_id })
            .map_err(|_| "AHP supervisor is stopped".to_owned())
    }

    /// Remove the current binding while keeping the channel logged in.
    pub async fn unbind(&self) -> Result<(), String> {
        let Some(commands) = &self.commands else {
            return Err("AHP is disabled".into());
        };
        commands
            .send(Command::Unbind)
            .map_err(|_| "AHP supervisor is stopped".to_owned())
    }

    /// Stop the channel.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

impl Drop for AhpSupervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run(
    endpoint: Url,
    token: Option<String>,
    machine_id: MachineId,
    manager: Arc<SessionManager>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    status: Arc<RwLock<AhpStatus>>,
    cancel: CancellationToken,
    reconnect: Duration,
) {
    let mut desired_binding: Option<SessionId> = None;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        set_status(&status, AhpStatus::Connecting);
        let connection = tokio::select! {
            result = connect(&endpoint, token.as_deref()) => result,
            _ = cancel.cancelled() => return,
        };
        let (socket, _) = match connection {
            Ok(connection) => connection,
            Err(error) => {
                set_status(
                    &status,
                    AhpStatus::Reconnecting {
                        reason: error.to_string(),
                    },
                );
                if wait_or_cancel(&cancel, reconnect).await {
                    return;
                }
                continue;
            }
        };
        let (mut sink, mut stream) = socket.split();
        if send(
            &mut sink,
            ClientFrame::Hello {
                version: AHP_VERSION,
                machine_id: machine_id.to_string(),
            },
        )
        .await
        .is_err()
        {
            set_status(
                &status,
                AhpStatus::Reconnecting {
                    reason: "hello failed".into(),
                },
            );
            continue;
        }
        let (outgoing, mut outgoing_rx) = mpsc::channel::<ClientFrame>(OUTGOING_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(frame) = outgoing_rx.recv().await {
                if send(&mut sink, frame).await.is_err() {
                    break;
                }
            }
        });
        let mut event_task: Option<JoinHandle<()>> = None;
        let mut current_binding = desired_binding.clone();
        if let Some(session_id) = current_binding.clone() {
            let _ = outgoing.send(ClientFrame::Bind { session_id }).await;
            if let Some(session_id) = current_binding.clone()
                && let Ok(subscription) = manager.subscribe(&session_id)
            {
                event_task = Some(tokio::spawn(forward_events(subscription, outgoing.clone())));
            }
        }
        let mut connected = true;
        while connected {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Bind { session_id }) => {
                        desired_binding = Some(session_id.clone());
                        current_binding = Some(session_id.clone());
                        if let Some(task) = event_task.take() { task.abort(); }
                        match manager.subscribe(&session_id) {
                            Ok(subscription) => {
                                let _ = outgoing.send(ClientFrame::Bind { session_id: session_id.clone() }).await;
                                event_task = Some(tokio::spawn(forward_events(subscription, outgoing.clone())));
                                set_status(&status, AhpStatus::Bound { session_id });
                            }
                            Err(error) => { set_status(&status, AhpStatus::Failed { reason: error.to_string() }); }
                        }
                    }
                    Some(Command::Unbind) => {
                        desired_binding = None;
                        current_binding = None;
                        if let Some(task) = event_task.take() { task.abort(); }
                        let _ = outgoing.send(ClientFrame::Unbind).await;
                        set_status(&status, AhpStatus::Ready { channel_id: None });
                    }
                    None => return,
                },
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(frame) = serde_json::from_str::<ServerFrame>(&text) {
                            handle_server_frame(frame, &manager, &status, &outgoing, &mut desired_binding, &mut current_binding, &mut event_task).await;
                        } else { warn!("ignoring malformed AHP frame"); }
                    }
                    Some(Ok(Message::Ping(_))) => { let _ = outgoing.send(ClientFrame::Pong).await; }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => connected = false,
                    _ => {}
                },
                _ = cancel.cancelled() => { connected = false; }
            }
        }
        if let Some(task) = event_task.take() {
            task.abort();
        }
        writer.abort();
        if current_binding.is_some() {
            set_status(
                &status,
                AhpStatus::Reconnecting {
                    reason: "channel disconnected".into(),
                },
            );
        }
        if wait_or_cancel(&cancel, reconnect).await {
            return;
        }
    }
}

async fn connect(
    endpoint: &Url,
    token: Option<&str>,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    let mut request = endpoint.as_str().into_client_request()?;
    if let Some(token) = token {
        let value = format!("Bearer {token}");
        let value = value.parse().map_err(|_| {
            tokio_tungstenite::tungstenite::Error::Http(
                tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(400)
                    .body(Some(b"invalid AHP token".to_vec()))
                    .expect("valid response"),
            )
        })?;
        request.headers_mut().insert("authorization", value);
    }
    connect_async(request).await
}

async fn handle_server_frame(
    frame: ServerFrame,
    manager: &Arc<SessionManager>,
    status: &Arc<RwLock<AhpStatus>>,
    outgoing: &mpsc::Sender<ClientFrame>,
    desired_binding: &mut Option<SessionId>,
    current_binding: &mut Option<SessionId>,
    event_task: &mut Option<JoinHandle<()>>,
) {
    match frame {
        ServerFrame::Hello {
            version,
            channel_id,
            qr_code,
        } => {
            if version != AHP_VERSION {
                set_status(
                    status,
                    AhpStatus::Failed {
                        reason: format!("unsupported AHP version {version}"),
                    },
                );
            } else if let Some(qr_code) = qr_code {
                set_status(status, AhpStatus::AwaitingQr { qr_code });
            } else {
                set_status(status, AhpStatus::Ready { channel_id });
            }
        }
        ServerFrame::UserMessage { text } => {
            let Some(session_id) = current_binding.clone() else {
                return;
            };
            if let Err(error) = manager
                .send_prompt_from(&session_id, vec![PromptBlock::text(text)], Some("ahp"))
                .await
            {
                let _ = outgoing
                    .send(ClientFrame::Notice {
                        text: error.to_string(),
                    })
                    .await;
            }
        }
        ServerFrame::Bound { session_id } => {
            if desired_binding.as_ref() == Some(&session_id) {
                set_status(status, AhpStatus::Bound { session_id });
            }
        }
        ServerFrame::Unbound => {
            *current_binding = None;
            *desired_binding = None;
            if let Some(task) = event_task.take() {
                task.abort();
            }
            set_status(status, AhpStatus::Ready { channel_id: None });
        }
        ServerFrame::Error { message } => {
            set_status(status, AhpStatus::Failed { reason: message });
        }
        ServerFrame::Ping => {
            let _ = outgoing.send(ClientFrame::Pong).await;
        }
    }
}

async fn forward_events(
    mut subscription: gateway_core::bus::EventSubscription,
    outgoing: mpsc::Sender<ClientFrame>,
) {
    let mut answer = String::new();
    while let Ok(event) = subscription.recv().await {
        match event.event_type.as_str() {
            "user_message"
                if event.payload.get("source").and_then(|value| value.as_str()) != Some("ahp") =>
            {
                let text = prompt_text(&event);
                if !text.is_empty() {
                    let _ = outgoing
                        .send(ClientFrame::UserMessage {
                            text: format!("[acp-gw User] {text}"),
                        })
                        .await;
                }
            }
            "agent_message_chunk" => answer.push_str(&content_text(&event)),
            "session_completed" => {
                if !answer.is_empty() {
                    let text = std::mem::take(&mut answer);
                    let _ = outgoing.send(ClientFrame::AgentMessage { text }).await;
                }
            }
            "session_failed" => {
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent failed");
                let _ = outgoing
                    .send(ClientFrame::Notice {
                        text: reason.to_owned(),
                    })
                    .await;
                answer.clear();
            }
            _ => {}
        }
    }
}

fn prompt_text(event: &AgentEvent) -> String {
    event
        .payload
        .get("content")
        .and_then(|content| {
            content
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .or_else(|| content.as_str().map(ToOwned::to_owned))
        })
        .unwrap_or_default()
}

fn content_text(event: &AgentEvent) -> String {
    event
        .payload
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

async fn send<S>(sink: &mut S, frame: ClientFrame) -> Result<(), ()>
where
    S: futures::Sink<Message> + Unpin,
{
    let text = serde_json::to_string(&frame).map_err(|_| ())?;
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

fn set_status(status: &Arc<RwLock<AhpStatus>>, value: AhpStatus) {
    *status.write().expect("AHP status lock") = value;
}

async fn wait_or_cancel(cancel: &CancellationToken, duration: Duration) -> bool {
    tokio::select! { _ = cancel.cancelled() => true, _ = sleep(duration) => false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::ids::EventId;

    fn event(event_type: &str, payload: serde_json::Value) -> AgentEvent {
        AgentEvent {
            id: EventId::new("evt_1"),
            session_id: SessionId::new("sess_1"),
            seq: 1,
            timestamp: chrono::Utc::now(),
            event_type: event_type.to_owned(),
            payload,
        }
    }

    #[test]
    fn prompt_text_flattens_acp_prompt_blocks() {
        let event = event(
            "user_message",
            serde_json::json!({ "content": [{ "text": "one" }, { "text": "two" }] }),
        );
        assert_eq!(prompt_text(&event), "onetwo");
    }

    #[test]
    fn agent_content_reads_text_chunks_only() {
        let event = event(
            "agent_message_chunk",
            serde_json::json!({ "content": { "text": "answer" } }),
        );
        assert_eq!(content_text(&event), "answer");
    }
}
