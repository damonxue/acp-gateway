//! The machine a gateway daemon runs on.
//!
//! 运行 Gateway 守护进程的机器。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::MachineId;

/// Identity and presence of one developer machine.
///
/// 一台开发机的身份与在线状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// Stable id, derived from the machine's Ed25519 identity.
    /// 稳定 id，由机器的 Ed25519 身份推导而来。
    pub id: MachineId,
    /// Display name, defaults to the hostname. / 展示名称，默认为主机名。
    pub name: String,
    /// `macos`, `linux`, `windows`. / 平台。
    pub platform: String,
    /// Hostname at registration time. / 注册时的主机名。
    pub hostname: String,
    /// Gateway version string. / Gateway 版本号。
    pub version: String,
    /// Public HTTPS endpoint if a tunnel is configured.
    /// 如果配置了 tunnel，则为其公网 HTTPS 地址。
    pub public_endpoint: Option<String>,
    /// First registration. / 首次注册时间。
    pub created_at: DateTime<Utc>,
    /// Last metadata update. / 最后一次元数据更新时间。
    pub updated_at: DateTime<Utc>,
    /// Last heartbeat. / 最后一次心跳时间。
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Machine {
    /// Whether the machine reported a heartbeat within `window`.
    ///
    /// 机器是否在 `window` 时长内上报过心跳。
    #[must_use]
    pub fn is_online(&self, now: DateTime<Utc>, window: chrono::Duration) -> bool {
        self.last_seen_at
            .is_some_and(|seen| now.signed_duration_since(seen) <= window)
    }
}
