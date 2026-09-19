use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::api::{WeixinApi, WeixinError};
use crate::journal::{Journal, JournalError, OutboxRecord, OutboxStatus};
use crate::types::{Credentials, SendItem, SendMessage, TextItem, split_text_with_limits};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundRole {
    VsCodeUser,
    Agent,
}

impl OutboundRole {
    fn label(self) -> &'static str {
        match self {
            Self::VsCodeUser => "user",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("reply context is not available yet")]
    WaitingForContext,
    #[error("outbound text is invalid: {0}")]
    InvalidText(String),
    #[error(transparent)]
    Weixin(#[from] WeixinError),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    Sent { parts: usize },
    WaitingForContext,
    Uncertain { sent_parts: usize },
}

/// Sends one logical reply as tracked UTF-8 chunks. A transport interruption
/// never triggers an automatic retry because the remote side may have
/// accepted the message already.
#[derive(Debug)]
pub struct OutboundSender<A, J> {
    api: Arc<A>,
    journal: Arc<J>,
    max_text_bytes: usize,
    max_chunk_bytes: usize,
}

impl<A, J> OutboundSender<A, J>
where
    A: WeixinApi,
    J: Journal,
{
    pub fn new(api: Arc<A>, journal: Arc<J>) -> Self {
        Self {
            api,
            journal,
            max_text_bytes: crate::types::MAX_TEXT_BYTES,
            max_chunk_bytes: crate::types::MAX_CHUNK_BYTES,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, max_text_bytes: usize, max_chunk_bytes: usize) -> Self {
        self.max_text_bytes = max_text_bytes;
        self.max_chunk_bytes = max_chunk_bytes;
        self
    }

    pub async fn send_text(
        &self,
        credentials: &Credentials,
        binding_id: &str,
        source_id: &str,
        role: OutboundRole,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<SendResult, OutboundError> {
        let context = context_token.filter(|value| !value.is_empty());
        if context.is_none() {
            let id = stable_id(source_id, role, 0);
            self.journal
                .append_outbox(OutboxRecord {
                    id,
                    binding_id: binding_id.to_owned(),
                    source_id: source_id.to_owned(),
                    role: role.label().to_owned(),
                    text: text.to_owned(),
                    context_token: String::new(),
                    status: OutboxStatus::WaitingForContext,
                    sent_parts: 0,
                })
                .await?;
            return Ok(SendResult::WaitingForContext);
        }
        if text.contains(&credentials.token) || text.contains(context.unwrap()) {
            return Err(OutboundError::InvalidText(
                "text contains transport credentials".into(),
            ));
        }
        let context = context.unwrap();
        for pending in self.journal.waiting_outbox(binding_id).await? {
            if pending.text.contains(&credentials.token) || pending.text.contains(context) {
                continue;
            }
            let Some(pending_role) = (match pending.role.as_str() {
                "user" => Some(OutboundRole::VsCodeUser),
                "agent" => Some(OutboundRole::Agent),
                _ => None,
            }) else {
                continue;
            };
            self.journal
                .set_outbox_context(&pending.id, context)
                .await?;
            let _ = self
                .transmit(
                    credentials,
                    &pending.id,
                    &pending.source_id,
                    pending_role,
                    &pending.text,
                    context,
                )
                .await?;
        }
        let chunks = present_chunks(role, text, self.max_text_bytes, self.max_chunk_bytes)?;
        let root_id = stable_id(source_id, role, usize::MAX);
        self.journal
            .append_outbox(OutboxRecord {
                id: root_id.clone(),
                binding_id: binding_id.to_owned(),
                source_id: source_id.to_owned(),
                role: role.label().to_owned(),
                text: text.to_owned(),
                context_token: context.to_owned(),
                status: OutboxStatus::Sending,
                sent_parts: 0,
            })
            .await?;

        self.transmit_chunks(credentials, &root_id, source_id, role, context, &chunks)
            .await
    }

    async fn transmit(
        &self,
        credentials: &Credentials,
        root_id: &str,
        source_id: &str,
        role: OutboundRole,
        text: &str,
        context: &str,
    ) -> Result<SendResult, OutboundError> {
        let chunks = present_chunks(role, text, self.max_text_bytes, self.max_chunk_bytes)?;
        self.transmit_chunks(credentials, root_id, source_id, role, context, &chunks)
            .await
    }

    async fn transmit_chunks(
        &self,
        credentials: &Credentials,
        root_id: &str,
        source_id: &str,
        role: OutboundRole,
        context: &str,
        chunks: &[String],
    ) -> Result<SendResult, OutboundError> {
        for (index, chunk) in chunks.iter().enumerate() {
            self.journal
                .update_outbox(&root_id, OutboxStatus::Sending, index)
                .await?;
            let message = SendMessage {
                from_user_id: String::new(),
                to_user_id: credentials.owner_id.clone(),
                client_id: stable_id(source_id, role, index),
                message_type: 2,
                message_state: 2,
                context_token: context.to_owned(),
                item_list: vec![SendItem {
                    r#type: 1,
                    text_item: TextItem {
                        text: chunk.clone(),
                    },
                }],
            };
            if self.api.send(credentials, &message).await.is_err() {
                self.journal
                    .update_outbox(&root_id, OutboxStatus::Uncertain, index)
                    .await?;
                return Ok(SendResult::Uncertain { sent_parts: index });
            }
        }
        self.journal
            .update_outbox(&root_id, OutboxStatus::Sent, chunks.len())
            .await?;
        Ok(SendResult::Sent {
            parts: chunks.len(),
        })
    }
}

fn stable_id(source_id: &str, role: OutboundRole, part: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(source_id.as_bytes());
    hash.update([0]);
    hash.update(role.label().as_bytes());
    hash.update([0]);
    hash.update(part.to_be_bytes());
    format!("wechat-{:x}", hash.finalize())
}

fn present_chunks(
    role: OutboundRole,
    text: &str,
    max_text_bytes: usize,
    max_chunk_bytes: usize,
) -> Result<Vec<String>, OutboundError> {
    if role == OutboundRole::Agent {
        return split_text_with_limits(text, max_text_bytes, max_chunk_bytes)
            .map_err(OutboundError::InvalidText);
    }
    let prefix = "[VS Code User]\n";
    let reserve = "[VS Code User 16/16]\n".len();
    let parts = split_text_with_limits(
        text,
        max_text_bytes,
        max_chunk_bytes.saturating_sub(reserve),
    )
    .map_err(OutboundError::InvalidText)?;
    if parts.len() == 1 {
        return Ok(vec![format!("{prefix}{}", parts[0])]);
    }
    let total = parts.len();
    Ok(parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| format!("[VS Code User {}/{}]\n{part}", index + 1, total))
        .collect())
}
