//! Short-lived, single-use credentials: pairing codes and WebSocket tickets.
//!
//! 短时效、一次性凭证：配对码与 WebSocket ticket。
//!
//! Both types share one shape — mint, hand to a human or a relay, redeem
//! exactly once before an expiry — so they share one module and one set of
//! rules. The *atomic* part of "exactly once" belongs to the repository
//! ([`crate::ports::PairingRepository::consume`],
//! [`crate::ports::TicketRepository::consume`]); the predicates here describe
//! what a redeemed credential is allowed to look like.
//!
//! 两者形状相同——生成、交给人或 Relay、在过期前恰好核销一次——因此共用一个模块与一套规则。
//! “恰好一次”中的*原子性*部分属于仓储层（[`crate::ports::PairingRepository::consume`]、
//! [`crate::ports::TicketRepository::consume`]）；这里的谓词只描述一个可核销的凭证
//! 应该长什么样。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{DeviceId, MachineId};

/// A pairing code shown as a QR code on the developer's machine.
///
/// 在开发机上以二维码形式展示的配对码。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    /// Short numeric code the user can also type by hand.
    /// 短数字码，用户也可以手动输入。
    pub code: String,
    /// Random nonce, echoed by the device to prove it saw this exact code.
    /// 随机 nonce，设备需原样回传，以证明它看到的就是这个码。
    pub nonce: String,
    /// Expiry. Pairing codes are minted per pairing attempt and live minutes.
    /// 过期时间。配对码每次配对生成一个，存活几分钟。
    pub expires_at: DateTime<Utc>,
    /// When the code was redeemed, if it was. / 如已核销，则为核销时间。
    pub consumed_at: Option<DateTime<Utc>>,
}

impl PairingCode {
    /// Whether the code can still be redeemed at `now`.
    ///
    /// 在 `now` 时刻该码是否仍可核销。
    #[must_use]
    pub fn is_redeemable(&self, now: DateTime<Utc>) -> bool {
        self.consumed_at.is_none() && now < self.expires_at
    }
}

/// A one-shot credential admitting exactly one WebSocket connection.
///
/// Requirements come straight from the PRD: short-lived, single-use, bound to
/// both the device and the machine, and carrying a nonce so a captured ticket
/// cannot be replayed against a different gateway.
///
/// 一次性凭证，只允许建立一条 WebSocket 连接。
///
/// 要求直接来自 PRD：短时效、一次性、同时绑定设备与机器，并带有 nonce，
/// 使被截获的 ticket 无法重放到另一个 Gateway。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsTicket {
    /// Opaque ticket id, the value presented in the `?ticket=` query parameter.
    /// 不透明的 ticket id，即 `?ticket=` 查询参数中提交的值。
    pub id: String,
    /// Device the ticket was issued to. / ticket 签发给哪个设备。
    pub device_id: DeviceId,
    /// Machine the ticket is valid for. / ticket 对哪台机器有效。
    pub machine_id: MachineId,
    /// Random nonce bound into the ticket. / 绑定在 ticket 中的随机 nonce。
    pub nonce: String,
    /// Expiry, typically seconds away. / 过期时间，通常只有几秒。
    pub expires_at: DateTime<Utc>,
    /// When the ticket was redeemed, if it was. / 如已核销，则为核销时间。
    pub used_at: Option<DateTime<Utc>>,
}

impl WsTicket {
    /// Whether the ticket may still be redeemed by `machine_id` at `now`.
    ///
    /// 在 `now` 时刻，`machine_id` 是否仍可核销该 ticket。
    #[must_use]
    pub fn is_redeemable(&self, machine_id: &MachineId, now: DateTime<Utc>) -> bool {
        self.used_at.is_none() && now < self.expires_at && &self.machine_id == machine_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn ticket(now: DateTime<Utc>) -> WsTicket {
        WsTicket {
            id: "tkt_1".to_owned(),
            device_id: DeviceId::new("device_1"),
            machine_id: MachineId::new("machine_1"),
            nonce: "n".to_owned(),
            expires_at: now + Duration::seconds(60),
            used_at: None,
        }
    }

    #[test]
    fn a_ticket_is_bound_to_one_machine_and_one_use() {
        let now = Utc::now();
        let fresh = ticket(now);
        assert!(fresh.is_redeemable(&MachineId::new("machine_1"), now));
        assert!(!fresh.is_redeemable(&MachineId::new("machine_2"), now));
        assert!(!fresh.is_redeemable(&MachineId::new("machine_1"), now + Duration::seconds(61)));

        let used = WsTicket {
            used_at: Some(now),
            ..ticket(now)
        };
        assert!(!used.is_redeemable(&MachineId::new("machine_1"), now));
    }
}
