use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::types::{Credentials, MAX_TEXT_BYTES};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    pub id: String,
    pub message_id: String,
    pub text: String,
    pub context_token: String,
}

pub fn parse_inbound(
    value: &Value,
    credentials: &Credentials,
) -> Result<InboundMessage, &'static str> {
    parse_inbound_with_limit(value, credentials, MAX_TEXT_BYTES)
}

pub fn parse_inbound_with_limit(
    value: &Value,
    credentials: &Credentials,
    max_text_bytes: usize,
) -> Result<InboundMessage, &'static str> {
    let object = value.as_object().ok_or("malformed message")?;
    if object.get("from_user_id").and_then(Value::as_str) != Some(credentials.owner_id.as_str()) {
        return Err("unauthorized sender");
    }
    if object.get("to_user_id").and_then(Value::as_str) != Some(credentials.bot_id.as_str()) {
        return Err("different recipient");
    }
    if object
        .get("group_id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        return Err("group messages are unsupported");
    }
    if object.get("message_type").and_then(Value::as_i64) != Some(1)
        || object.get("message_state").and_then(Value::as_i64) != Some(2)
    {
        return Err("non-text or unfinished message");
    }
    if object
        .get("delete_time_ms")
        .and_then(Value::as_i64)
        .is_some_and(|n| n != 0)
    {
        return Err("deleted message");
    }
    let message_id = match object.get("message_id") {
        Some(Value::String(s)) if valid_id(s) => s.clone(),
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        _ => return Err("invalid message id"),
    };
    let context_token = object
        .get("context_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 8192)
        .ok_or("missing reply context")?;
    let items = object
        .get("item_list")
        .and_then(Value::as_array)
        .ok_or("invalid content list")?;
    if items.is_empty() || items.len() > 32 {
        return Err("invalid content list");
    }
    let mut parts = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_i64) != Some(1) {
            return Err("non-text or oversized content");
        }
        let text = item
            .get("text_item")
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= max_text_bytes)
            .ok_or("non-text or oversized content")?;
        parts.push(text);
    }
    let text = parts.join("\n");
    if text.trim().is_empty()
        || text.len() > max_text_bytes
        || text.contains(&credentials.token)
        || text.contains(context_token)
    {
        return Err("empty, oversized, or credential-bearing text");
    }
    let mut hasher = Sha256::new();
    hasher.update(credentials.bot_id.as_bytes());
    hasher.update([0]);
    hasher.update(credentials.owner_id.as_bytes());
    hasher.update([0]);
    hasher.update(message_id.as_bytes());
    let id = format!("{:x}", hasher.finalize());
    Ok(InboundMessage {
        id,
        message_id,
        text,
        context_token: context_token.to_owned(),
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            bot_id: "bot".into(),
            owner_id: "owner".into(),
            token: "secret-token".into(),
            base_url: "https://ilinkai.weixin.qq.com".into(),
        }
    }

    fn message() -> Value {
        serde_json::json!({
            "from_user_id": "owner",
            "to_user_id": "bot",
            "message_id": 18446744073709551615u64.to_string(),
            "message_type": 1,
            "message_state": 2,
            "context_token": "ctx",
            "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
        })
    }

    #[test]
    fn accepts_owner_private_text_and_preserves_large_id() {
        let parsed = parse_inbound(&message(), &credentials()).unwrap();
        assert_eq!(parsed.message_id, "18446744073709551615");
        assert_eq!(parsed.text, "hello");
    }

    #[test]
    fn rejects_group_or_other_sender() {
        let mut value = message();
        value["group_id"] = Value::String("group".into());
        assert!(parse_inbound(&value, &credentials()).is_err());
        value["group_id"] = Value::Null;
        value["from_user_id"] = Value::String("other".into());
        assert!(parse_inbound(&value, &credentials()).is_err());
    }
}
