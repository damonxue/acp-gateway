//! ACP bridge for IDE-owned sessions.
//!
//! ACP bridge（IDE 自有 Session 的桥接进程）。
//!
//! `agent-gateway acp-bridge` is spawned by an IDE as if it were an ACP
//! agent. It then starts the configured real agent and proxies JSON-RPC in
//! both directions while mirroring the session into the daemon's loopback
//! `/bridge` socket.
//!
//! `agent-gateway acp-bridge` 会被 IDE 当作 ACP Agent 进程启动。它再启动配置中的真实
//! Agent，并在两端之间代理 JSON-RPC，同时把 Session 镜像到守护进程本机 `/bridge`
//! WebSocket。
//!
//! The bridge deliberately supports one IDE session per process. That matches
//! the daemon's bridge socket contract and keeps lifecycle semantics crisp:
//! when the IDE process connection closes, the mirrored session detaches and
//! history is retained.
//!
//! 本桥接进程有意只支持单个 IDE Session。这与 daemon 端 bridge socket 的契约一致，
//! 也让生命周期语义清晰：IDE 连接关闭时，镜像 Session 断开，历史保留。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentChunk, NewSessionRequest, NewSessionResponse, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionId as AcpSessionId, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, Dispatch, JsonRpcMessage,
    JsonRpcResponse, Responder, Stdio,
};
use futures::{SinkExt, StreamExt};
use gateway_core::agent::AgentDescriptor;
use gateway_core::bridge::{BRIDGE_PATH, BridgeMessage, DaemonMessage};
use gateway_core::error::{GatewayError, Result};
use gateway_core::event::EventType;
use gateway_core::ids::PermissionId;
use serde_json::json;
use tokio::sync::{RwLock, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::mapper;

/// Remote-control policy for IDE-owned sessions.
///
/// IDE Session 的远程控制策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeControlMode {
    /// Remote clients may submit prompts, cancel turns and answer permissions.
    /// 远程客户端可以发送 prompt、取消回合、回答权限请求。
    Full,
    /// Remote clients may watch only; daemon commands are ignored.
    /// 远程客户端只能观察；来自 daemon 的控制命令会被忽略。
    ReadOnly,
}

/// Configuration for one bridge process.
///
/// 单个桥接进程的配置。
#[derive(Clone, Debug)]
pub struct AcpBridgeConfig {
    /// The real agent this bridge proxies to.
    /// 本桥代理到的真实 Agent。
    pub agent: AgentDescriptor,
    /// Daemon bridge WebSocket URL, for example `ws://127.0.0.1:48100/bridge`.
    /// daemon bridge WebSocket 地址，例如 `ws://127.0.0.1:48100/bridge`。
    pub daemon_url: String,
    /// Whether the daemon may drive the IDE-owned session.
    /// daemon 是否可以驱动 IDE 自有 Session。
    pub control_mode: BridgeControlMode,
    /// Queue depth for messages sent to the daemon.
    /// 发往 daemon 的消息队列深度。
    pub daemon_queue: usize,
}

impl AcpBridgeConfig {
    /// Build a bridge config from a daemon bind address.
    ///
    /// 用 daemon 监听地址构造 bridge 配置。
    #[must_use]
    pub fn new(agent: AgentDescriptor, bind: std::net::SocketAddr) -> Self {
        Self {
            agent,
            daemon_url: format!("ws://{bind}{BRIDGE_PATH}"),
            control_mode: BridgeControlMode::Full,
            daemon_queue: 256,
        }
    }
}

/// Run the bridge on stdin/stdout.
///
/// 在 stdin/stdout 上运行桥接进程。
///
/// # Errors
/// Returns when the ACP connection fails before a clean close.
pub async fn run_bridge(config: AcpBridgeConfig) -> Result<()> {
    let daemon = DaemonLink::connect(config.daemon_url.clone(), config.daemon_queue).await;
    let commands = daemon.commands;
    let reporter = daemon.reporter;
    let state = Arc::new(BridgeState::new(config.agent.clone(), reporter));

    let from_ide = Arc::clone(&state);
    Agent
        .builder()
        .name(format!("agent-gateway:{}", config.agent.id))
        .on_receive_dispatch(
            async move |dispatch: Dispatch, cx: ConnectionTo<Client>| {
                handle_from_ide(dispatch, cx, Arc::clone(&from_ide)).await
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(Stdio::new(), async move |ide: ConnectionTo<Client>| {
            let from_agent = Arc::clone(&state);
            let ide_for_agent = ide.clone();
            let agent_side = Client.builder().on_receive_dispatch(
                async move |dispatch: Dispatch, cx: ConnectionTo<Agent>| {
                    handle_from_agent(dispatch, cx, ide_for_agent.clone(), Arc::clone(&from_agent))
                        .await
                },
                agent_client_protocol::on_receive_dispatch!(),
            );

            let transport = AcpAgent::new(
                AcpAgentConfig::new(&config.agent.command)
                    .args(config.agent.args.clone())
                    .envs(config.agent.env.clone()),
            );
            let real_agent = ide.spawn_connection(agent_side, transport)?;
            state.attach_real_agent(real_agent.clone()).await;

            tokio::spawn(command_loop(
                commands,
                real_agent.clone(),
                ide.clone(),
                Arc::clone(&state),
                config.control_mode,
            ));

            tokio::select! {
                () = ide.incoming_closed() => {
                    state.detach("the IDE connection closed").await;
                }
                () = real_agent.incoming_closed() => {
                    state.detach("the agent connection closed").await;
                }
            }

            Ok(())
        })
        .await
        .map_err(|error| GatewayError::AgentUnavailable(error.to_string()))
}

async fn handle_from_ide(
    dispatch: Dispatch,
    _ide: ConnectionTo<Client>,
    state: Arc<BridgeState>,
) -> std::result::Result<agent_client_protocol::Handled<Dispatch>, agent_client_protocol::Error> {
    if let Some(request) = parse_dispatch::<NewSessionRequest>(&dispatch)? {
        state.remember_new_session(request).await;
    }

    if let Some(request) = parse_dispatch::<PromptRequest>(&dispatch)? {
        state
            .emit_event(
                EventType::UserMessage,
                json!({ "content": json_value(&request.prompt) }),
            )
            .await;

        // A prompt that originates in the IDE is already an ACP request. The
        // old raw proxy path forwarded its response back to Zed, but never
        // observed that response in the gateway event stream. That left
        // downstream adapters waiting forever for `session_completed` after a
        // Zed-originated turn, even though Zed itself received the answer.
        // Consume the forwarded response here so we can mirror the same
        // completion/failure event emitted by daemon-originated prompts while
        // still returning the exact ACP response to the IDE.
        let Dispatch::Request(_, responder) = dispatch else {
            return Err(agent_client_protocol::Error::internal_error()
                .data("prompt dispatch was not a request"));
        };
        let real_agent = state.real_agent().await?;
        let state_for_result = Arc::clone(&state);
        let cancellation = responder.cancellation();
        real_agent
            .send_request(request)
            .forward_cancellation_from(cancellation)
            .on_receiving_result(async move |result| {
                let draft = match &result {
                    Ok(response) => mapper::turn_completed_event(response.stop_reason),
                    Err(error) => mapper::failure_event(error, false),
                };
                state_for_result
                    .emit_event(draft.event_type, draft.payload)
                    .await;
                responder.respond_with_result(result.map(|response| json_value(&response)))
            })?;
        return Ok(agent_client_protocol::Handled::Yes);
    }

    if parse_dispatch::<CancelNotification>(&dispatch)?.is_some() {
        state.cancel_pending_permissions().await;
    }

    let real_agent = state.real_agent().await?;
    real_agent.send_proxied_message(dispatch)?;
    Ok(agent_client_protocol::Handled::Yes)
}

async fn handle_from_agent(
    dispatch: Dispatch,
    _agent: ConnectionTo<Agent>,
    ide: ConnectionTo<Client>,
    state: Arc<BridgeState>,
) -> std::result::Result<agent_client_protocol::Handled<Dispatch>, agent_client_protocol::Error> {
    if let Some(response) = parse_new_session_response(&dispatch)? {
        state.adopt(response.session_id.clone()).await;
    }

    if let Some(notification) = parse_dispatch::<SessionNotification>(&dispatch)? {
        let draft = mapper::event_for_update(&notification.update);
        state.emit_event(draft.event_type, draft.payload).await;
    }

    match dispatch {
        Dispatch::Request(message, responder)
            if RequestPermissionRequest::matches_method(message.method()) =>
        {
            let request =
                RequestPermissionRequest::parse_message(message.method(), message.params())?;
            let responder = responder.cast();
            let cancellation = responder.cancellation();
            let id = PermissionId::generate();
            let (domain, draft) =
                mapper::permission_request(id.clone(), &request, chrono::Utc::now());
            state.insert_permission(id.clone(), domain, responder).await;

            ide.send_request(request)
                .forward_cancellation_from(cancellation)
                .on_receiving_result({
                    let state = Arc::clone(&state);
                    async move |result| {
                        let outcome = match result {
                            Ok(response) => response.outcome,
                            Err(error) => {
                                debug!(%error, permission_id = %id, "IDE permission request failed");
                                RequestPermissionOutcome::Cancelled
                            }
                        };
                        state.resolve_permission_outcome(&id, outcome).await;
                        Ok(())
                    }
                })?;

            state.emit_event(draft.event_type, draft.payload).await;
            Ok(agent_client_protocol::Handled::Yes)
        }
        other => {
            ide.send_proxied_message(other)?;
            Ok(agent_client_protocol::Handled::Yes)
        }
    }
}

async fn command_loop(
    mut commands: mpsc::Receiver<DaemonMessage>,
    real_agent: ConnectionTo<Agent>,
    ide: ConnectionTo<Client>,
    state: Arc<BridgeState>,
    mode: BridgeControlMode,
) {
    while let Some(command) = commands.recv().await {
        match command {
            DaemonMessage::Adopted { session_id } => {
                state.mark_adopted(session_id.to_string());
            }
            DaemonMessage::Prompt { blocks } => {
                if mode == BridgeControlMode::ReadOnly {
                    continue;
                }
                let Some(acp_session_id) = state.acp_session_id().await else {
                    warn!("daemon sent a bridge prompt before the ACP session was ready");
                    continue;
                };
                let request = mapper::prompt_request(&acp_session_id, &blocks);
                for block in &request.prompt {
                    if let Err(error) = ide.send_notification(SessionNotification::new(
                        acp_session_id.clone(),
                        SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
                    )) {
                        warn!(%error, "cannot mirror remote user message to the IDE");
                    }
                }
                let state = Arc::clone(&state);
                if let Err(error) =
                    real_agent
                        .send_request(request)
                        .on_receiving_result(async move |result| {
                            let draft = match result {
                                Ok(response) => mapper::turn_completed_event(response.stop_reason),
                                Err(error) => mapper::failure_event(error, false),
                            };
                            state.emit_event(draft.event_type, draft.payload).await;
                            Ok(())
                        })
                {
                    warn!(%error, "cannot forward daemon prompt into IDE bridge");
                }
            }
            DaemonMessage::Cancel => {
                if mode == BridgeControlMode::ReadOnly {
                    continue;
                }
                let Some(acp_session_id) = state.acp_session_id().await else {
                    continue;
                };
                if let Err(error) =
                    real_agent.send_notification(CancelNotification::new(acp_session_id))
                {
                    warn!(%error, "cannot cancel IDE bridge session");
                }
                state.cancel_pending_permissions().await;
            }
            DaemonMessage::Permission {
                permission_id,
                decision,
            } => {
                if mode == BridgeControlMode::ReadOnly {
                    continue;
                }
                state
                    .resolve_permission_decision(&permission_id, decision)
                    .await;
            }
            DaemonMessage::Error { message } => {
                warn!(%message, "daemon bridge error");
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct BridgeState {
    agent: AgentDescriptor,
    reporter: DaemonReporter,
    real_agent: RwLock<Option<ConnectionTo<Agent>>>,
    new_session: RwLock<Option<NewSessionRequest>>,
    acp_session_id: RwLock<Option<AcpSessionId>>,
    adopted: AtomicBool,
    detached: AtomicBool,
    permissions: Mutex<HashMap<PermissionId, PendingPermission>>,
}

impl BridgeState {
    fn new(agent: AgentDescriptor, reporter: DaemonReporter) -> Self {
        Self {
            agent,
            reporter,
            real_agent: RwLock::new(None),
            new_session: RwLock::new(None),
            acp_session_id: RwLock::new(None),
            adopted: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            permissions: Mutex::new(HashMap::new()),
        }
    }

    async fn attach_real_agent(&self, connection: ConnectionTo<Agent>) {
        *self.real_agent.write().await = Some(connection);
    }

    async fn real_agent(
        &self,
    ) -> std::result::Result<ConnectionTo<Agent>, agent_client_protocol::Error> {
        for _ in 0..100 {
            if let Some(connection) = self.real_agent.read().await.clone() {
                return Ok(connection);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(agent_client_protocol::Error::internal_error()
            .data("real agent connection was not ready"))
    }

    async fn remember_new_session(&self, request: NewSessionRequest) {
        *self.new_session.write().await = Some(request);
    }

    async fn adopt(&self, acp_session_id: AcpSessionId) {
        if self.adopted.swap(true, Ordering::AcqRel) {
            return;
        }

        *self.acp_session_id.write().await = Some(acp_session_id.clone());
        let request = self.new_session.read().await.clone();
        let cwd = request
            .as_ref()
            .map_or_else(current_dir_or_home, |request| request.cwd.clone());
        let workspace = cwd.clone();
        self.reporter
            .send(BridgeMessage::Adopt {
                agent_id: self.agent.id.clone(),
                agent_name: self.agent.name.clone(),
                acp_session_id: acp_session_id.to_string(),
                workspace,
                cwd,
            })
            .await;
        info!(agent = %self.agent.id, acp_session_id = %acp_session_id, "IDE ACP session adopted");
    }

    fn mark_adopted(&self, session_id: String) {
        debug!(%session_id, "daemon accepted IDE bridge session");
    }

    async fn acp_session_id(&self) -> Option<AcpSessionId> {
        self.acp_session_id.read().await.clone()
    }

    async fn emit_event(&self, event_type: EventType, payload: serde_json::Value) {
        self.reporter
            .send(BridgeMessage::Event {
                event_type,
                payload,
            })
            .await;
    }

    async fn detach(&self, reason: &str) {
        if self.detached.swap(true, Ordering::AcqRel) {
            return;
        }
        self.reporter
            .send(BridgeMessage::Detach {
                reason: reason.to_owned(),
            })
            .await;
    }

    async fn insert_permission(
        &self,
        id: PermissionId,
        request: gateway_core::permission::PermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) {
        self.permissions
            .lock()
            .expect("permission map")
            .insert(id, PendingPermission { request, responder });
    }

    async fn resolve_permission_decision(
        &self,
        id: &PermissionId,
        decision: gateway_core::permission::PermissionDecision,
    ) {
        let pending = self.permissions.lock().expect("permission map").remove(id);
        let Some(pending) = pending else {
            debug!(permission_id = %id, "daemon answered an already-resolved permission");
            return;
        };
        let outcome = mapper::outcome_for(&pending.request, &decision);
        self.finish_permission(id, pending.responder, outcome).await;
    }

    async fn resolve_permission_outcome(
        &self,
        id: &PermissionId,
        outcome: RequestPermissionOutcome,
    ) {
        let pending = self.permissions.lock().expect("permission map").remove(id);
        let Some(pending) = pending else {
            debug!(permission_id = %id, "IDE answered an already-resolved permission");
            return;
        };
        self.finish_permission(id, pending.responder, outcome).await;
    }

    async fn finish_permission(
        &self,
        id: &PermissionId,
        responder: Responder<RequestPermissionResponse>,
        outcome: RequestPermissionOutcome,
    ) {
        responder
            .respond(RequestPermissionResponse::new(outcome.clone()))
            .ok();
        let draft = mapper::permission_response_event(id, &outcome);
        self.emit_event(draft.event_type, draft.payload).await;
    }

    async fn cancel_pending_permissions(&self) {
        let cancelled: BTreeMap<PermissionId, PendingPermission> = self
            .permissions
            .lock()
            .expect("permission map")
            .drain()
            .collect();
        for (id, pending) in cancelled {
            self.finish_permission(&id, pending.responder, RequestPermissionOutcome::Cancelled)
                .await;
        }
    }
}

struct PendingPermission {
    request: gateway_core::permission::PermissionRequest,
    responder: Responder<RequestPermissionResponse>,
}

impl std::fmt::Debug for PendingPermission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingPermission")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
struct DaemonReporter {
    outgoing: mpsc::Sender<BridgeMessage>,
}

impl DaemonReporter {
    async fn send(&self, message: BridgeMessage) {
        self.outgoing.send(message).await.ok();
    }
}

#[derive(Debug)]
struct DaemonLink {
    reporter: DaemonReporter,
    commands: mpsc::Receiver<DaemonMessage>,
}

impl DaemonLink {
    async fn connect(url: String, queue: usize) -> Self {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(queue);
        let (incoming_tx, incoming_rx) = mpsc::channel(queue);
        tokio::spawn(daemon_supervisor(url, outgoing_rx, incoming_tx));
        Self {
            reporter: DaemonReporter {
                outgoing: outgoing_tx,
            },
            commands: incoming_rx,
        }
    }
}

/// Keep the IDE bridge connected to the daemon when the daemon is restarted or
/// starts after Zed. Bridge messages are queued while the socket is down, and
/// the latest `adopt` announcement is replayed on every new connection so the
/// daemon can reattach the retained Gateway session.
async fn daemon_supervisor(
    url: String,
    mut outgoing: mpsc::Receiver<BridgeMessage>,
    incoming: mpsc::Sender<DaemonMessage>,
) {
    let mut pending = VecDeque::new();
    let mut last_adopt: Option<BridgeMessage> = None;
    loop {
        while let Ok(message) = outgoing.try_recv() {
            if matches!(message, BridgeMessage::Adopt { .. }) {
                last_adopt = Some(message.clone());
            }
            pending.push_back(message);
        }

        match connect_async(&url).await {
            Ok((socket, _)) => {
                info!(%url, "connected IDE bridge to gateway daemon");
                let (mut sink, mut stream) = socket.split();

                if let Some(adopt) = last_adopt.clone() {
                    let Ok(text) = serde_json::to_string(&adopt) else {
                        continue;
                    };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    if let Some(index) = pending
                        .iter()
                        .position(|message| matches!(message, BridgeMessage::Adopt { .. }))
                    {
                        pending.remove(index);
                    }
                }

                'connected: loop {
                    while let Some(message) = pending.pop_front() {
                        let Ok(text) = serde_json::to_string(&message) else {
                            continue;
                        };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            pending.push_front(message);
                            break 'connected;
                        }
                    }

                    tokio::select! {
                        message = outgoing.recv() => {
                            let Some(message) = message else { return };
                            if matches!(message, BridgeMessage::Adopt { .. }) {
                                last_adopt = Some(message.clone());
                            }
                            let Ok(text) = serde_json::to_string(&message) else { continue };
                            if sink.send(Message::Text(text.into())).await.is_err() {
                                pending.push_back(message);
                                break 'connected;
                            }
                        }
                        frame = stream.next() => {
                            let Some(Ok(frame)) = frame else { break 'connected };
                            let Message::Text(text) = frame else {
                                if matches!(frame, Message::Close(_)) {
                                    break 'connected;
                                }
                                continue;
                            };
                            match serde_json::from_str::<DaemonMessage>(&text) {
                                Ok(message) => {
                                    if incoming.send(message).await.is_err() {
                                        return;
                                    }
                                }
                                Err(error) => warn!(%error, "unparsable daemon bridge message"),
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%url, %error, "gateway daemon unavailable; retrying IDE bridge connection");
            }
        }

        while let Ok(message) = outgoing.try_recv() {
            if matches!(message, BridgeMessage::Adopt { .. }) {
                last_adopt = Some(message.clone());
            }
            pending.push_back(message);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn parse_dispatch<M>(
    dispatch: &Dispatch,
) -> std::result::Result<Option<M>, agent_client_protocol::Error>
where
    M: JsonRpcMessage,
{
    let Some(message) = dispatch.message() else {
        return Ok(None);
    };
    if M::matches_method(message.method()) {
        return M::parse_message(message.method(), message.params()).map(Some);
    }
    Ok(None)
}

fn parse_new_session_response(
    dispatch: &Dispatch,
) -> std::result::Result<Option<NewSessionResponse>, agent_client_protocol::Error> {
    let Dispatch::Response(Ok(value), router) = dispatch else {
        return Ok(None);
    };
    if NewSessionRequest::matches_method(router.method()) {
        return NewSessionResponse::from_value(router.method(), value.clone()).map(Some);
    }
    Ok(None)
}

fn json_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn current_dir_or_home() -> PathBuf {
    std::env::current_dir()
        .ok()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}
