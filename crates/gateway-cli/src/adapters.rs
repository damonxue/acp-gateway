//! Chat adapter composition for the daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post};
use gateway_config::GatewayConfig;
use gateway_core::agent::PromptBlock;
use gateway_core::manager::SessionManager;
use gateway_lark::{LarkApi, LarkError};
use gateway_telegram::{TelegramApi, TelegramError, UpdateHandler};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Debug)]
pub(crate) struct AdapterRuntime {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl AdapterRuntime {
    pub(crate) fn start(config: &GatewayConfig, manager: Arc<SessionManager>) -> Result<Self> {
        let mut runtime = Self {
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
        };
        if let Some(section) = config.telegram.as_ref().filter(|section| section.enabled) {
            let binding = section
                .binding
                .as_ref()
                .context("telegram.binding is required when Telegram is enabled")?;
            let api = Arc::new(TelegramApi::with_base(
                &section.api_base,
                section.bot_token.expose(),
            )?);
            let bridge = Arc::new(TelegramBridge {
                manager: Arc::clone(&manager),
                api: Arc::clone(&api),
                session_id: binding.session_id.clone(),
                chat_id: binding.chat_id.clone(),
                max_chunk_bytes: section.max_chunk_bytes,
                cancel: runtime.cancel.clone(),
            });
            let poll_timeout = section.poll_timeout;
            let poll_bridge = Arc::clone(&bridge);
            runtime.tasks.push(tokio::spawn(async move {
                if let Err(error) = gateway_telegram::poll(
                    api.as_ref(),
                    poll_bridge.as_ref(),
                    poll_bridge.cancel.clone(),
                    poll_timeout,
                )
                .await
                {
                    warn!(%error, "Telegram adapter stopped");
                }
            }));
            runtime.tasks.push(spawn_event_forwarder(
                Arc::clone(&manager),
                bridge.session_id.clone(),
                runtime.cancel.clone(),
                {
                    let bridge = Arc::clone(&bridge);
                    move |text| {
                        let bridge = Arc::clone(&bridge);
                        async move {
                            bridge
                                .send_text(&text)
                                .await
                                .map_err(|error| error.to_string())
                        }
                    }
                },
            ));
            info!(chat_id = %binding.chat_id, session_id = %binding.session_id, "Telegram adapter enabled");
        }
        if let Some(section) = config.lark.as_ref().filter(|section| section.enabled) {
            let binding = section
                .binding
                .as_ref()
                .context("lark.binding is required when Lark is enabled")?;
            let api = Arc::new(LarkApi::new(
                &section.api_base,
                section.app_id.clone(),
                section.app_secret.expose(),
            )?);
            let bridge = Arc::new(LarkBridge {
                manager: Arc::clone(&manager),
                api: Arc::clone(&api),
                session_id: binding.session_id.clone(),
                chat_id: binding.chat_id.clone(),
                verification_token: section
                    .verification_token
                    .as_ref()
                    .map(|s| s.expose().to_owned()),
                encrypt_key: section.encrypt_key.as_ref().map(|s| s.expose().to_owned()),
                cancel: runtime.cancel.clone(),
            });
            let listener = std::net::SocketAddr::from_str(&section.webhook_bind)
                .context("invalid lark.webhook_bind")?;
            let route_bridge = Arc::clone(&bridge);
            let shutdown_bridge = Arc::clone(&bridge);
            runtime.tasks.push(tokio::spawn(async move {
                let router = Router::new()
                    .route("/webhook", post(lark_webhook))
                    .with_state(route_bridge);
                match tokio::net::TcpListener::bind(listener).await {
                    Ok(listener) => {
                        if let Err(error) = axum::serve(listener, router)
                            .with_graceful_shutdown(async move {
                                shutdown_bridge.cancel.cancelled().await
                            })
                            .await
                        {
                            warn!(%error, "Lark webhook stopped");
                        }
                    }
                    Err(error) => warn!(%error, "cannot bind Lark webhook"),
                }
            }));
            runtime.tasks.push(spawn_event_forwarder(
                Arc::clone(&manager),
                bridge.session_id.clone(),
                runtime.cancel.clone(),
                {
                    let bridge = Arc::clone(&bridge);
                    move |text| {
                        let bridge = Arc::clone(&bridge);
                        async move {
                            bridge
                                .send_text(&text)
                                .await
                                .map_err(|error| error.to_string())
                        }
                    }
                },
            ));
            info!(chat_id = %binding.chat_id, session_id = %binding.session_id, bind = %section.webhook_bind, "Lark adapter enabled");
        }
        Ok(runtime)
    }

    pub(crate) fn shutdown(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for AdapterRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct TelegramBridge {
    manager: Arc<SessionManager>,
    api: Arc<TelegramApi>,
    session_id: String,
    chat_id: String,
    max_chunk_bytes: usize,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl UpdateHandler for TelegramBridge {
    async fn handle(&self, message: gateway_telegram::InboundMessage) -> Result<(), TelegramError> {
        if message.chat_id != self.chat_id {
            return Ok(());
        }
        if message.text == "/help" {
            self.send_text("/help 查看帮助\n/session 查看绑定的 ACP session")
                .await
                .map_err(|e| TelegramError::Protocol(e.to_string()))?;
            return Ok(());
        }
        self.manager
            .send_prompt_from(
                &gateway_core::SessionId::new(self.session_id.clone()),
                vec![PromptBlock::text(message.text)],
                Some("telegram"),
            )
            .await
            .map_err(|e| TelegramError::Protocol(e.to_string()))?;
        Ok(())
    }
}
impl TelegramBridge {
    async fn send_text(&self, text: &str) -> Result<(), TelegramError> {
        for chunk in gateway_telegram::split_text(text, self.max_chunk_bytes)? {
            self.api.send_text(&self.chat_id, &chunk).await?;
        }
        Ok(())
    }
}

struct LarkBridge {
    manager: Arc<SessionManager>,
    api: Arc<LarkApi>,
    session_id: String,
    chat_id: String,
    verification_token: Option<String>,
    encrypt_key: Option<String>,
    cancel: CancellationToken,
}
impl LarkBridge {
    async fn accept(&self, body: &[u8], headers: &HeaderMap) -> Result<Option<String>, LarkError> {
        if let Some(challenge) =
            gateway_lark::verify_challenge(body, self.verification_token.as_deref())?
        {
            return Ok(Some(challenge));
        }
        let timestamp = headers
            .get("x-lark-request-timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let nonce = headers
            .get("x-lark-request-nonce")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if let Some(key) = self.encrypt_key.as_deref() {
            let signature = headers
                .get("x-lark-signature")
                .and_then(|v| v.to_str().ok())
                .ok_or(LarkError::InvalidSignature)?;
            if !gateway_lark::verify_signature(signature, timestamp, nonce, body, key) {
                return Err(LarkError::InvalidSignature);
            }
        }
        if let Some(message) = gateway_lark::parse_webhook(body, timestamp, nonce, None)? {
            if message.chat_id == self.chat_id {
                self.manager
                    .send_prompt_from(
                        &gateway_core::SessionId::new(self.session_id.clone()),
                        vec![PromptBlock::text(message.text)],
                        Some("lark"),
                    )
                    .await
                    .map_err(|e| LarkError::Protocol(e.to_string()))?;
            }
        }
        Ok(None)
    }
    async fn send_text(&self, text: &str) -> Result<(), LarkError> {
        for chunk in split_text(text, 3500) {
            self.api.send_text(&self.chat_id, &chunk).await?;
        }
        Ok(())
    }
}

async fn lark_webhook(
    State(bridge): State<Arc<LarkBridge>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    match bridge.accept(&body, &headers).await {
        Ok(Some(challenge)) => {
            axum::Json(serde_json::json!({"challenge": challenge})).into_response()
        }
        Ok(None) => axum::Json(serde_json::json!({"code": 0})).into_response(),
        Err(error) => {
            warn!(%error, "Lark webhook rejected");
            (axum::http::StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
    }
}

fn spawn_event_forwarder<F, Fut>(
    manager: Arc<SessionManager>,
    session_id: String,
    cancel: CancellationToken,
    send: F,
) -> JoinHandle<()>
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    tokio::spawn(async move {
        let id = gateway_core::SessionId::new(session_id);
        let mut events = loop {
            match manager.subscribe(&id) {
                Ok(events) => break events,
                Err(_) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                    }
                }
            }
        };
        let mut answer = String::new();
        loop {
            let event = tokio::select! { _ = cancel.cancelled() => return, result = events.recv() => match result { Ok(event) => event, Err(_) => return } };
            match event.event_type.as_str() {
                "agent_message" | "agent_message_chunk" => {
                    answer.push_str(content_text(event.payload.get("content")).as_str())
                }
                "session_completed" | "session_failed" => {
                    let text = std::mem::take(&mut answer);
                    if !text.trim().is_empty() && event.event_type == "session_completed" {
                        if let Err(error) = send(text).await {
                            warn!(%error, "chat adapter could not send agent reply");
                        }
                    }
                }
                _ => {}
            }
        }
    })
}

fn content_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        None => String::new(),
    }
}
fn split_text(text: &str, max_bytes: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if !current.is_empty() && current.len() + ch.len_utf8() > max_bytes {
            result.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}
use std::str::FromStr;
