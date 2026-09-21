//! The daemon's end of the IDE bridge.
//!
//! An IDE spawns `agent-gateway acp-bridge` as its "agent"; the bridge proxies
//! ACP to the real agent and connects here. This module turns that socket into
//! an ordinary [`AgentSessionHandle`], which is the whole trick: once the
//! session is adopted, nothing above [`gateway_core::SessionManager`] can tell
//! an IDE-owned session from one the gateway launched itself, so the remote
//! protocol, the event log and the phone UI work on it unchanged.
//!
//! ```text
//!   bridge ──adopt──► adopt_bridge_session()  ──► session with origin=ide_bridge
//!   bridge ──event──► append_event()          ──► the same event log
//!   bridge ◄─prompt── BridgeHandle::submit_prompt (from a phone)
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use gateway_core::agent::{AcpSessionInfo, AgentSessionHandle, PromptBlock};
use gateway_core::bridge::{BridgeMessage, DaemonMessage};
use gateway_core::error::{GatewayError, Result};
use gateway_core::event::EventDraft;
use gateway_core::ids::{PermissionId, SessionId};
use gateway_core::manager::AdoptSessionSpec;
use gateway_core::permission::PermissionDecision;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::error::ApiError;
use crate::state::{Access, AppState};

/// Depth of the command queue toward one bridge.
const COMMAND_QUEUE: usize = 64;

/// `GET /bridge` — loopback only.
///
/// The bridge runs on the same machine by construction (the IDE spawned it), so
/// this endpoint is not exposed through the tunnel: a remote caller must not be
/// able to inject events into a session's log.
///
/// # Errors
/// 403 when called from anywhere but loopback.
pub async fn socket(
    access: Access,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    access.require_local()?;
    Ok(upgrade.on_upgrade(move |socket| serve(state, socket)))
}

/// Drive one bridge connection until it closes.
async fn serve(state: AppState, socket: WebSocket) {
    let (mut sink, mut incoming) = socket.split();
    let (commands_tx, mut commands_rx) = mpsc::channel::<DaemonMessage>(COMMAND_QUEUE);
    let alive = Arc::new(AtomicBool::new(true));
    let refresh_waiters = Arc::new(std::sync::Mutex::new(HashMap::<
        String,
        oneshot::Sender<Result<Vec<AcpSessionInfo>>>,
    >::new()));

    let writer = tokio::spawn(async move {
        while let Some(command) = commands_rx.recv().await {
            let Ok(text) = serde_json::to_string(&command) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        sink.close().await.ok();
    });

    let mut session_id: Option<SessionId> = None;

    while let Some(Ok(message)) = incoming.next().await {
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let parsed = match serde_json::from_str::<BridgeMessage>(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(%error, "unparsable bridge message");
                continue;
            }
        };

        match parsed {
            BridgeMessage::Adopt {
                agent_id,
                agent_name,
                acp_session_id,
                workspace,
                cwd,
            } => {
                if session_id.is_some() {
                    send(
                        &commands_tx,
                        DaemonMessage::Error {
                            message: "this bridge already adopted a session".to_owned(),
                        },
                    )
                    .await;
                    continue;
                }
                let handle = Arc::new(BridgeHandle {
                    commands: commands_tx.clone(),
                    alive: Arc::clone(&alive),
                    refresh_waiters: Arc::clone(&refresh_waiters),
                });
                let adopted = state
                    .manager()
                    .adopt_bridge_session(AdoptSessionSpec {
                        agent_id,
                        agent_name,
                        acp_session_id,
                        workspace,
                        cwd,
                        handle,
                    })
                    .await;
                match adopted {
                    Ok(session) => {
                        info!(session_id = %session.id, "adopted an IDE bridge session");
                        session_id = Some(session.id.clone());
                        send(
                            &commands_tx,
                            DaemonMessage::Adopted {
                                session_id: session.id,
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        warn!(%error, "cannot adopt a bridge session");
                        send(
                            &commands_tx,
                            DaemonMessage::Error {
                                message: error.to_string(),
                            },
                        )
                        .await;
                    }
                }
            }

            BridgeMessage::Event {
                event_type,
                payload,
            } => {
                let Some(id) = session_id.clone() else {
                    warn!("bridge sent an event before adopting a session");
                    continue;
                };
                if let Err(error) = state
                    .manager()
                    .append_event(&id, EventDraft::from_value(event_type, payload))
                    .await
                {
                    warn!(%error, "cannot record a bridge event");
                }
            }

            BridgeMessage::Detach { reason } => {
                if let Some(id) = session_id.clone() {
                    detach(&state, &id, &reason).await;
                    session_id = None;
                }
                break;
            }

            BridgeMessage::SessionList {
                request_id,
                sessions,
                error,
            } => {
                let waiter = refresh_waiters
                    .lock()
                    .expect("refresh waiter lock")
                    .remove(&request_id);
                if let Some(waiter) = waiter {
                    let result = error.map_or(Ok(sessions), |message| {
                        Err(GatewayError::AgentUnavailable(message))
                    });
                    let _ = waiter.send(result);
                }
            }

            _ => {
                warn!("ignoring an unknown bridge message");
            }
        }
    }

    // The socket died without a `detach`: the IDE quit, or the bridge crashed.
    alive.store(false, Ordering::Release);
    if let Some(id) = session_id {
        detach(&state, &id, "the IDE bridge disconnected").await;
    }
    writer.abort();
}

async fn detach(state: &AppState, session_id: &SessionId, reason: &str) {
    if let Err(error) = state.manager().detach_session(session_id, reason).await {
        warn!(%error, "cannot detach a bridge session");
    }
}

async fn send(commands: &mpsc::Sender<DaemonMessage>, message: DaemonMessage) {
    commands.send(message).await.ok();
}

/// Drives an IDE-owned session through the bridge's socket.
#[derive(Debug)]
struct BridgeHandle {
    commands: mpsc::Sender<DaemonMessage>,
    alive: Arc<AtomicBool>,
    refresh_waiters:
        Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Result<Vec<AcpSessionInfo>>>>>>,
}

impl BridgeHandle {
    async fn command(&self, message: DaemonMessage) -> Result<()> {
        self.commands
            .send(message)
            .await
            .map_err(|_| GatewayError::AgentUnavailable("the IDE bridge is gone".to_owned()))
    }
}

#[async_trait]
impl AgentSessionHandle for BridgeHandle {
    async fn submit_prompt(&self, blocks: Vec<PromptBlock>) -> Result<()> {
        self.command(DaemonMessage::Prompt { blocks }).await
    }

    async fn cancel(&self) -> Result<()> {
        self.command(DaemonMessage::Cancel).await
    }

    async fn resolve_permission(
        &self,
        permission_id: &PermissionId,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.command(DaemonMessage::Permission {
            permission_id: permission_id.clone(),
            decision,
        })
        .await
    }

    async fn shutdown(&self) -> Result<()> {
        // Closing a bridge session must not kill the IDE's agent: the developer
        // is still using it. Dropping the command channel just stops remote
        // control.
        self.alive.store(false, Ordering::Release);
        Ok(())
    }

    async fn refresh_sessions(&self) -> Result<Vec<AcpSessionInfo>> {
        let request_id = SessionId::generate().to_string();
        let (response, result) = oneshot::channel();
        self.refresh_waiters
            .lock()
            .expect("refresh waiter lock")
            .insert(request_id.clone(), response);
        if self
            .commands
            .send(DaemonMessage::RefreshSessions {
                request_id: request_id.clone(),
            })
            .await
            .is_err()
        {
            self.refresh_waiters
                .lock()
                .expect("refresh waiter lock")
                .remove(&request_id);
            return Err(GatewayError::AgentUnavailable(
                "the IDE bridge is gone".to_owned(),
            ));
        }
        result
            .await
            .map_err(|_| GatewayError::AgentUnavailable("agent refresh task ended".to_owned()))?
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.commands.is_closed()
    }
}
