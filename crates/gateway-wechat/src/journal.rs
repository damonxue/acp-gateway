use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::inbound::InboundMessage;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal is unavailable: {0}")]
    Unavailable(String),
    #[error("journal operation failed: {0}")]
    Operation(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRecord {
    pub binding_id: String,
    pub message: InboundMessage,
    pub received_at: DateTime<Utc>,
    pub delivered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRecord {
    pub id: String,
    pub binding_id: String,
    pub source_id: String,
    pub role: String,
    pub text: String,
    pub context_token: String,
    pub status: OutboxStatus,
    pub sent_parts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxStatus {
    WaitingForContext,
    Sending,
    Sent,
    Uncertain,
}

#[async_trait]
pub trait Journal: Send + Sync {
    async fn cursor(&self, binding_id: &str) -> Result<String, JournalError>;
    async fn set_cursor(&self, binding_id: &str, cursor: &str) -> Result<(), JournalError>;

    /// Persist the inbox record and cursor as one logical operation. Returns
    /// false when the stable message id was already committed.
    async fn commit_inbound(
        &self,
        binding_id: &str,
        cursor: Option<&str>,
        message: &InboundMessage,
    ) -> Result<bool, JournalError>;

    async fn mark_inbound_delivered(&self, id: &str) -> Result<(), JournalError>;
    async fn append_outbox(&self, record: OutboxRecord) -> Result<(), JournalError>;
    async fn update_outbox(
        &self,
        id: &str,
        status: OutboxStatus,
        sent_parts: usize,
    ) -> Result<(), JournalError>;
    async fn waiting_outbox(&self, binding_id: &str) -> Result<Vec<OutboxRecord>, JournalError>;
    async fn set_outbox_context(&self, id: &str, context_token: &str) -> Result<(), JournalError>;
    async fn recover_sending(&self) -> Result<(), JournalError>;
}

#[derive(Debug, Default)]
struct MemoryState {
    cursors: HashMap<String, String>,
    message_ids: HashSet<String>,
    inbox: HashMap<String, InboxRecord>,
    outbox: HashMap<String, OutboxRecord>,
}

/// Deterministic journal used by unit/integration tests. A production daemon
/// can implement the same trait over the gateway SQLite pool without changing
/// polling or bridge semantics.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    state: Mutex<MemoryState>,
}

/// SQLite-backed journal. The gateway store migration creates the four
/// `wechat_*` tables before this adapter is started.
#[derive(Clone, Debug)]
pub struct SqliteJournal {
    pool: sqlx::SqlitePool,
}

impl SqliteJournal {
    #[must_use]
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Journal for SqliteJournal {
    async fn cursor(&self, binding_id: &str) -> Result<String, JournalError> {
        sqlx::query_scalar("SELECT cursor FROM wechat_cursor WHERE binding_id = ?")
            .bind(binding_id)
            .fetch_optional(&self.pool)
            .await
            .map(|value: Option<String>| value.unwrap_or_default())
            .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn set_cursor(&self, binding_id: &str, cursor: &str) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO wechat_cursor(binding_id,cursor,updated_at) VALUES(?,?,?)
             ON CONFLICT(binding_id) DO UPDATE SET cursor=excluded.cursor, updated_at=excluded.updated_at",
        )
        .bind(binding_id)
        .bind(cursor)
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn commit_inbound(
        &self,
        binding_id: &str,
        cursor: Option<&str>,
        message: &InboundMessage,
    ) -> Result<bool, JournalError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| JournalError::Operation(error.to_string()))?;
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT delivered FROM wechat_inbox WHERE id=?")
                .bind(&message.id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| JournalError::Operation(error.to_string()))?;
        if let Some(delivered) = existing {
            if let Some(cursor) = cursor {
                sqlx::query(
                    "INSERT INTO wechat_cursor(binding_id,cursor,updated_at) VALUES(?,?,?)
                     ON CONFLICT(binding_id) DO UPDATE SET cursor=excluded.cursor, updated_at=excluded.updated_at",
                )
                .bind(binding_id)
                .bind(cursor)
                .bind(Utc::now().timestamp_millis())
                .execute(&mut *transaction)
                .await
                .map_err(|error| JournalError::Operation(error.to_string()))?;
            }
            transaction
                .commit()
                .await
                .map_err(|error| JournalError::Operation(error.to_string()))?;
            return Ok(delivered == 0);
        }
        let inserted = sqlx::query(
            "INSERT INTO wechat_inbox(id,binding_id,message_id,text,context_token,received_at)
             VALUES(?,?,?,?,?,?) ON CONFLICT(message_id) DO NOTHING",
        )
        .bind(&message.id)
        .bind(binding_id)
        .bind(&message.message_id)
        .bind(&message.text)
        .bind(&message.context_token)
        .bind(Utc::now().timestamp_millis())
        .execute(&mut *transaction)
        .await
        .map_err(|error| JournalError::Operation(error.to_string()))?
        .rows_affected()
            > 0;
        if let Some(cursor) = cursor {
            sqlx::query(
                "INSERT INTO wechat_cursor(binding_id,cursor,updated_at) VALUES(?,?,?)
                 ON CONFLICT(binding_id) DO UPDATE SET cursor=excluded.cursor, updated_at=excluded.updated_at",
            )
            .bind(binding_id)
            .bind(cursor)
            .bind(Utc::now().timestamp_millis())
            .execute(&mut *transaction)
            .await
            .map_err(|error| JournalError::Operation(error.to_string()))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| JournalError::Operation(error.to_string()))?;
        Ok(inserted)
    }

    async fn mark_inbound_delivered(&self, id: &str) -> Result<(), JournalError> {
        sqlx::query("UPDATE wechat_inbox SET delivered=1 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn append_outbox(&self, record: OutboxRecord) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO wechat_outbox(id,binding_id,source_id,role,text,context_token,status,sent_parts,created_at)
             VALUES(?,?,?,?,?,?,?,?,?)",
        )
        .bind(record.id)
        .bind(record.binding_id)
        .bind(record.source_id)
        .bind(record.role)
        .bind(record.text)
        .bind(record.context_token)
        .bind(outbox_status_name(record.status))
        .bind(record.sent_parts as i64)
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn update_outbox(
        &self,
        id: &str,
        status: OutboxStatus,
        sent_parts: usize,
    ) -> Result<(), JournalError> {
        sqlx::query("UPDATE wechat_outbox SET status=?, sent_parts=? WHERE id=?")
            .bind(outbox_status_name(status))
            .bind(sent_parts as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn waiting_outbox(&self, binding_id: &str) -> Result<Vec<OutboxRecord>, JournalError> {
        let rows = sqlx::query("SELECT id,binding_id,source_id,role,text,context_token,status,sent_parts FROM wechat_outbox WHERE binding_id=? AND status='waiting_for_context' ORDER BY created_at")
            .bind(binding_id).fetch_all(&self.pool).await.map_err(|error| JournalError::Operation(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                use sqlx::Row;
                Ok(OutboxRecord {
                    id: row
                        .try_get("id")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    binding_id: row
                        .try_get("binding_id")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    source_id: row
                        .try_get("source_id")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    role: row
                        .try_get("role")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    text: row
                        .try_get("text")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    context_token: row
                        .try_get("context_token")
                        .map_err(|error| JournalError::Operation(error.to_string()))?,
                    status: OutboxStatus::WaitingForContext,
                    sent_parts: row
                        .try_get::<i64, _>("sent_parts")
                        .map_err(|error| JournalError::Operation(error.to_string()))?
                        as usize,
                })
            })
            .collect()
    }

    async fn set_outbox_context(&self, id: &str, context_token: &str) -> Result<(), JournalError> {
        sqlx::query("UPDATE wechat_outbox SET context_token=?, status='sending' WHERE id=?")
            .bind(context_token)
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| JournalError::Operation(error.to_string()))
    }

    async fn recover_sending(&self) -> Result<(), JournalError> {
        sqlx::query("UPDATE wechat_outbox SET status='uncertain' WHERE status='sending'")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| JournalError::Operation(error.to_string()))
    }
}

fn outbox_status_name(status: OutboxStatus) -> &'static str {
    match status {
        OutboxStatus::WaitingForContext => "waiting_for_context",
        OutboxStatus::Sending => "sending",
        OutboxStatus::Sent => "sent",
        OutboxStatus::Uncertain => "uncertain",
    }
}

impl MemoryJournal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inbox(&self, id: &str) -> Option<InboxRecord> {
        self.state.lock().ok()?.inbox.get(id).cloned()
    }

    pub fn outbox(&self, id: &str) -> Option<OutboxRecord> {
        self.state.lock().ok()?.outbox.get(id).cloned()
    }
}

#[async_trait]
impl Journal for MemoryJournal {
    async fn cursor(&self, binding_id: &str) -> Result<String, JournalError> {
        let state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        Ok(state.cursors.get(binding_id).cloned().unwrap_or_default())
    }

    async fn commit_inbound(
        &self,
        binding_id: &str,
        cursor: Option<&str>,
        message: &InboundMessage,
    ) -> Result<bool, JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        if state.message_ids.contains(&message.id) {
            if let Some(cursor) = cursor {
                state
                    .cursors
                    .insert(binding_id.to_owned(), cursor.to_owned());
            }
            return Ok(!state
                .inbox
                .get(&message.id)
                .is_some_and(|record| record.delivered));
        }
        state.message_ids.insert(message.id.clone());
        state.inbox.insert(
            message.id.clone(),
            InboxRecord {
                binding_id: binding_id.to_owned(),
                message: message.clone(),
                received_at: Utc::now(),
                delivered: false,
            },
        );
        if let Some(cursor) = cursor {
            state
                .cursors
                .insert(binding_id.to_owned(), cursor.to_owned());
        }
        Ok(true)
    }

    async fn set_cursor(&self, binding_id: &str, cursor: &str) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        state
            .cursors
            .insert(binding_id.to_owned(), cursor.to_owned());
        Ok(())
    }

    async fn mark_inbound_delivered(&self, id: &str) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        if let Some(record) = state.inbox.get_mut(id) {
            record.delivered = true;
        }
        Ok(())
    }

    async fn append_outbox(&self, record: OutboxRecord) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        state.outbox.insert(record.id.clone(), record);
        Ok(())
    }

    async fn update_outbox(
        &self,
        id: &str,
        status: OutboxStatus,
        sent_parts: usize,
    ) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        if let Some(record) = state.outbox.get_mut(id) {
            record.status = status;
            record.sent_parts = sent_parts;
        }
        Ok(())
    }

    async fn waiting_outbox(&self, binding_id: &str) -> Result<Vec<OutboxRecord>, JournalError> {
        let state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        Ok(state
            .outbox
            .values()
            .filter(|record| {
                record.binding_id == binding_id && record.status == OutboxStatus::WaitingForContext
            })
            .cloned()
            .collect())
    }

    async fn set_outbox_context(&self, id: &str, context_token: &str) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        if let Some(record) = state.outbox.get_mut(id) {
            record.context_token = context_token.to_owned();
            record.status = OutboxStatus::Sending;
        }
        Ok(())
    }

    async fn recover_sending(&self) -> Result<(), JournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| JournalError::Unavailable("journal lock poisoned".into()))?;
        for record in state.outbox.values_mut() {
            if record.status == OutboxStatus::Sending {
                record.status = OutboxStatus::Uncertain;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::InboundMessage;

    fn inbound(id: &str) -> InboundMessage {
        InboundMessage {
            id: id.into(),
            message_id: id.into(),
            text: "hi".into(),
            context_token: "ctx".into(),
        }
    }

    #[tokio::test]
    async fn duplicate_commit_is_idempotent_and_recovery_marks_sending_uncertain() {
        let journal = MemoryJournal::new();
        assert!(
            journal
                .commit_inbound("b", Some("c1"), &inbound("m"))
                .await
                .unwrap()
        );
        journal.mark_inbound_delivered("m").await.unwrap();
        assert!(
            !journal
                .commit_inbound("b", Some("c2"), &inbound("m"))
                .await
                .unwrap()
        );
        assert_eq!(journal.cursor("b").await.unwrap(), "c2");
        journal
            .append_outbox(OutboxRecord {
                id: "o".into(),
                binding_id: "b".into(),
                source_id: "s".into(),
                role: "agent".into(),
                text: "x".into(),
                context_token: "ctx".into(),
                status: OutboxStatus::Sending,
                sent_parts: 0,
            })
            .await
            .unwrap();
        journal.recover_sending().await.unwrap();
        assert_eq!(journal.outbox("o").unwrap().status, OutboxStatus::Uncertain);
    }
}
