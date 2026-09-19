//! Weixin Bot adapter.
//!
//! This crate deliberately does not depend on AHP. AHP is useful when the
//! adapter is a separate process; the embedded adapter calls SessionManager
//! directly and uses the same durable event semantics as the HTTP client.

#![forbid(unsafe_code)]

mod api;
mod bridge;
mod credential;
mod inbound;
mod journal;
mod login;
mod outbound;
mod poller;
mod status;
mod supervisor;
mod types;

pub use api::{HttpWeixinApi, WeixinApi, WeixinError, validate_origin};
pub use bridge::{Binding, BridgeError, WechatBridge};
pub use credential::{
    CredentialError, CredentialStore, KeychainCredentialStore, MemoryCredentialStore,
};
pub use inbound::{InboundMessage, parse_inbound, parse_inbound_with_limit};
pub use journal::{
    InboxRecord, Journal, JournalError, MemoryJournal, OutboxRecord, OutboxStatus, SqliteJournal,
};
pub use login::login;
pub use outbound::{OutboundError, OutboundRole, OutboundSender, SendResult};
pub use poller::{Poller, PollerError};
pub use status::WechatStatus;
pub use supervisor::WechatSupervisor;
pub use types::LoginPoll;
pub use types::{CLIENT_VERSION, DEFAULT_BASE, MAX_CHUNK_BYTES, MAX_TEXT_BYTES};
pub use types::{
    Credentials, QrChallenge, SendMessage, Updates, base_info, split_text, split_text_with_limits,
};
