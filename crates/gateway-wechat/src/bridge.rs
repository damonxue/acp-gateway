use std::sync::Arc;

use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use gateway_core::agent::PromptBlock;
use gateway_core::event::AgentEvent;
use gateway_core::session::SessionStatus;
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
    binding: Arc<RwLock<Binding>>,
    credentials: Credentials,
    outbound: OutboundSender<A, J>,
    context_token: Arc<RwLock<Option<String>>>,
    cancel: CancellationToken,
    rebind_notify: Arc<Notify>,
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
            binding: Arc::new(RwLock::new(binding)),
            credentials,
            outbound: OutboundSender::new(api, Arc::clone(&journal))
                .with_limits(max_text_bytes, max_chunk_bytes),
            context_token: Arc::new(RwLock::new(None)),
            cancel,
            rebind_notify: Arc::new(Notify::new()),
        }
    }

    /// Switch the active Gateway session without restarting the WeChat
    /// poller or losing the conversation context and outbox journal.
    pub async fn rebind(&self, session_id: SessionId) {
        let mut binding = self.binding.write().await;
        if binding.session_id == session_id {
            return;
        }
        binding.session_id = session_id;
        drop(binding);
        self.rebind_notify.notify_waiters();
    }

    /// Deliver an accepted inbound message into the selected session. The
    /// source marker identifies the originating channel while the event stream
    /// still mirrors the user message to both sides.
    pub async fn accept_inbound(
        &self,
        message: &crate::inbound::InboundMessage,
    ) -> Result<(), BridgeError> {
        let binding = self.binding.read().await.clone();
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
                        &binding.binding_id,
                        &message.message_id,
                        OutboundRole::Agent,
                        &reply,
                        context.as_deref(),
                    )
                    .await?;
                return Ok(());
            }
        }
        let session_id = self.wait_for_live_session().await?;
        self.manager
            .send_prompt_from(
                &session_id,
                vec![PromptBlock::text(message.text.clone())],
                Some("wechat"),
            )
            .await?;
        Ok(())
    }

    async fn wait_for_live_session(&self) -> Result<SessionId, BridgeError> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(BridgeError::Gateway(
                    gateway_core::GatewayError::AgentUnavailable(
                        "WeChat bridge cancelled".to_owned(),
                    ),
                ));
            }
            let session_id = self.binding.read().await.session_id.clone();
            match self.manager.get_snapshot(&session_id).await {
                Ok(snapshot) if snapshot.connected && snapshot.session.status.is_drivable() => {
                    return Ok(snapshot.session.id);
                }
                Ok(snapshot)
                    if matches!(
                        snapshot.session.status,
                        SessionStatus::Completed | SessionStatus::Failed
                    ) =>
                {
                    return Err(BridgeError::Gateway(
                        gateway_core::GatewayError::InvalidSessionState {
                            session: session_id.clone(),
                            action: "prompt",
                            status: snapshot.session.status.as_str(),
                        },
                    ));
                }
                Ok(_) | Err(gateway_core::GatewayError::SessionNotFound(_)) => {}
                Err(error) => return Err(BridgeError::Gateway(error)),
            }
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Err(BridgeError::Gateway(
                        gateway_core::GatewayError::AgentUnavailable(
                            "WeChat bridge cancelled".to_owned(),
                        ),
                    ));
                }
                _ = self.rebind_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    }

    async fn format_current_session(&self) -> Result<String, BridgeError> {
        let binding = self.binding.read().await.clone();
        let snapshot = self.manager.get_snapshot(&binding.session_id).await?;
        Ok(format_session_line(&snapshot, true))
    }

    async fn format_sessions(&self) -> Result<String, BridgeError> {
        let sessions = self.manager.list_sessions().await?;
        let binding = self.binding.read().await.clone();
        if sessions.is_empty() {
            return Ok("Gateway 当前没有 session。".to_owned());
        }
        let mut lines = vec![format!("Gateway session 列表（{}）：", sessions.len())];
        for session in sessions {
            let snapshot = self.manager.get_snapshot(&session.id).await?;
            lines.push(format_session_line(
                &snapshot,
                session.id == binding.session_id,
            ));
        }
        Ok(lines.join("\n"))
    }

    /// Subscribe to the manager's durable event stream and forward eligible
    /// user messages and completed agent text. Thoughts, tools, permissions and
    /// partial progress are intentionally excluded.
    pub async fn run(self: Arc<Self>) -> Result<(), BridgeError> {
        loop {
            let session_id = self.binding.read().await.session_id.clone();
            let mut subscription = match self.manager.subscribe(&session_id) {
                Ok(subscription) => subscription,
                Err(gateway_core::GatewayError::SessionNotFound(_)) => {
                    tokio::select! {
                        _ = self.cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => continue,
                    }
                }
                Err(error) => return Err(BridgeError::Gateway(error)),
            };
            let mut answer = String::new();
            let event = tokio::select! {
                _ = self.cancel.cancelled() => return Ok(()),
                _ = self.rebind_notify.notified() => continue,
                event = subscription.recv() => match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(error) => return Err(BridgeError::Journal(JournalError::Operation(error.to_string()))),
                },
            };
            self.handle_event(&event, &mut answer).await?;
            loop {
                let event = tokio::select! {
                    _ = self.cancel.cancelled() => return Ok(()),
                    _ = self.rebind_notify.notified() => break,
                    event = subscription.recv() => match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(error) => return Err(BridgeError::Journal(JournalError::Operation(error.to_string()))),
                    },
                };
                self.handle_event(&event, &mut answer).await?;
            }
        }
    }

    async fn handle_event(
        &self,
        event: &AgentEvent,
        answer: &mut String,
    ) -> Result<(), BridgeError> {
        let binding = self.binding.read().await.clone();
        match event.event_type.as_str() {
            "user_message" => {
                let text = content_text(event.payload.get("content"));
                if !text.is_empty() {
                    let context = self.context_token.read().await.clone();
                    let _ = self
                        .outbound
                        .send_text(
                            &self.credentials,
                            &binding.binding_id,
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
                            &binding.binding_id,
                            &event.id.to_string(),
                            OutboundRole::Agent,
                            &text,
                            context.as_deref(),
                        )
                        .await?;
                }
            }
            // Codex emits its terminal turn state as an extension metadata
            // update. Some IDE-originated turns do not produce a separate
            // prompt response before the bridge is refreshed, so use the
            // terminal state as an idempotent completion fallback. A later
            // `session_completed` sees an empty accumulator and sends nothing.
            "session_update" if is_idle_update(&event.payload) => {
                let text = std::mem::take(answer);
                if !text.trim().is_empty() {
                    let context = self.context_token.read().await.clone();
                    let _ = self
                        .outbound
                        .send_text(
                            &self.credentials,
                            &binding.binding_id,
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

fn is_idle_update(payload: &serde_json::Value) -> bool {
    payload
        .get("_meta")
        .and_then(|value| value.get("codex"))
        .and_then(|value| value.get("threadStatus"))
        .and_then(|value| value.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("idle")
}

#[cfg(test)]
mod tests {
    use super::is_idle_update;

    #[test]
    fn recognizes_codex_idle_turn_metadata() {
        assert!(is_idle_update(&serde_json::json!({
            "_meta": {"codex": {"threadStatus": {"type": "idle"}}}
        })));
        assert!(!is_idle_update(&serde_json::json!({
            "_meta": {"codex": {"threadStatus": {"type": "active"}}}
        })));
    }
}
