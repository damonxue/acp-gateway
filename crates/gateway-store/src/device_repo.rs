//! [`DeviceRepository`] on SQLite.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::device::Device;
use gateway_core::error::Result;
use gateway_core::ids::DeviceId;
use gateway_core::ports::DeviceRepository;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::time::{from_millis, from_millis_opt, to_millis, to_millis_opt};
use crate::{SqlitePool, internal};

/// Stores paired devices.
#[derive(Clone, Debug)]
pub struct SqliteDeviceRepository {
    pool: SqlitePool,
}

impl SqliteDeviceRepository {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DeviceRepository for SqliteDeviceRepository {
    async fn upsert(&self, device: &Device) -> Result<()> {
        sqlx::query(
            "INSERT INTO devices (id, name, public_key, platform, revoked, created_at, \
             updated_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
             public_key = excluded.public_key, platform = excluded.platform, \
             revoked = excluded.revoked, updated_at = excluded.updated_at",
        )
        .bind(device.id.as_str())
        .bind(&device.name)
        .bind(&device.public_key)
        .bind(&device.platform)
        .bind(i64::from(device.revoked))
        .bind(to_millis(device.created_at))
        .bind(to_millis(device.updated_at))
        .bind(to_millis_opt(device.last_seen_at))
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn get(&self, id: &DeviceId) -> Result<Option<Device>> {
        let row = sqlx::query("SELECT * FROM devices WHERE id = ?1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
        row.map(device_from_row).transpose()
    }

    async fn list(&self) -> Result<Vec<Device>> {
        let rows = sqlx::query("SELECT * FROM devices ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        rows.into_iter().map(device_from_row).collect()
    }

    async fn set_revoked(&self, id: &DeviceId, revoked: bool, at: DateTime<Utc>) -> Result<bool> {
        let result = sqlx::query("UPDATE devices SET revoked = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id.as_str())
            .bind(i64::from(revoked))
            .bind(to_millis(at))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(result.rows_affected() > 0)
    }

    async fn touch(&self, id: &DeviceId, at: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE devices SET last_seen_at = ?2 WHERE id = ?1")
            .bind(id.as_str())
            .bind(to_millis(at))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }
}

fn device_from_row(row: SqliteRow) -> Result<Device> {
    let revoked: i64 = row.try_get("revoked").map_err(internal)?;
    Ok(Device {
        id: DeviceId::new(row.try_get::<String, _>("id").map_err(internal)?),
        name: row.try_get("name").map_err(internal)?,
        public_key: row.try_get("public_key").map_err(internal)?,
        platform: row.try_get("platform").map_err(internal)?,
        revoked: revoked != 0,
        created_at: from_millis(row.try_get("created_at").map_err(internal)?),
        updated_at: from_millis(row.try_get("updated_at").map_err(internal)?),
        last_seen_at: from_millis_opt(row.try_get("last_seen_at").map_err(internal)?),
    })
}
