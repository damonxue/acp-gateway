//! Paired remote devices.
//!
//! 已配对的远程设备。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::DeviceId;

/// A phone, tablet or browser that has completed pairing with this machine.
///
/// The device's Ed25519 public key is stored so that a future release can
/// require signed ticket requests; today it is recorded at pairing time and
/// used to prove continuity of the same device across re-pairings.
///
/// 已与本机完成配对的手机、平板或浏览器。
///
/// 设备的 Ed25519 公钥会被保存：它在配对时记录，用于校验签名的 ticket 请求，
/// 也用于证明重新配对前后仍是同一台设备。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Gateway-assigned device id. / Gateway 分配的设备 id。
    pub id: DeviceId,
    /// User-visible name (`iPhone 15`). / 用户可见的名称（如 `iPhone 15`）。
    pub name: String,
    /// Base64 (standard, padded) Ed25519 public key supplied at pairing time.
    /// 配对时提供的 Ed25519 公钥（标准 base64，带填充）。
    pub public_key: String,
    /// `ios`, `android`, `web`, … / 设备平台。
    pub platform: String,
    /// Revoked devices keep their history but can no longer obtain tickets.
    /// 已撤销的设备保留历史，但不能再获取 ticket。
    pub revoked: bool,
    /// Pairing time. / 配对时间。
    pub created_at: DateTime<Utc>,
    /// Last metadata change. / 最后一次元数据变更时间。
    pub updated_at: DateTime<Utc>,
    /// Last time the device presented a valid credential.
    /// 设备最后一次出示有效凭证的时间。
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Device {
    /// Whether the device may still authenticate.
    ///
    /// 该设备是否仍可通过认证。
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.revoked
    }
}
