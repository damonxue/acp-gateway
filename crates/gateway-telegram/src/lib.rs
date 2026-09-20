//! Telegram Bot API adapter.
//!
//! The adapter follows Telegram's documented long-polling model. It keeps the
//! update offset in memory, acknowledges an update only after the caller has
//! accepted it, and never logs the bot token.

#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";
pub const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_CHUNK_BYTES: usize = 4096;

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("Telegram request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Telegram returned HTTP {0}")]
    Http(StatusCode),
    #[error("Telegram response is invalid: {0}")]
    Protocol(String),
    #[error("Telegram API error {code}: {description}")]
    Api { code: i64, description: String },
}

#[derive(Clone, Debug)]
pub struct TelegramApi {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl TelegramApi {
    pub fn new(token: impl Into<String>) -> Result<Self, TelegramError> {
        Self::with_base(DEFAULT_API_BASE, token)
    }

    pub fn with_base(base_url: &str, token: impl Into<String>) -> Result<Self, TelegramError> {
        let base = url::Url::parse(base_url)
            .map_err(|error| TelegramError::Protocol(format!("invalid API base: {error}")))?;
        if base.scheme() != "https" || base.host_str().is_none() || base.query().is_some() {
            return Err(TelegramError::Protocol(
                "API base must be an https origin".into(),
            ));
        }
        let token = token.into();
        if token.trim().is_empty() || token.len() > 512 {
            return Err(TelegramError::Protocol(
                "bot token is empty or oversized".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(65))
            .build()?;
        Ok(Self {
            client,
            base_url: base.as_str().trim_end_matches('/').to_owned(),
            token,
        })
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<T, TelegramError> {
        let url = format!("{}/bot{}/{}", self.base_url, self.token, method);
        let response = self.client.post(url).json(params).send().await?;
        if !response.status().is_success() {
            return Err(TelegramError::Http(response.status()));
        }
        let envelope: ApiResponse<T> = response.json().await?;
        if !envelope.ok {
            return Err(TelegramError::Api {
                code: envelope.error_code.unwrap_or_default(),
                description: envelope
                    .description
                    .unwrap_or_else(|| "unknown error".into()),
            });
        }
        envelope
            .result
            .ok_or_else(|| TelegramError::Protocol("missing result".into()))
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout: Duration,
        limit: u8,
    ) -> Result<Vec<TelegramUpdate>, TelegramError> {
        let params = GetUpdatesRequest {
            offset,
            timeout: timeout.as_secs().min(50),
            limit: limit.clamp(1, 100),
            allowed_updates: vec!["message".into()],
        };
        self.call("getUpdates", &params).await
    }

    pub async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
    ) -> Result<TelegramMessage, TelegramError> {
        if chat_id.trim().is_empty() || text.trim().is_empty() {
            return Err(TelegramError::Protocol(
                "chat_id and text must be non-empty".into(),
            ));
        }
        self.call(
            "sendMessage",
            &SendMessageRequest {
                chat_id,
                text,
                disable_web_page_preview: true,
            },
        )
        .await
    }

    pub async fn get_me(&self) -> Result<TelegramUser, TelegramError> {
        self.call("getMe", &()).await
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i64>,
    description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct GetUpdatesRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<i64>,
    timeout: u64,
    limit: u8,
    allowed_updates: Vec<&'a str>,
}

#[derive(Clone, Debug, Serialize)]
struct SendMessageRequest<'a> {
    chat_id: &'a str,
    text: &'a str,
    disable_web_page_preview: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: TelegramChat,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramUser {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    pub update_id: i64,
    pub message_id: i64,
    pub chat_id: String,
    pub text: String,
}

impl TelegramUpdate {
    #[must_use]
    pub fn into_text(self) -> Option<InboundMessage> {
        let message = self.message?;
        let text = message.text?.trim().to_owned();
        (!text.is_empty()).then_some(InboundMessage {
            update_id: self.update_id,
            message_id: message.message_id,
            chat_id: message.chat.id.to_string(),
            text,
        })
    }
}

pub fn split_text(text: &str, max_bytes: usize) -> Result<Vec<String>, TelegramError> {
    if text.is_empty() || max_bytes < 4 || text.len() > 1024 * 1024 {
        return Err(TelegramError::Protocol(
            "text is empty, oversized, or max_bytes is too small".into(),
        ));
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if !current.is_empty() && current.len() + ch.len_utf8() > max_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

#[async_trait]
pub trait UpdateHandler: Send + Sync {
    async fn handle(&self, message: InboundMessage) -> Result<(), TelegramError>;
}

pub async fn poll<H: UpdateHandler + ?Sized>(
    api: &TelegramApi,
    handler: &H,
    cancel: CancellationToken,
    poll_timeout: Duration,
) -> Result<(), TelegramError> {
    let mut offset = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = api.get_updates(offset, poll_timeout, 100) => {
                for update in result? {
                    let update_id = update.update_id;
                    if let Some(message) = update.into_text() {
                        handler.handle(message).await?;
                    }
                    // The next request acknowledges this update. Advance only
                    // after the handler accepted the text, so a transient
                    // session/transport error can be retried safely.
                    offset = Some(update_id + 1);
                }
            }
        }
    }
}

pub fn parse_webhook(value: Value) -> Option<InboundMessage> {
    serde_json::from_value::<TelegramUpdate>(value)
        .ok()?
        .into_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode() {
        let chunks = split_text(&"😀".repeat(100), 17).unwrap();
        assert_eq!(chunks.concat(), "😀".repeat(100));
        assert!(chunks.iter().all(|chunk| chunk.len() <= 17));
    }
    #[test]
    fn ignores_non_text_updates() {
        let update = TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 2,
                chat: TelegramChat {
                    id: 3,
                    r#type: None,
                },
                from: None,
                text: None,
            }),
        };
        assert!(update.into_text().is_none());
    }
}
