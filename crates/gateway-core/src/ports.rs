//! Driven ports: everything the domain needs from the outside world.
//!
//! 被驱动端口：领域层对外部世界的全部需求。
//!
//! Each trait is deliberately narrow. `gateway-store` implements the
//! persistence ports on SQLite; tests implement them in memory.
//!
//! 每个 trait 都故意保持狭窄。`gateway-store` 在 SQLite 上实现持久化端口；
//! 测试则用内存实现。

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::credential::{PairingCode, WsTicket};
use crate::device::Device;
use crate::error::Result;
use crate::event::AgentEvent;
use crate::ids::{DeviceId, MachineId, SessionId};
use crate::machine::Machine;
use crate::session::{AgentSession, SessionStatus};

/// Persistence for the session aggregate.
///
/// Session 聚合根的持久化。
#[async_trait]
pub trait SessionRepository: Send + Sync + std::fmt::Debug {
    /// Insert a new session.
    ///
    /// 插入一个新 Session。
    async fn insert(&self, session: &AgentSession) -> Result<()>;

    /// Read one session.
    ///
    /// 读取单个 Session。
    async fn get(&self, id: &SessionId) -> Result<Option<AgentSession>>;

    /// List sessions for a machine, newest activity first.
    ///
    /// 列出某台机器的 Session，活跃时间最新的在前。
    async fn list(&self, machine_id: &MachineId, limit: u32) -> Result<Vec<AgentSession>>;

    /// Persist status, `last_seq`, title and ACP session id in one write.
    ///
    /// A single method (instead of one per field) keeps the "update session
    /// row" step of the event pipeline to exactly one round trip.
    ///
    /// 一次写入就持久化 status、`last_seq`、title 和 ACP session id。
    ///
    /// 用一个方法（而不是每个字段一个）能保证事件管道中“更新 Session 行”这一步
    /// 只有一次往返。
    async fn update_runtime_state(
        &self,
        id: &SessionId,
        status: SessionStatus,
        last_seq: u64,
        acp_session_id: Option<&str>,
        title: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Move every non-terminal session of a machine to `status`.
    ///
    /// Used on daemon startup: sessions that were running when the process
    /// died are `Disconnected`, not `Running`.
    ///
    /// 将某台机器上所有非终止状态的 Session 置为 `status`。
    ///
    /// 用于守护进程启动：进程死掉时正在运行的 Session 应该是 `Disconnected`，
    /// 而不是 `Running`。
    async fn mark_non_terminal(
        &self,
        machine_id: &MachineId,
        status: SessionStatus,
        updated_at: DateTime<Utc>,
    ) -> Result<u64>;
}

/// Append-only persistence for events.
///
/// 事件的只追加持久化。
#[async_trait]
pub trait EventStore: Send + Sync + std::fmt::Debug {
    /// Append one event. Must reject duplicate `(session_id, seq)`.
    ///
    /// 追加一个事件。必须拒绝重复的 `(session_id, seq)`。
    async fn append(&self, event: &AgentEvent) -> Result<()>;

    /// Return events with `seq > after_seq`, ascending, capped at `limit`.
    ///
    /// 返回 `seq > after_seq` 的事件，升序，数量不超过 `limit`。
    async fn read_after(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<AgentEvent>>;

    /// Highest `seq` written for a session, or 0.
    ///
    /// 某个 Session 已写入的最大 `seq`，没有则为 0。
    async fn last_seq(&self, session_id: &SessionId) -> Result<u64>;

    /// Delete events of a session (used by retention/pruning).
    ///
    /// 删除某个 Session 的事件（用于保留策略 / 清理）。
    async fn delete_for_session(&self, session_id: &SessionId) -> Result<u64>;
}

/// Persistence for machine identity and presence.
///
/// 机器身份与在线状态的持久化。
#[async_trait]
pub trait MachineRepository: Send + Sync + std::fmt::Debug {
    /// Insert or update the machine row.
    ///
    /// 插入或更新机器记录。
    async fn upsert(&self, machine: &Machine) -> Result<()>;

    /// Read one machine.
    ///
    /// 读取单台机器。
    async fn get(&self, id: &MachineId) -> Result<Option<Machine>>;

    /// Record a heartbeat.
    ///
    /// 记录一次心跳。
    async fn touch(&self, id: &MachineId, at: DateTime<Utc>) -> Result<()>;
}

/// Persistence for paired devices.
///
/// 已配对设备的持久化。
#[async_trait]
pub trait DeviceRepository: Send + Sync + std::fmt::Debug {
    /// Insert or update a device.
    ///
    /// 插入或更新一个设备。
    async fn upsert(&self, device: &Device) -> Result<()>;

    /// Read one device.
    ///
    /// 读取单个设备。
    async fn get(&self, id: &DeviceId) -> Result<Option<Device>>;

    /// All devices, newest first.
    ///
    /// 所有设备，最新的在前。
    async fn list(&self) -> Result<Vec<Device>>;

    /// Flip the revoked flag. Returns `false` if the device is unknown.
    ///
    /// 修改撤销标志。设备不存在时返回 `false`。
    async fn set_revoked(&self, id: &DeviceId, revoked: bool, at: DateTime<Utc>) -> Result<bool>;

    /// Record that the device presented a valid credential.
    ///
    /// 记录该设备出示过有效凭证。
    async fn touch(&self, id: &DeviceId, at: DateTime<Utc>) -> Result<()>;
}

/// Persistence for pairing codes.
///
/// 配对码的持久化。
#[async_trait]
pub trait PairingRepository: Send + Sync + std::fmt::Debug {
    /// Store a freshly minted code.
    ///
    /// 保存一个新生成的配对码。
    async fn insert(&self, code: &PairingCode) -> Result<()>;

    /// Atomically redeem a code.
    ///
    /// Returns the code only if it was unconsumed and unexpired at `now`; the
    /// check and the write must happen in one statement so two devices racing
    /// on the same QR code cannot both win.
    ///
    /// 原子地核销一个配对码。
    ///
    /// 仅当它在 `now` 时未被使用且未过期时才返回；检查与写入必须在同一条语句里完成，
    /// 这样两个设备抢同一个二维码时不会同时成功。
    async fn consume(&self, code: &str, now: DateTime<Utc>) -> Result<Option<PairingCode>>;

    /// Drop codes that expired before `before`. Returns how many were removed.
    ///
    /// 删除在 `before` 之前已过期的配对码，返回删除数量。
    async fn purge_expired(&self, before: DateTime<Utc>) -> Result<u64>;
}

/// Persistence for WebSocket tickets.
///
/// WebSocket ticket 的持久化。
#[async_trait]
pub trait TicketRepository: Send + Sync + std::fmt::Debug {
    /// Store a freshly issued ticket.
    ///
    /// 保存一个新签发的 ticket。
    async fn insert(&self, ticket: &WsTicket) -> Result<()>;

    /// Atomically redeem a ticket. See [`PairingRepository::consume`].
    ///
    /// 原子地核销一个 ticket，详见 [`PairingRepository::consume`]。
    async fn consume(&self, id: &str, now: DateTime<Utc>) -> Result<Option<WsTicket>>;

    /// Drop tickets that expired before `before`. Returns how many were removed.
    ///
    /// 删除在 `before` 之前已过期的 ticket，返回删除数量。
    async fn purge_expired(&self, before: DateTime<Utc>) -> Result<u64>;
}

/// Wall clock, injectable so time-dependent logic is testable.
///
/// 壁钟；可注入，以便测试与时间相关的逻辑。
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current time.
    ///
    /// 当前时间。
    fn now(&self) -> DateTime<Utc>;
}

/// The real clock.
///
/// 真实的系统时钟。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_moves_forward() {
        let clock = SystemClock;
        assert!(clock.now() <= SystemClock.now());
    }
}
