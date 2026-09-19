use base64::Engine;
use serde::{Deserialize, Serialize};

pub const CLIENT_VERSION: &str = "0.1.1";
pub const DEFAULT_BASE: &str = "https://ilinkai.weixin.qq.com";
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_CHUNK_BYTES: usize = 3500;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub bot_id: String,
    pub owner_id: String,
    pub token: String,
    pub base_url: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("bot_id", &self.bot_id)
            .field("owner_id", &self.owner_id)
            .field("token", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrChallenge {
    pub code: String,
    pub image_content: String,
    pub base_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginPoll {
    Wait,
    Scanned,
    NeedVerifyCode,
    Redirect { host: String },
    Confirmed(Credentials),
    Expired,
    VerifyCodeBlocked,
    BindedRedirect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Updates {
    pub messages: Vec<serde_json::Value>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessage {
    pub from_user_id: String,
    pub to_user_id: String,
    pub client_id: String,
    pub message_type: u8,
    pub message_state: u8,
    pub context_token: String,
    pub item_list: Vec<SendItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendItem {
    pub r#type: u8,
    pub text_item: TextItem,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextItem {
    pub text: String,
}

pub fn base_info() -> serde_json::Value {
    serde_json::json!({
        "channel_version": CLIENT_VERSION,
        "bot_agent": concat!("WechatAHP-Rust/", env!("CARGO_PKG_VERSION")),
    })
}

pub fn split_text(text: &str) -> Result<Vec<String>, String> {
    split_text_with_limits(text, MAX_TEXT_BYTES, MAX_CHUNK_BYTES)
}

pub fn split_text_with_limits(
    text: &str,
    max_text_bytes: usize,
    max_chunk_bytes: usize,
) -> Result<Vec<String>, String> {
    if text.is_empty() || text.len() > max_text_bytes {
        return Err("text must be non-empty and at most 16 KiB".into());
    }
    if max_chunk_bytes < 4 {
        return Err("chunk size must allow one Unicode code point".into());
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut bytes = 0;
    for point in text.chars() {
        let size = point.len_utf8();
        if bytes + size > max_chunk_bytes {
            result.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push(point);
        bytes += size;
    }
    if !current.is_empty() {
        result.push(current);
    }
    Ok(result)
}

pub(crate) fn random_uin() -> String {
    let bytes: [u8; 4] = rand_bytes();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn rand_bytes() -> [u8; 4] {
    let value = rand::random::<u32>();
    value.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_chunks_never_split_utf8_or_exceed_limit() {
        let text = "😀".repeat(2_000);
        let chunks = split_text(&text).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.len() <= MAX_CHUNK_BYTES));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn empty_and_oversized_text_are_rejected() {
        assert!(split_text("").is_err());
        assert!(split_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }
}
