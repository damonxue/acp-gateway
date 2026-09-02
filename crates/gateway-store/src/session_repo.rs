//! [`SessionRepository`] on SQLite.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::error::Result;
use gateway_core::ids::{AgentId, MachineId, SessionId};
use gateway_core::ports::SessionRepository;
use gateway_core::session::{AgentSession, SessionOrigin, SessionStatus};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::time::{from_millis, to_millis};
use crate::{SqlitePool, internal};

/// Stores sessions in the `sessions` table.
#[derive(Clone, Debug)]
pub struct SqliteSessionRepository {
    pool: SqlitePool,
}

impl SqliteSessionRepository {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn insert(&self, session: &AgentSession) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, agent_id, agent_name, acp_session_id, \
             workspace, cwd, title, origin, status, last_seq, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(session.id.as_str())
        .bind(session.machine_id.as_str())
        .bind(session.agent_id.as_str())
        .bind(&session.agent_name)
        .bind(session.acp_session_id.as_deref())
        .bind(session.workspace.to_string_lossy().as_ref())
        .bind(session.cwd.to_string_lossy().as_ref())
        .bind(session.title.as_deref())
        .bind(session.origin.as_str())
        .bind(session.status.as_str())
        .bind(i64::try_from(session.last_seq).unwrap_or(i64::MAX))
        .bind(to_millis(session.created_at))
        .bind(to_millis(session.updated_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn get(&self, id: &SessionId) -> Result<Option<AgentSession>> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
        row.map(session_from_row).transpose()
    }

    async fn list(&self, machine_id: &MachineId, limit: u32) -> Result<Vec<AgentSession>> {
        let rows = sqlx::query(
            "SELECT * FROM sessions WHERE machine_id = ?1 ORDER BY updated_at DESC LIMIT ?2",
        )
        .bind(machine_id.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.into_iter().map(session_from_row).collect()
    }

    async fn update_runtime_state(
        &self,
        id: &SessionId,
        status: SessionStatus,
        last_seq: u64,
        acp_session_id: Option<&str>,
        title: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET status = ?2, last_seq = ?3, acp_session_id = ?4, title = ?5, \
             updated_at = ?6 WHERE id = ?1",
        )
        .bind(id.as_str())
        .bind(status.as_str())
        .bind(i64::try_from(last_seq).unwrap_or(i64::MAX))
        .bind(acp_session_id)
        .bind(title)
        .bind(to_millis(updated_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn mark_non_terminal(
        &self,
        machine_id: &MachineId,
        status: SessionStatus,
        updated_at: DateTime<Utc>,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE sessions SET status = ?2, updated_at = ?3 \
             WHERE machine_id = ?1 AND status NOT IN ('completed', 'failed', 'disconnected')",
        )
        .bind(machine_id.as_str())
        .bind(status.as_str())
        .bind(to_millis(updated_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(result.rows_affected())
    }
}

fn session_from_row(row: SqliteRow) -> Result<AgentSession> {
    let status: String = row.try_get("status").map_err(internal)?;
    let origin: String = row.try_get("origin").map_err(internal)?;
    let last_seq: i64 = row.try_get("last_seq").map_err(internal)?;
    let workspace: String = row.try_get("workspace").map_err(internal)?;
    let cwd: String = row.try_get("cwd").map_err(internal)?;
    Ok(AgentSession {
        id: SessionId::new(row.try_get::<String, _>("id").map_err(internal)?),
        machine_id: MachineId::new(row.try_get::<String, _>("machine_id").map_err(internal)?),
        agent_id: AgentId::new(row.try_get::<String, _>("agent_id").map_err(internal)?),
        agent_name: row.try_get("agent_name").map_err(internal)?,
        acp_session_id: row.try_get("acp_session_id").map_err(internal)?,
        workspace: workspace.into(),
        cwd: cwd.into(),
        title: row.try_get("title").map_err(internal)?,
        origin: SessionOrigin::from_str_opt(&origin).unwrap_or(SessionOrigin::Gateway),
        // An unknown status can only come from a newer gateway version or a
        // hand-edited row; treating it as `Disconnected` keeps the session
        // listable and replayable but not drivable.
        status: SessionStatus::from_str_opt(&status).unwrap_or(SessionStatus::Disconnected),
        last_seq: u64::try_from(last_seq).unwrap_or(0),
        created_at: from_millis(row.try_get("created_at").map_err(internal)?),
        updated_at: from_millis(row.try_get("updated_at").map_err(internal)?),
    })
}
