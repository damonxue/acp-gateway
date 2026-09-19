use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use gateway_core::agent::PromptBlock;
use gateway_core::event::AgentEvent;
use gateway_core::{SessionId, SessionManager};

use crate::api::WeixinApi;
use crate::journal::{Journal, JournalError};
use crate::outbound::{OutboundError, OutboundRole, OutboundSender};
use crate::types::Credentials;

#[derive(Clone, Debug)]
pub struct Binding {
    pub binding_id: String,
    pub session_id: SessionId,
    pub chat_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("session manager error: {0}")]
    Gateway(#[from] gateway_core::GatewayError),
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// Bridges one explicit Gateway session to one WeChat owner conversation.
/// Binding is intentionally one-to-one; no implicit "active session" exists.
#[derive(Debug)]
pub struct WechatBridge<A, J> {
    manager: Arc<SessionManager>,
    binding: Binding,
    credentials: Credentials,
    outbound: OutboundSender<A, J>,
    context_token: Arc<RwLock<Option<String>>>,
    cancel: CancellationToken,
}

impl<A, J> WechatBridge<A, J>
where
    A: WeixinApi + 'static,
    J: Journal + 'static,
{
    pub fn new(
        manager: Arc<SessionManager>,
        binding: Binding,
        credentials: Credentials,
        api: Arc<A>,
        journal: Arc<J>,
        cancel: CancellationToken,
    ) -> Self {
        Self::new_with_limits(
            manager,
            binding,
            credentials,
            api,
            journal,
            cancel,
            crate::types::MAX_TEXT_BYTES,
            crate::types::MAX_CHUNK_BYTES,
        )
    }

    pub fn new_with_limits(
        manager: Arc<SessionManager>,
        binding: Binding,
        credentials: Credentials,
        api: Arc<A>,
        journal: Arc<J>,
        cancel: CancellationToken,
        max_text_bytes: usize,
        max_chunk_bytes: usize,
    ) -> Self {
        Self {
            manager,
            binding,
            credentials,
            outbound: OutboundSender::new(api, Arc::clone(&journal))
                .with_limits(max_text_bytes, max_chunk_bytes),
            context_token: Arc::new(RwLock::new(None)),
            cancel,
        }
    }

    /// Deliver an accepted inbound message into the selected session. The
    /// source marker prevents this event from being echoed back to WeChat.
    pub async fn accept_inbound(
        &self,
        message: &crate::inbound::InboundMessage,
    ) -> Result<(), BridgeError> {
        *self.context_token.write().await = Some(message.context_token.clone());
        if let Some(command) = message.text.strip_prefix('/') {
            let reply = match command.trim() {
                "sessions" => Some(self.format_sessions().await?),
                "session" => Some(self.format_current_session().await?),
                "help" => Some(
                    "可用命令：\n/sessions 查看 Gateway session 列表\n/session 查看当前微信绑定\n/help 查看帮助"
                        .to_owned(),
                ),
                _ => None,
            };
            if let Some(reply) = reply {
                let context = self.context_token.read().await.clone();
                self.outbound
                    .send_text(
                        &self.credentials,
                        &self.binding.binding_id,
                        &message.message_id,
                        OutboundRole::Agent,
                        &reply,
                        context.as_deref(),
                    )
                    .await?;
                return Ok(());
            }
        }
        self.manager
            .send_prompt_from(
                &self.binding.session_id,
                vec![PromptBlock::text(message.text.clone())],
                Some("wechat"),
            )
            .await?;
        Ok(())
    }

    async fn format_current_session(&self) -> Result<String, BridgeError> {
        let snapshot = self.manager.get_snapshot(&self.binding.session_id).await?;
        Ok(format_session_line(&snapshot, true))
    }

    async fn format_sessions(&self) -> Result<String, BridgeError> {
        let sessions = self.manager.list_sessions().await?;
        if sessions.is_empty() {
            return Ok("Gateway 当前没有 session。".to_owned());
        }
        let mut lines = vec![format!("Gateway session 列表（{}）：", sessions.len())];
        for session in sessions {
            let snapshot = self.manager.get_snapshot(&session.id).await?;
            lines.push(format_session_line(
                &snapshot,
                session.id == self.binding.session_id,
            ));
        }
        Ok(lines.join("\n"))
    }

    /// Subscribe to the manager's durable event stream and forward eligible
    /// user messages and completed agent text. Thoughts, tools, permissions and
    /// partial progress are intentionally excluded.
    pub async fn run(self: Arc<Self>) -> Result<(), BridgeError> {
        let mut subscription = self.manager.subscribe(&self.binding.session_id)?;
        let mut answer = String::new();
        loop {
            let event = tokio::select! {
                _ = self.cancel.cancelled() => return Ok(()),
                event = subscription.recv() => event.map_err(|error| BridgeError::Journal(JournalError::Operation(error.to_string())))?,
            };
            self.handle_event(&event, &mut answer).await?;
        }
    }

    async fn handle_event(
        &self,
        event: &AgentEvent,
        answer: &mut String,
    ) -> Result<(), BridgeError> {
        match event.event_type.as_str() {
            "user_message" => {
                if event.payload.get("source").and_then(|value| value.as_str()) == Some("wechat") {
                    return Ok(());
                }
                let text = content_text(event.payload.get("content"));
                if !text.is_empty() {
                    let context = self.context_token.read().await.clone();
                    let _ = self
                        .outbound
                        .send_text(
                            &self.credentials,
                            &self.binding.binding_id,
                            &event.id.to_string(),
                            OutboundRole::VsCodeUser,
                            &text,
                            context.as_deref(),
                        )
                        .await?;
                }
            }
            "agent_message" => {
                if answer.is_empty() {
                    answer.push_str(&content_text(event.payload.get("content")));
                }
            }
            "agent_message_chunk" => {
                answer.push_str(&content_text(event.payload.get("content")));
            }
            "session_completed" => {
                let text = std::mem::take(answer);
                if !text.trim().is_empty() {
                    let context = self.context_token.read().await.clone();
                    let _ = self
                        .outbound
                        .send_text(
                            &self.credentials,
                            &self.binding.binding_id,
                            &event.id.to_string(),
                            OutboundRole::Agent,
                            &text,
                            context.as_deref(),
                        )
                        .await?;
                }
            }
            "session_failed" => answer.clear(),
            _ => {}
        }
        Ok(())
    }
}

fn format_session_line(
    snapshot: &gateway_core::manager::SessionSnapshot,
    selected: bool,
) -> String {
    let marker = if selected { "*" } else { "-" };
    let connected = if snapshot.connected {
        "connected"
    } else {
        "disconnected"
    };
    format!(
        "{marker} {} [{} / {} / {}]\n  {}",
        snapshot.session.id,
        snapshot.session.origin.as_str(),
        snapshot.session.status.as_str(),
        connected,
        snapshot.session.workspace.display()
    )
}

fn content_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Object(object)) => object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .or_else(|| object.get("content").and_then(serde_json::Value::as_str))
            .unwrap_or_default()
            .to_owned(),
        Some(serde_json::Value::Array(items)) => {
            items.iter().map(|item| content_text(Some(item))).collect()
        }
        _ => String::new(),
    }
}
