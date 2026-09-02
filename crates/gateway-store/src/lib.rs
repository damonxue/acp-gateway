//! # gateway-store
//!
//! SQLite adapters for the persistence ports declared in
//! [`gateway_core::ports`].
//!
//! The crate has exactly one responsibility: turn domain values into rows and
//! back. It contains no policy — no expiry decisions, no status transitions,
//! no id minting — with one deliberate exception: *atomic* redemption of
//! pairing codes and tickets, which can only be made race-free by the database
//! (`UPDATE … WHERE consumed_at IS NULL`) and is therefore expressed as SQL.
//!
//! ## Time
//!
//! Every timestamp is stored as UTC milliseconds since the Unix epoch. The
//! conversion lives in [`time`] so no call site invents its own.
//!
//! ```no_run
//! # async fn run() -> gateway_core::Result<()> {
//! use gateway_store::Database;
//!
//! let db = Database::connect(std::path::Path::new("/tmp/gateway.db")).await?;
//! let sessions = db.sessions();
//! # let _ = sessions;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod credential_repo;
mod device_repo;
mod event_store;
mod machine_repo;
mod session_repo;
pub mod time;

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use gateway_core::error::{GatewayError, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Pool, Sqlite};
use tracing::info;

pub use credential_repo::{SqlitePairingRepository, SqliteTicketRepository};
pub use device_repo::SqliteDeviceRepository;
pub use event_store::SqliteEventStore;
pub use machine_repo::SqliteMachineRepository;
pub use session_repo::SqliteSessionRepository;

/// Alias for the concrete pool type used throughout the crate.
pub type SqlitePool = Pool<Sqlite>;

/// An open, migrated gateway database.
///
/// `Database` is a cheap handle around a connection pool; clone it freely.
/// It also acts as the composition root for the repositories, so wiring code
/// says `db.sessions()` instead of naming concrete adapter types.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open (creating if needed) and migrate the database at `path`.
    ///
    /// # Errors
    /// Fails if the file cannot be opened or a migration does not apply.
    pub async fn connect(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                GatewayError::internal(format!("cannot create data dir: {error}"))
            })?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(internal)?
            .create_if_missing(true)
            // WAL keeps the event writer from blocking readers: a phone
            // replaying history must never stall an agent's output.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        Self::from_options(options, 8).await
    }

    /// Open a private in-memory database. Used by tests.
    ///
    /// # Errors
    /// Fails if migrations do not apply.
    pub async fn connect_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:").map_err(internal)?;
        // One connection, or every pooled connection would see its own empty
        // in-memory database.
        Self::from_options(options, 1).await
    }

    async fn from_options(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(internal)?;
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|error| GatewayError::internal(format!("migration failed: {error}")))?;
        info!("database ready");
        Ok(Self { pool })
    }

    /// The underlying pool, for callers that need raw SQL.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Session persistence.
    #[must_use]
    pub fn sessions(&self) -> Arc<SqliteSessionRepository> {
        Arc::new(SqliteSessionRepository::new(self.pool.clone()))
    }

    /// Event persistence.
    #[must_use]
    pub fn events(&self) -> Arc<SqliteEventStore> {
        Arc::new(SqliteEventStore::new(self.pool.clone()))
    }

    /// Machine persistence.
    #[must_use]
    pub fn machines(&self) -> Arc<SqliteMachineRepository> {
        Arc::new(SqliteMachineRepository::new(self.pool.clone()))
    }

    /// Device persistence.
    #[must_use]
    pub fn devices(&self) -> Arc<SqliteDeviceRepository> {
        Arc::new(SqliteDeviceRepository::new(self.pool.clone()))
    }

    /// Pairing code persistence.
    #[must_use]
    pub fn pairing_codes(&self) -> Arc<SqlitePairingRepository> {
        Arc::new(SqlitePairingRepository::new(self.pool.clone()))
    }

    /// Ticket persistence.
    #[must_use]
    pub fn tickets(&self) -> Arc<SqliteTicketRepository> {
        Arc::new(SqliteTicketRepository::new(self.pool.clone()))
    }

    /// Close the pool, flushing WAL state.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Map any database failure to the domain's internal error.
pub(crate) fn internal(error: impl std::fmt::Display) -> GatewayError {
    GatewayError::internal(format!("database: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_to_a_fresh_database() {
        let db = Database::connect_in_memory().await.unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(db.pool())
                .await
                .unwrap();
        for expected in [
            "devices",
            "events",
            "machines",
            "pairing_codes",
            "sessions",
            "ws_tickets",
        ] {
            assert!(
                tables.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }
}
