//! Strongly typed identifiers.
//!
//! 强类型标识符。
//!
//! Every identifier in the domain is a newtype over a `String`. Passing bare
//! strings around is the single most common source of "which id is this?"
//! bugs in a system that juggles machine / session / device / event / agent
//! ids simultaneously, so the domain never exposes one.
//!
//! 领域层的每个标识符都是 `String` 的 newtype。在同时摆弄 machine / session / device /
//! event / agent 多种 id 的系统里，到处传裸 `String` 是“这到底是哪个 id”类 bug 最常见
//! 的根源，因此领域层一律不暴露裸字符串。
//!
//! Ids are *opaque* to the rest of the system: only this module knows how a
//! fresh id is shaped.
//!
//! id 对系统其余部分是*不透明*的：只有本模块知道新 id 长什么样。

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Declare a `String` newtype id with a generation prefix.
///
/// 定义一个带生成前缀的 `String` newtype id。
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The prefix used by [`Self::generate`].
            ///
            /// [`Self::generate`] 使用的前缀。
            pub const PREFIX: &'static str = $prefix;

            /// Wrap an existing string without validating its shape.
            ///
            /// Ids minted by other systems (an ACP agent's session id, a relay's
            /// device id) are accepted verbatim; the prefix is a convention for
            /// readability, not a security boundary.
            ///
            /// 包装已有字符串，不校验其格式。其他系统生成的 id（ACP Agent 的 session id、
            /// Relay 的 device id）原样接受；前缀只是为了可读性的约定，不是安全边界。
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Mint a fresh random id.
            ///
            /// 生成一个新的随机 id。
            #[must_use]
            pub fn generate() -> Self {
                Self(format!("{}{}", $prefix, Uuid::new_v4().simple()))
            }

            /// Borrow the underlying string.
            ///
            /// 借用底层字符串。
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the underlying string.
            ///
            /// 消耗自身，返回底层字符串。
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            /// Uses `pad` rather than `write_str` so that ids honour width and
            /// alignment specifiers: the CLI prints them in columns.
            ///
            /// 用 `pad` 而不是 `write_str`，使 id 遵守宽度与对齐格式符：CLI 需要按列打印。
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

define_id!(
    /// Identifies one developer machine running a gateway daemon.
    ///
    /// 标识一台运行 Gateway 守护进程的开发机。
    MachineId,
    "machine_"
);
define_id!(
    /// Identifies a gateway session. Distinct from the *ACP* session id, which
    /// is minted by the agent and stored on [`crate::AgentSession::acp_session_id`].
    ///
    /// 标识一个 Gateway Session。与 *ACP* session id 不同：后者由 Agent 生成，存在
    /// [`crate::AgentSession::acp_session_id`] 上。
    SessionId,
    "sess_"
);
define_id!(
    /// Identifies a paired remote device (phone, tablet, browser).
    ///
    /// 标识一个已配对的远程设备（手机、平板、浏览器）。
    DeviceId,
    "device_"
);
define_id!(
    /// Identifies a single persisted event.
    ///
    /// 标识单个已持久化的事件。
    EventId,
    "evt_"
);
define_id!(
    /// Identifies one outstanding permission request.
    ///
    /// 标识一个待回答的权限请求。
    PermissionId,
    "perm_"
);
define_id!(
    /// Identifies a *configured* agent (`codex`, `claude`, …), not a process.
    ///
    /// 标识一个*已配置*的 Agent（`codex`、`claude` 等），而不是某个进程。
    AgentId,
    ""
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_carry_their_prefix() {
        assert!(SessionId::generate().as_str().starts_with("sess_"));
        assert!(EventId::generate().as_str().starts_with("evt_"));
    }

    #[test]
    fn ids_honour_width_specifiers() {
        assert_eq!(
            format!("[{:<10}]", SessionId::new("sess_1")),
            "[sess_1    ]"
        );
    }

    #[test]
    fn ids_round_trip_through_json() {
        let id = DeviceId::new("device_abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"device_abc\"");
        assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), id);
    }
}
