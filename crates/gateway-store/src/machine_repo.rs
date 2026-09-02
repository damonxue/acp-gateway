//! [`MachineRepository`] on SQLite.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::error::Result;
use gateway_core::ids::MachineId;
use gateway_core::machine::Machine;
use gateway_core::ports::MachineRepository;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::time::{from_millis, from_millis_opt, to_millis, to_millis_opt};
use crate::{SqlitePool, internal};

/// Stores the local machine record.
#[derive(Clone, Debug)]
pub struct SqliteMachineRepository {
    pool: SqlitePool,
}

impl SqliteMachineRepository {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MachineRepository for SqliteMachineRepository {
    async fn upsert(&self, machine: &Machine) -> Result<()> {
        sqlx::query(
            "INSERT INTO machines (id, name, platform, hostname, version, public_endpoint, \
             created_at, updated_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
             hostname = excluded.hostname, version = excluded.version, \
             public_endpoint = excluded.public_endpoint, updated_at = excluded.updated_at",
        )
        .bind(machine.id.as_str())
        .bind(&machine.name)
        .bind(&machine.platform)
        .bind(&machine.hostname)
        .bind(&machine.version)
        .bind(machine.public_endpoint.as_deref())
        .bind(to_millis(machine.created_at))
        .bind(to_millis(machine.updated_at))
        .bind(to_millis_opt(machine.last_seen_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn get(&self, id: &MachineId) -> Result<Option<Machine>> {
        let row = sqlx::query("SELECT * FROM machines WHERE id = ?1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
        row.map(machine_from_row).transpose()
    }

    async fn touch(&self, id: &MachineId, at: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE machines SET last_seen_at = ?2, updated_at = ?2 WHERE id = ?1")
            .bind(id.as_str())
            .bind(to_millis(at))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }
}

fn machine_from_row(row: SqliteRow) -> Result<Machine> {
    Ok(Machine {
        id: MachineId::new(row.try_get::<String, _>("id").map_err(internal)?),
        name: row.try_get("name").map_err(internal)?,
        platform: row.try_get("platform").map_err(internal)?,
        hostname: row.try_get("hostname").map_err(internal)?,
        version: row.try_get("version").map_err(internal)?,
        public_endpoint: row.try_get("public_endpoint").map_err(internal)?,
        created_at: from_millis(row.try_get("created_at").map_err(internal)?),
        updated_at: from_millis(row.try_get("updated_at").map_err(internal)?),
        last_seen_at: from_millis_opt(row.try_get("last_seen_at").map_err(internal)?),
    })
}
