//! A string that cannot be printed by accident.
//!
//! 一种不会被误打印的字符串。

use std::fmt;

use serde::{Deserialize, Serialize};

/// A configuration value that must never reach a log line.
///
/// Tunnel tokens and relay credentials are ordinary `String`s as far as the
/// wire is concerned, but the rule "a tunnel token must never be logged" cannot
/// be enforced by review alone — one `debug!(?config)` is enough to leak it.
/// Wrapping them makes the *type* refuse: both `Debug` and `Display` print a
/// placeholder, and the plaintext is only reachable through the deliberately
/// ugly [`Secret::expose`].
///
/// 一个绝不允许出现在日志里的配置值。
///
/// 从线上看，tunnel token 和 relay 凭证只是普通 `String`，但“tunnel token 不能写进日志”
/// 这条规则光靠代码评审是管不住的——一句 `debug!(?config)` 就足以泄露。将它包装后，
/// 是*类型*在拒绝：`Debug` 和 `Display` 都只输出占位符，明文只能通过故意难看的
/// [`Secret::expose`] 取得。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a sensitive value.
    ///
    /// 包装一个敏感值。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the plaintext. Every call site is a place to justify in review.
    ///
    /// 读取明文。每一个调用点都需要在评审时给出理由。
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty.
    ///
    /// 密钥是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_in_both_formatters() {
        let secret = Secret::new("cloudflare-token");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(format!("{secret}"), "***");
        assert!(!format!("{secret:?} {secret}").contains("cloudflare"));
        assert_eq!(secret.expose(), "cloudflare-token");
    }
}
