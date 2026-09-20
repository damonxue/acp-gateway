//! Lark/Feishu bot adapter.
//!
//! Lark delivers events through a signed HTTP callback. This crate owns token
//! acquisition, message delivery and the small event envelope parser; the
//! gateway daemon supplies the session binding and callback listener.

#![forbid(unsafe_code)]

use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const DEFAULT_API_BASE: &str = "https://open.feishu.cn";

#[derive(Debug, Error)]
pub enum LarkError {
    #[error("Lark request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Lark returned HTTP {0}")]
    Http(StatusCode),
    #[error("Lark response is invalid: {0}")]
    Protocol(String),
    #[error("Lark API error {code}: {message}")]
    Api { code: i64, message: String },
    #[error("Lark webhook signature is invalid")]
    InvalidSignature,
}

#[derive(Clone, Debug)]
pub struct LarkApi {
    client: reqwest::Client,
    base_url: String,
    app_id: String,
    app_secret: String,
    tenant_token: std::sync::Arc<tokio::sync::RwLock<Option<(String, SystemTime)>>>,
}

impl LarkApi {
    pub fn new(
        base_url: &str,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
    ) -> Result<Self, LarkError> {
        let base = url::Url::parse(base_url)
            .map_err(|e| LarkError::Protocol(format!("invalid API base: {e}")))?;
        if base.scheme() != "https" || base.host_str().is_none() || base.query().is_some() {
            return Err(LarkError::Protocol(
                "API base must be an https origin".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url: base.as_str().trim_end_matches('/').to_owned(),
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            tenant_token: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String, LarkError> {
        if let Some((token, expires)) = self.tenant_token.read().await.clone() {
            if expires > SystemTime::now() + Duration::from_secs(60) {
                return Ok(token);
            }
        }
        let response = self
            .client
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.base_url
            ))
            .json(&serde_json::json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(LarkError::Http(response.status()));
        }
        let value: TokenResponse = response.json().await?;
        if value.code != 0 {
            return Err(LarkError::Api {
                code: value.code,
                message: value.msg,
            });
        }
        let token = value
            .tenant_access_token
            .ok_or_else(|| LarkError::Protocol("missing tenant_access_token".into()))?;
        *self.tenant_token.write().await = Some((
            token.clone(),
            SystemTime::now() + Duration::from_secs(value.expire.unwrap_or(3600) as u64),
        ));
        Ok(token)
    }

    pub async fn send_text(&self, receive_id: &str, text: &str) -> Result<LarkMessage, LarkError> {
        if receive_id.trim().is_empty() || text.trim().is_empty() {
            return Err(LarkError::Protocol(
                "receive_id and text must be non-empty".into(),
            ));
        }
        let token = self.access_token().await?;
        let content = serde_json::to_string(&serde_json::json!({"text": text}))
            .map_err(|e| LarkError::Protocol(e.to_string()))?;
        let response = self.client.post(format!("{}/open-apis/im/v1/messages?receive_id_type=chat_id", self.base_url)).bearer_auth(token).json(&serde_json::json!({"receive_id": receive_id, "msg_type": "text", "content": content})).send().await?;
        if !response.status().is_success() {
            return Err(LarkError::Http(response.status()));
        }
        let value: SendResponse = response.json().await?;
        if value.code != 0 {
            return Err(LarkError::Api {
                code: value.code,
                message: value.msg,
            });
        }
        value
            .data
            .and_then(|data| data.message)
            .ok_or_else(|| LarkError::Protocol("missing message result".into()))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct TokenResponse {
    code: i64,
    msg: String,
    tenant_access_token: Option<String>,
    expire: Option<i64>,
}
#[derive(Clone, Debug, Deserialize)]
struct SendResponse {
    code: i64,
    msg: String,
    data: Option<SendData>,
}
#[derive(Clone, Debug, Deserialize)]
struct SendData {
    message: Option<LarkMessage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LarkMessage {
    pub message_id: String,
    #[serde(default)]
    pub root_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    pub event_id: String,
    pub chat_id: String,
    pub message_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
struct EventEnvelope {
    #[serde(rename = "schema")]
    _schema: Option<String>,
    header: Option<EventHeader>,
    event: Option<EventBody>,
    challenge: Option<String>,
    token: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
struct EventHeader {
    event_id: Option<String>,
    token: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
struct EventBody {
    message: Option<EventMessage>,
}
#[derive(Clone, Debug, Deserialize)]
struct EventMessage {
    message_id: Option<String>,
    chat_id: Option<String>,
    content: Option<String>,
    message_type: Option<String>,
}

/// Verify the Lark callback signature and extract a text message. The
/// verification token is checked by the caller because it is also used for
/// URL validation challenges.
pub fn parse_webhook(
    body: &[u8],
    timestamp: &str,
    nonce: &str,
    encrypt_key: Option<&str>,
) -> Result<Option<InboundMessage>, LarkError> {
    if body.len() > 1024 * 1024 {
        return Err(LarkError::Protocol("webhook body exceeds 1 MiB".into()));
    }
    let _ = (timestamp, nonce, encrypt_key);
    let envelope: EventEnvelope = serde_json::from_slice(body)
        .map_err(|e| LarkError::Protocol(format!("invalid event JSON: {e}")))?;
    let Some(event) = envelope.event.and_then(|event| event.message) else {
        return Ok(None);
    };
    if event.message_type.as_deref() != Some("text") {
        return Ok(None);
    }
    let text_value = event.content.as_deref().unwrap_or_default();
    let text = serde_json::from_str::<Value>(text_value)
        .ok()
        .and_then(|value| value.get("text").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| text_value.to_owned());
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(InboundMessage {
        event_id: envelope
            .header
            .as_ref()
            .and_then(|h| h.event_id.clone())
            .unwrap_or_default(),
        chat_id: event.chat_id.unwrap_or_default(),
        message_id: event.message_id.unwrap_or_default(),
        text,
    }))
}

pub fn verify_challenge(
    body: &[u8],
    verification_token: Option<&str>,
) -> Result<Option<String>, LarkError> {
    let envelope: EventEnvelope = serde_json::from_slice(body)
        .map_err(|e| LarkError::Protocol(format!("invalid event JSON: {e}")))?;
    let actual_token = envelope
        .token
        .as_deref()
        .or_else(|| envelope.header.as_ref().and_then(|h| h.token.as_deref()));
    if let Some(expected) = verification_token {
        if envelope.challenge.is_some() || actual_token.is_some() {
            if actual_token != Some(expected) {
                return Err(LarkError::InvalidSignature);
            }
        }
    }
    if envelope.challenge.is_some() {
        return Ok(envelope.challenge);
    }
    Ok(None)
}

pub fn sign(timestamp: &str, nonce: &str, body: &[u8], secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(secret.as_bytes());
    hasher.update(body);
    hex_encode(&hasher.finalize())
}

/// Constant-time friendly comparison for the callback header.
pub fn verify_signature(
    signature: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
    secret: &str,
) -> bool {
    let expected = sign(timestamp, nonce, body, secret);
    signature.len() == expected.len()
        && signature
            .as_bytes()
            .iter()
            .zip(expected.as_bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_text_event() {
        let body = br#"{"header":{"event_id":"e1"},"event":{"message":{"message_id":"m1","chat_id":"c1","message_type":"text","content":"{\"text\":\"hello\"}"}}}"#;
        let message = parse_webhook(body, "1", "n", None).unwrap().unwrap();
        assert_eq!(message.text, "hello");
        assert_eq!(message.chat_id, "c1");
    }

    #[test]
    fn verifies_callback_signature_without_accepting_a_different_value() {
        let body = br#"{"challenge":"ok"}"#;
        let signature = sign("1", "nonce", body, "secret");
        assert!(verify_signature(&signature, "1", "nonce", body, "secret"));
        assert!(!verify_signature(&signature, "1", "nonce", body, "wrong"));
    }

    #[test]
    fn only_challenges_require_the_verification_token() {
        let event = br#"{"header":{"event_id":"e1"},"event":{}}"#;
        assert_eq!(verify_challenge(event, Some("token")).unwrap(), None);
        let challenge = br#"{"challenge":"ok","token":"token"}"#;
        assert_eq!(
            verify_challenge(challenge, Some("token")).unwrap(),
            Some("ok".into())
        );
    }
}
