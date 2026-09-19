use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::api::{WeixinApi, WeixinError};
use crate::journal::{Journal, JournalError};
use crate::types::Credentials;

#[derive(Debug, thiserror::Error)]
pub enum PollerError {
    #[error(transparent)]
    Weixin(#[from] WeixinError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("session bridge failed: {0}")]
    Bridge(String),
}

/// One long-polling worker. A single instance must be used for a binding;
/// starting two workers would race the cursor and can duplicate delivery.
#[derive(Debug)]
pub struct Poller<A, J> {
    api: Arc<A>,
    journal: Arc<J>,
    credentials: Credentials,
    binding_id: String,
    cancel: CancellationToken,
    min_backoff: Duration,
    max_backoff: Duration,
    max_text_bytes: usize,
}

impl<A, J> Poller<A, J>
where
    A: WeixinApi + 'static,
    J: Journal + 'static,
{
    pub fn new(
        api: Arc<A>,
        journal: Arc<J>,
        credentials: Credentials,
        binding_id: impl Into<String>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            api,
            journal,
            credentials,
            binding_id: binding_id.into(),
            cancel,
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            max_text_bytes: crate::types::MAX_TEXT_BYTES,
        }
    }

    #[must_use]
    pub fn with_backoff(mut self, min: Duration, max: Duration) -> Self {
        self.min_backoff = min;
        self.max_backoff = max.max(min);
        self
    }

    #[must_use]
    pub fn with_max_text_bytes(mut self, max_text_bytes: usize) -> Self {
        self.max_text_bytes = max_text_bytes;
        self
    }

    /// Process one response. The callback is invoked only for a new,
    /// validated owner direct message, after journal commit succeeds.
    pub async fn poll_once<F, Fut>(&self, mut on_message: F) -> Result<usize, PollerError>
    where
        F: FnMut(crate::inbound::InboundMessage) -> Fut,
        Fut: Future<Output = Result<(), PollerError>>,
    {
        let cursor = self.journal.cursor(&self.binding_id).await?;
        let updates = tokio::select! {
            _ = self.cancel.cancelled() => return Ok(0),
            result = self.api.updates(&self.credentials, &cursor) => result?,
        };
        let mut delivered = 0;
        for raw in updates.messages {
            let Ok(message) = crate::inbound::parse_inbound_with_limit(
                &raw,
                &self.credentials,
                self.max_text_bytes,
            ) else {
                // Invalid/non-owner messages are consumed from the upstream
                // cursor but never reach the local session.
                continue;
            };
            let committed = self
                .journal
                .commit_inbound(&self.binding_id, None, &message)
                .await?;
            if !committed {
                continue;
            }
            on_message(message.clone()).await?;
            self.journal.mark_inbound_delivered(&message.id).await?;
            delivered += 1;
        }
        // Cursor advancement is separate from inbox commit for batches that
        // contain no valid messages, but both happen only after a valid JSON
        // response has been accepted.
        if let Some(next) = updates.cursor {
            self.journal.set_cursor(&self.binding_id, &next).await?;
        }
        Ok(delivered)
    }

    /// Run until cancelled. Credential expiry is terminal and is returned to
    /// the supervisor so it can request a fresh QR login.
    pub async fn run<F, Fut>(&self, mut on_message: F) -> Result<(), PollerError>
    where
        F: FnMut(crate::inbound::InboundMessage) -> Fut,
        Fut: Future<Output = Result<(), PollerError>>,
    {
        let mut backoff = self.min_backoff;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(());
            }
            match self.poll_once(&mut on_message).await {
                Ok(_) => backoff = self.min_backoff,
                Err(PollerError::Weixin(WeixinError::CredentialsExpired)) => {
                    return Err(PollerError::Weixin(WeixinError::CredentialsExpired));
                }
                Err(error) => {
                    tracing::warn!(error = %error, "wechat poll failed; backing off");
                    tokio::select! {
                        _ = self.cancel.cancelled() => return Ok(()),
                        _ = sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.max_backoff);
                }
            }
        }
    }
}
