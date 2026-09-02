//! [`PairingRepository`] and [`TicketRepository`] on SQLite.
//!
//! Redemption is a single `UPDATE … WHERE … IS NULL … RETURNING *`. Doing the
//! check in Rust and the write afterwards would leave a window in which two
//! concurrent redemptions both observe an unused credential — exactly the
//! replay the PRD forbids — so the condition and the write must be one
//! statement.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::credential::{PairingCode, WsTicket};
use gateway_core::error::Result;
use gateway_core::ids::{DeviceId, MachineId};
use gateway_core::ports::{PairingRepository, TicketRepository};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::time::{from_millis, from_millis_opt, to_millis, to_millis_opt};
use crate::{SqlitePool, internal};

/// Stores pairing codes.
#[derive(Clone, Debug)]
pub struct SqlitePairingRepository {
    pool: SqlitePool,
}

impl SqlitePairingRepository {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PairingRepository for SqlitePairingRepository {
    async fn insert(&self, code: &PairingCode) -> Result<()> {
        sqlx::query(
            "INSERT INTO pairing_codes (code, nonce, expires_at, consumed_at) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&code.code)
        .bind(&code.nonce)
        .bind(to_millis(code.expires_at))
        .bind(to_millis_opt(code.consumed_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn consume(&self, code: &str, now: DateTime<Utc>) -> Result<Option<PairingCode>> {
        let row = sqlx::query(
            "UPDATE pairing_codes SET consumed_at = ?2 \
             WHERE code = ?1 AND consumed_at IS NULL AND expires_at > ?2 RETURNING *",
        )
        .bind(code)
        .bind(to_millis(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        row.map(pairing_from_row).transpose()
    }

    async fn purge_expired(&self, before: DateTime<Utc>) -> Result<u64> {
        let result = sqlx::query("DELETE FROM pairing_codes WHERE expires_at < ?1")
            .bind(to_millis(before))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(result.rows_affected())
    }
}

/// Stores WebSocket tickets.
#[derive(Clone, Debug)]
pub struct SqliteTicketRepository {
    pool: SqlitePool,
}

impl SqliteTicketRepository {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TicketRepository for SqliteTicketRepository {
    async fn insert(&self, ticket: &WsTicket) -> Result<()> {
        sqlx::query(
            "INSERT INTO ws_tickets (id, device_id, machine_id, nonce, expires_at, used_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&ticket.id)
        .bind(ticket.device_id.as_str())
        .bind(ticket.machine_id.as_str())
        .bind(&ticket.nonce)
        .bind(to_millis(ticket.expires_at))
        .bind(to_millis_opt(ticket.used_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn consume(&self, id: &str, now: DateTime<Utc>) -> Result<Option<WsTicket>> {
        let row = sqlx::query(
            "UPDATE ws_tickets SET used_at = ?2 \
             WHERE id = ?1 AND used_at IS NULL AND expires_at > ?2 RETURNING *",
        )
        .bind(id)
        .bind(to_millis(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        row.map(ticket_from_row).transpose()
    }

    async fn purge_expired(&self, before: DateTime<Utc>) -> Result<u64> {
        let result = sqlx::query("DELETE FROM ws_tickets WHERE expires_at < ?1")
            .bind(to_millis(before))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(result.rows_affected())
    }
}

fn pairing_from_row(row: SqliteRow) -> Result<PairingCode> {
    Ok(PairingCode {
        code: row.try_get("code").map_err(internal)?,
        nonce: row.try_get("nonce").map_err(internal)?,
        expires_at: from_millis(row.try_get("expires_at").map_err(internal)?),
        consumed_at: from_millis_opt(row.try_get("consumed_at").map_err(internal)?),
    })
}

fn ticket_from_row(row: SqliteRow) -> Result<WsTicket> {
    Ok(WsTicket {
        id: row.try_get("id").map_err(internal)?,
        device_id: DeviceId::new(row.try_get::<String, _>("device_id").map_err(internal)?),
        machine_id: MachineId::new(row.try_get::<String, _>("machine_id").map_err(internal)?),
        nonce: row.try_get("nonce").map_err(internal)?,
        expires_at: from_millis(row.try_get("expires_at").map_err(internal)?),
        used_at: from_millis_opt(row.try_get("used_at").map_err(internal)?),
    })
}
