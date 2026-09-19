use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use crate::types::{
    Credentials, DEFAULT_BASE, LoginPoll, QrChallenge, SendMessage, Updates, base_info, random_uin,
};

#[derive(Debug, Error)]
pub enum WeixinError {
    #[error("invalid Weixin origin")]
    InvalidOrigin,
    #[error("Weixin HTTP {0}")]
    Http(StatusCode),
    #[error("Weixin network request failed")]
    Network(#[source] reqwest::Error),
    #[error("Weixin response is invalid: {0}")]
    Protocol(String),
    #[error("Weixin credentials expired")]
    CredentialsExpired,
    #[error("Weixin business error {0}")]
    Business(i64),
    #[error("operation cancelled")]
    Cancelled,
}

#[async_trait]
pub trait WeixinApi: Send + Sync {
    async fn begin_login(&self) -> Result<QrChallenge, WeixinError>;
    async fn poll_login(
        &self,
        challenge: &QrChallenge,
        verify_code: Option<&str>,
    ) -> Result<LoginPoll, WeixinError>;
    async fn updates(
        &self,
        credentials: &Credentials,
        cursor: &str,
    ) -> Result<Updates, WeixinError>;
    async fn send(
        &self,
        credentials: &Credentials,
        message: &SendMessage,
    ) -> Result<(), WeixinError>;

    /// Switch to a server-provided redirect origin after validating it.
    /// Implementations which do not support redirects can retain the default.
    async fn redirect_base(&self, host: &str) -> Result<(), WeixinError> {
        let _ = host;
        Err(WeixinError::Protocol("redirect is unsupported".into()))
    }
}

#[derive(Clone, Debug)]
pub struct HttpWeixinApi {
    client: Client,
    base_url: Arc<RwLock<String>>,
}

impl HttpWeixinApi {
    pub fn new(base_url: &str) -> Result<Self, WeixinError> {
        Self::with_timeout(base_url, Duration::from_secs(45))
    }

    pub fn with_timeout(base_url: &str, timeout: Duration) -> Result<Self, WeixinError> {
        let base_url = validate_origin(base_url)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(WeixinError::Network)?;
        Ok(Self {
            client,
            base_url: Arc::new(RwLock::new(base_url)),
        })
    }

    pub fn default() -> Result<Self, WeixinError> {
        Self::new(DEFAULT_BASE)
    }

    async fn request(
        &self,
        endpoint: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value, WeixinError> {
        let valid_endpoint = endpoint.strip_prefix("ilink/bot/").is_some_and(|rest| {
            let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
            !path.is_empty()
                && path
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_')
                && !query.contains('#')
                && !query.contains('\\')
        });
        if !valid_endpoint {
            return Err(WeixinError::Protocol("invalid endpoint".into()));
        }
        let version = crate::types::CLIENT_VERSION
            .split('.')
            .map(|s| s.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>();
        let client_version = ((version.first().copied().unwrap_or(0) & 0xff) << 16)
            | ((version.get(1).copied().unwrap_or(0) & 0xff) << 8)
            | (version.get(2).copied().unwrap_or(0) & 0xff);
        let mut request = self
            .client
            .request(
                if body.is_some() {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                format!("{}/{}", self.base_url.read().await, endpoint),
            )
            .header("iLink-App-Id", "bot")
            .header("iLink-App-ClientVersion", client_version.to_string());
        if let Some(body) = body {
            request = request
                .header("AuthorizationType", "ilink_bot_token")
                .header("X-WECHAT-UIN", random_uin())
                .json(&body);
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
        }
        let response = request.send().await.map_err(WeixinError::Network)?;
        if response.status() == StatusCode::UNAUTHORIZED
            || response.status() == StatusCode::FORBIDDEN
        {
            return Err(WeixinError::CredentialsExpired);
        }
        if !response.status().is_success() {
            return Err(WeixinError::Http(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > 1024 * 1024)
        {
            return Err(WeixinError::Protocol("response exceeds 1 MiB".into()));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(WeixinError::Network)?;
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(WeixinError::Protocol("response exceeds 1 MiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| WeixinError::Protocol(format!("invalid JSON response: {error}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| WeixinError::Protocol("response is not an object".into()))?;
        for key in ["ret", "errcode"] {
            let Some(code_value) = object.get(key) else {
                continue;
            };
            let Some(code) = code_value.as_i64() else {
                return Err(WeixinError::Protocol(format!("{key} must be an integer")));
            };
            if code == -14 {
                return Err(WeixinError::CredentialsExpired);
            }
            if code != 0 {
                return Err(WeixinError::Business(code));
            }
        }
        Ok(value)
    }
}

#[async_trait]
impl WeixinApi for HttpWeixinApi {
    async fn begin_login(&self) -> Result<QrChallenge, WeixinError> {
        let value = self
            .request(
                "ilink/bot/get_bot_qrcode?bot_type=3",
                None,
                Some(json!({"local_token_list": []})),
            )
            .await?;
        let code = value
            .get("qrcode")
            .and_then(Value::as_str)
            .ok_or_else(|| WeixinError::Protocol("missing qrcode".into()))?;
        let image = value
            .get("qrcode_img_content")
            .and_then(Value::as_str)
            .ok_or_else(|| WeixinError::Protocol("missing qrcode image".into()))?;
        if code.len() > 4096 || image.len() > 2048 {
            return Err(WeixinError::Protocol("QR response is oversized".into()));
        }
        Ok(QrChallenge {
            code: code.into(),
            image_content: image.into(),
            base_url: self.base_url.read().await.clone(),
        })
    }

    async fn poll_login(
        &self,
        challenge: &QrChallenge,
        verify_code: Option<&str>,
    ) -> Result<LoginPoll, WeixinError> {
        let mut endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            encode_query(&challenge.code)
        );
        if let Some(code) = verify_code {
            endpoint.push_str(&format!("&verify_code={}", encode_query(code)));
        }
        let value = self.request(&endpoint, None, None).await?;
        let current_base = self.base_url.read().await.clone();
        match value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "wait" => Ok(LoginPoll::Wait),
            "scaned" => Ok(LoginPoll::Scanned),
            "need_verifycode" => Ok(LoginPoll::NeedVerifyCode),
            "scaned_but_redirect" => Ok(LoginPoll::Redirect {
                host: value
                    .get("redirect_host")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WeixinError::Protocol("missing redirect host".into()))?
                    .into(),
            }),
            "expired" => Ok(LoginPoll::Expired),
            "verify_code_blocked" => Ok(LoginPoll::VerifyCodeBlocked),
            "binded_redirect" => Ok(LoginPoll::BindedRedirect),
            "confirmed" => {
                let token = value
                    .get("bot_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WeixinError::Protocol("missing bot token".into()))?;
                let bot_id = value
                    .get("ilink_bot_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WeixinError::Protocol("missing bot id".into()))?;
                let owner_id = value
                    .get("ilink_user_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WeixinError::Protocol("missing owner id".into()))?;
                if token.is_empty()
                    || token.len() > 8192
                    || token.contains(['\r', '\n'])
                    || !valid_identifier(bot_id)
                    || !valid_identifier(owner_id)
                    || bot_id == owner_id
                {
                    return Err(WeixinError::Protocol(
                        "invalid confirmed credentials".into(),
                    ));
                }
                let base_url = value
                    .get("baseurl")
                    .and_then(Value::as_str)
                    .unwrap_or(&current_base);
                Ok(LoginPoll::Confirmed(Credentials {
                    bot_id: bot_id.into(),
                    owner_id: owner_id.into(),
                    token: token.into(),
                    base_url: validate_origin(base_url)?,
                }))
            }
            other => Err(WeixinError::Protocol(format!("unknown QR status {other}"))),
        }
    }

    async fn updates(
        &self,
        credentials: &Credentials,
        cursor: &str,
    ) -> Result<Updates, WeixinError> {
        let value = self
            .request(
                "ilink/bot/getupdates",
                Some(&credentials.token),
                Some(json!({"get_updates_buf": cursor, "base_info": base_info()})),
            )
            .await?;
        let messages = value
            .get("msgs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if messages.len() > 200 {
            return Err(WeixinError::Protocol(
                "update batch exceeds 200 messages".into(),
            ));
        }
        let cursor = value
            .get("get_updates_buf")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 64 * 1024)
            .map(str::to_owned);
        Ok(Updates { messages, cursor })
    }

    async fn send(
        &self,
        credentials: &Credentials,
        message: &SendMessage,
    ) -> Result<(), WeixinError> {
        self.request(
            "ilink/bot/sendmessage",
            Some(&credentials.token),
            Some(json!({"msg": message, "base_info": base_info()})),
        )
        .await
        .map(|_| ())
    }

    async fn redirect_base(&self, host: &str) -> Result<(), WeixinError> {
        let base = format!("https://{host}");
        let validated = validate_origin(&base)?;
        *self.base_url.write().await = validated;
        Ok(())
    }
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

pub fn validate_origin(value: &str) -> Result<String, WeixinError> {
    let url = Url::parse(value).map_err(|_| WeixinError::InvalidOrigin)?;
    let authority = value
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || authority.contains(':')
        || value.contains('\\')
        || !url.host_str().is_some_and(valid_host)
    {
        return Err(WeixinError::InvalidOrigin);
    }
    Ok(url.origin().ascii_serialization())
}

fn valid_host(host: &str) -> bool {
    let Some(prefix) = host.strip_suffix(".weixin.qq.com") else {
        return false;
    };
    let Some(label) = prefix.strip_prefix("ilink") else {
        return false;
    };
    label.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowed_https_origins_pass() {
        assert_eq!(validate_origin(DEFAULT_BASE).unwrap(), DEFAULT_BASE);
        for invalid in [
            "http://ilinkai.weixin.qq.com",
            "https://evil.example",
            "https://ilinkai.weixin.qq.com:443",
            "https://ilinkai.weixin.qq.com/path",
            "https://user@ilinkai.weixin.qq.com",
            "https://ilinkai.weixin.qq.com/?x=1",
        ] {
            assert!(validate_origin(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn redirect_hosts_are_origin_only() {
        assert!(validate_origin("https://ilink-eu.weixin.qq.com").is_ok());
        assert!(validate_origin("https://ilink-eu.weixin.qq.com.attacker").is_err());
    }
}
