//! [`EventStore`] on SQLite.

use async_trait::async_trait;
use gateway_core::error::{GatewayError, Result};
use gateway_core::event::AgentEvent;
use gateway_core::ids::{EventId, SessionId};
use gateway_core::ports::EventStore;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::time::{from_millis, to_millis};
use crate::{SqlitePool, internal};

/// Append-only event log.
#[derive(Clone, Debug)]
pub struct SqliteEventStore {
    pool: SqlitePool,
}

impl SqliteEventStore {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&self, event: &AgentEvent) -> Result<()> {
        let payload = serde_json::to_string(&event.payload)
            .map_err(|error| GatewayError::EventStore(format!("payload is not JSON: {error}")))?;
        sqlx::query(
            "INSERT INTO events (id, session_id, seq, timestamp, event_type, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(event.id.as_str())
        .bind(event.session_id.as_str())
        .bind(i64::try_from(event.seq).unwrap_or(i64::MAX))
        .bind(to_millis(event.timestamp))
        .bind(&event.event_type)
        .bind(payload)
        .execute(&self.pool)
        .await
        .map_err(|error| {
            // The UNIQUE(session_id, seq) constraint is the last line of
            // defence for the monotonic-sequence invariant. Surfacing it as an
            // event-store error (not an internal one) tells the manager the
            // append failed *before* anything was broadcast.
            GatewayError::EventStore(format!(
                "cannot append seq {} for session {}: {error}",
                event.seq, event.session_id
            ))
        })?;
        Ok(())
    }

    async fn read_after(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<AgentEvent>> {
        let rows = sqlx::query(
            "SELECT * FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )
        .bind(session_id.as_str())
        .bind(i64::try_from(after_seq).unwrap_or(i64::MAX))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.into_iter().map(event_from_row).collect()
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64> {
        let seq: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE session_id = ?1")
                .bind(session_id.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(internal)?;
        Ok(seq.and_then(|value| u64::try_from(value).ok()).unwrap_or(0))
    }

    async fn delete_for_session(&self, session_id: &SessionId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM events WHERE session_id = ?1")
            .bind(session_id.as_str())
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(result.rows_affected())
    }
}

fn event_from_row(row: SqliteRow) -> Result<AgentEvent> {
    let payload: String = row.try_get("payload").map_err(internal)?;
    let seq: i64 = row.try_get("seq").map_err(internal)?;
    Ok(AgentEvent {
        id: EventId::new(row.try_get::<String, _>("id").map_err(internal)?),
        session_id: SessionId::new(row.try_get::<String, _>("session_id").map_err(internal)?),
        seq: u64::try_from(seq).unwrap_or(0),
        timestamp: from_millis(row.try_get("timestamp").map_err(internal)?),
        event_type: row.try_get("event_type").map_err(internal)?,
        payload: serde_json::from_str(&payload).map_err(|error| {
            GatewayError::EventStore(format!("stored payload is not JSON: {error}"))
        })?,
    })
}
