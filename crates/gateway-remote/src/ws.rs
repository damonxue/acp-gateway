//! The WebSocket endpoints.
//!
//! ## The reconnect guarantee
//!
//! A phone that loses its network must be able to come back and see exactly
//! the events it missed — no gaps, no duplicates, and no re-running of a
//! prompt. Subscription is therefore ordered deliberately:
//!
//! ```text
//! 1. subscribe to the live broadcast   (nothing is missed from here on)
//! 2. replay everything after `after_seq` from the store
//! 3. forward live events whose seq is greater than the last replayed one
//! ```
//!
//! Doing it the other way round — replay first, then subscribe — would drop
//! every event produced in between. Step 3's `seq` comparison is what removes
//! the duplicates that step 1 makes possible.
//!
//! A subscriber that falls behind the broadcast buffer is *lagged*, never
//! blocking: it resynchronises through the same replay path, so a slow phone
//! can never stall an agent.

use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use gateway_ahp::serve_connection;
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::SessionId;
use gateway_core::manager::{CreateSessionSpec, SessionManager};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::error::ApiError;
use crate::http::agent_summaries;
use crate::protocol::{ClientMessage, Outgoing, PROTOCOL_VERSION, ServerMessage};
use crate::state::{Access, AppState};

/// Outgoing queue depth per connection.
///
/// Deep enough to absorb a burst of chunks, shallow enough that a dead client
/// is noticed instead of being buffered forever.
const SEND_QUEUE: usize = 512;

/// `GET /remote?ticket=…`
#[derive(Debug, Deserialize)]
pub struct RemoteQuery {
    /// Single-use ticket obtained from `POST /devices/{id}/ws-ticket`.
    #[serde(default)]
    pub ticket: Option<String>,
}

/// The remote (phone/browser) endpoint.
///
/// # Errors
/// 401/410 when the ticket is missing, expired or already used.
pub async fn remote_socket(
    access: Access,
    State(state): State<AppState>,
    Query(query): Query<RemoteQuery>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    let device = match query.ticket {
        Some(ticket) => Some(state.auth().redeem_ticket(&ticket).await?),
        // A ticket is mandatory for anything that is not a genuinely local
        // caller on a gateway configured to trust loopback.
        None if access.is_local() && state.trust_loopback() => None,
        None => {
            return Err(ApiError(GatewayError::AuthenticationFailed(
                "a ws ticket is required".into(),
            )));
        }
    };
    let label = device
        .as_ref()
        .map_or_else(|| "local".to_owned(), |device| device.id.to_string());
    info!(client = %label, "remote client connected");
    Ok(upgrade.on_upgrade(move |socket| async move {
        serve(state, socket, None, label).await;
    }))
}

/// `GET /ahp` — official AHP Host WebSocket endpoint.
///
/// AHP is a server protocol: clients such as VS Code, AHPX, or a WeChat
/// adapter connect here. Authentication is enforced by the surrounding
/// deployment (loopback, reverse proxy, or tunnel); the protocol itself then
/// starts with the mandatory `initialize` JSON-RPC request.
pub async fn ahp_socket(
    access: Access,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    // AHP has no HTTP-level ticket exchange. Keep the raw host endpoint
    // loopback-only until a deployment supplies an authenticated reverse
    // proxy or a transport handshake credential.
    if !access.is_local() {
        return Err(ApiError(GatewayError::AuthenticationFailed(
            "AHP endpoint requires a trusted local or authenticated proxy".into(),
        )));
    }
    Ok(upgrade.on_upgrade(move |socket| async move {
        serve_connection(socket, Arc::clone(state.manager())).await;
    }))
}

/// `GET /sessions/{id}/stream?after_seq=…` — the loopback convenience endpoint.
///
/// # Errors
/// 403 when called through a tunnel.
pub async fn session_socket(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<StreamQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    access.require_local()?;
    let session_id = crate::http::parse_session_id(&session_id)?;
    // Fail the upgrade for an unknown session instead of accepting a socket
    // that will immediately error.
    state.manager().get_session(&session_id).await?;
    Ok(upgrade.on_upgrade(move |socket| async move {
        serve(
            state,
            socket,
            Some((session_id, query.after_seq)),
            peer.to_string(),
        )
        .await;
    }))
}

/// Query for [`session_socket`].
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Replay cursor.
    #[serde(default)]
    pub after_seq: u64,
}

/// Drive one connection until it closes.
async fn serve(
    state: AppState,
    socket: WebSocket,
    initial: Option<(SessionId, u64)>,
    label: String,
) {
    let (mut sink, mut incoming) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Outgoing>(SEND_QUEUE);

    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            let text = match serde_json::to_string(&message) {
                Ok(text) => text,
                Err(error) => {
                    warn!(%error, "cannot serialise outgoing message");
                    continue;
                }
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        sink.close().await.ok();
    });

    let mut connection = Connection {
        state: state.clone(),
        outgoing: outgoing_tx,
        subscriptions: HashMap::new(),
    };

    connection.send_hello().await;
    if let Some((session_id, after_seq)) = initial {
        connection.subscribe(session_id, after_seq).await;
    }

    while let Some(Ok(message)) = incoming.next().await {
        match message {
            Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text) {
                Ok(command) => connection.handle(command).await,
                Err(error) => {
                    connection
                        .send_error(
                            &GatewayError::invalid_request(format!("unparsable message: {error}")),
                            None,
                        )
                        .await;
                }
            },
            Message::Close(_) => break,
            // Ping/Pong are handled by axum; binary frames are not part of the
            // protocol.
            _ => {}
        }
    }

    connection.shutdown();
    writer.abort();
    info!(client = %label, "remote client disconnected");
}

struct Connection {
    state: AppState,
    outgoing: mpsc::Sender<Outgoing>,
    subscriptions: HashMap<SessionId, JoinHandle<()>>,
}

impl Connection {
    async fn send(&self, message: impl Into<Outgoing>) {
        self.outgoing.send(message.into()).await.ok();
    }

    async fn send_error(&self, error: &GatewayError, session_id: Option<SessionId>) {
        self.send(ServerMessage::Error {
            code: error.code().to_owned(),
            message: error.to_string(),
            session_id,
        })
        .await;
    }

    async fn send_hello(&self) {
        let sessions = self
            .state
            .manager()
            .list_sessions()
            .await
            .unwrap_or_default();
        self.send(ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            machine: self.state.machine(),
            agents: agent_summaries(&self.state),
            sessions,
        })
        .await;
    }

    async fn handle(&mut self, command: ClientMessage) {
        let result = match command {
            ClientMessage::Subscribe {
                session_id,
                after_seq,
            } => {
                self.subscribe(session_id, after_seq.unwrap_or(0)).await;
                Ok(())
            }
            ClientMessage::Unsubscribe { session_id } => {
                if let Some(task) = self.subscriptions.remove(&session_id) {
                    task.abort();
                }
                Ok(())
            }
            ClientMessage::Prompt { session_id, prompt } => self
                .state
                .manager()
                .send_prompt(&session_id, prompt.into_blocks())
                .await
                .map_err(|error| (error, Some(session_id))),
            ClientMessage::Cancel { session_id } => self
                .state
                .manager()
                .cancel_session(&session_id)
                .await
                .map_err(|error| (error, Some(session_id))),
            ClientMessage::PermissionResponse { session_id, answer } => self
                .state
                .manager()
                .respond_permission(&session_id, &answer.request_id, answer.decision())
                .await
                .map_err(|error| (error, Some(session_id))),
            ClientMessage::ListSessions => match self.state.manager().list_sessions().await {
                Ok(sessions) => {
                    self.send(ServerMessage::SessionList { sessions }).await;
                    Ok(())
                }
                Err(error) => Err((error, None)),
            },
            ClientMessage::CreateSession {
                agent_id,
                workspace,
                cwd,
            } => {
                let spec = CreateSessionSpec {
                    agent_id: agent_id.into(),
                    workspace: workspace.into(),
                    cwd: cwd.map(Into::into),
                    additional_directories: Vec::new(),
                };
                match self.state.manager().create_session(spec).await {
                    Ok(session) => {
                        self.send(ServerMessage::SessionCreated { session }).await;
                        Ok(())
                    }
                    Err(error) => Err((error, None)),
                }
            }
            ClientMessage::CloseSession { session_id } => self
                .state
                .manager()
                .close_session(&session_id)
                .await
                .map_err(|error| (error, Some(session_id))),
            ClientMessage::Ping => {
                self.send(ServerMessage::Pong).await;
                Ok(())
            }
        };

        if let Err((error, session_id)) = result {
            self.send_error(&error, session_id).await;
        }
    }

    /// Start (or restart) a subscription.
    async fn subscribe(&mut self, session_id: SessionId, after_seq: u64) {
        if let Some(existing) = self.subscriptions.remove(&session_id) {
            existing.abort();
        }
        let manager = Arc::clone(self.state.manager());
        let outgoing = self.outgoing.clone();
        let id = session_id.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = stream_session(&manager, &id, after_seq, &outgoing).await {
                debug!(session_id = %id, %error, "subscription ended");
                outgoing
                    .send(Outgoing::Control(ServerMessage::Error {
                        code: error.code().to_owned(),
                        message: error.to_string(),
                        session_id: Some(id),
                    }))
                    .await
                    .ok();
            }
        });
        self.subscriptions.insert(session_id, task);
    }

    fn shutdown(&mut self) {
        for (_, task) in self.subscriptions.drain() {
            task.abort();
        }
    }
}

/// Replay then stream one session, forever, until the socket or session dies.
async fn stream_session(
    manager: &Arc<SessionManager>,
    session_id: &SessionId,
    after_seq: u64,
    outgoing: &mpsc::Sender<Outgoing>,
) -> Result<()> {
    // Step 1: subscribe first, so nothing produced during replay is lost.
    let mut live = manager.subscribe(session_id)?;

    // Step 2: replay the backlog.
    let mut last_seq = replay(manager, session_id, after_seq, outgoing).await?;

    let snapshot = manager.get_snapshot(session_id).await?;
    send(
        outgoing,
        ServerMessage::Subscribed {
            session_id: session_id.clone(),
            last_seq,
            snapshot,
        },
    )
    .await?;

    // Step 3: live, skipping anything replay already delivered.
    loop {
        match live.recv().await {
            Ok(event) => {
                if event.seq > last_seq {
                    last_seq = event.seq;
                    send(outgoing, (*event).clone()).await?;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                debug!(session_id = %session_id, missed, "subscriber lagged; resyncing from the store");
                last_seq = replay(manager, session_id, last_seq, outgoing).await?;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

/// Send every stored event after `from`, in pages. Returns the new cursor.
async fn replay(
    manager: &Arc<SessionManager>,
    session_id: &SessionId,
    from: u64,
    outgoing: &mpsc::Sender<Outgoing>,
) -> Result<u64> {
    let mut cursor = from;
    loop {
        let page = manager.replay_events(session_id, cursor, None).await?;
        if page.is_empty() {
            return Ok(cursor);
        }
        for event in page {
            cursor = event.seq;
            send(outgoing, event).await?;
        }
    }
}

async fn send(outgoing: &mpsc::Sender<Outgoing>, message: impl Into<Outgoing>) -> Result<()> {
    outgoing
        .send(message.into())
        .await
        .map_err(|_| GatewayError::transport("remote client disconnected"))
}
