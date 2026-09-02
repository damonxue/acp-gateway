//! [`AcpAgentRuntime`]: agents as gateway-owned child processes.
//!
//! ## Shape of a live session
//!
//! ```text
//!   SessionManager                         one tokio task per session
//!        │ submit_prompt/cancel/decide            │
//!        ▼                                        │
//!   AcpSessionHandle ──mpsc::Command──► command loop ──ACP──► agent process
//!        ▲                                        │
//!        └─────── EventSink ◄── mapper ◄── handlers ─┘
//! ```
//!
//! Two properties drive that shape:
//!
//! * **The command loop must never block on the agent.** A prompt turn can run
//!   for minutes, and a cancel arriving during it has to be delivered. So the
//!   prompt request is consumed with `on_receiving_result` (fire-and-report)
//!   instead of being awaited inline.
//! * **A permission request must not block the connection.** The ACP handler
//!   stores the `Responder` and returns immediately; the decision arrives
//!   later, from a phone, through the command loop. Nothing in between holds
//!   the JSON-RPC event loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, FileSystemCapabilities, Implementation,
    InitializeRequest, NewSessionRequest, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionId as AcpSessionId, SessionNotification, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, LineDirection, Responder,
};
use async_trait::async_trait;
use gateway_core::agent::{
    AgentRuntime, AgentSessionHandle, EventSink, LaunchRequest, LaunchedAgent, PromptBlock,
};
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::{PermissionId, SessionId};
use gateway_core::permission::{PermissionDecision, PermissionRequest};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::mapper;
use crate::workspace_fs::WorkspaceScope;

/// How long stderr/wire lines may be before they are truncated in logs.
const LOG_LINE_LIMIT: usize = 200;

/// Tunables for [`AcpAgentRuntime`].
#[derive(Clone, Debug)]
pub struct AcpRuntimeConfig {
    /// Client name reported to agents in `initialize`.
    pub client_name: String,
    /// Client version reported to agents in `initialize`.
    pub client_version: String,
    /// How long `initialize` + `session/new` may take before the launch fails.
    pub launch_timeout: Duration,
    /// Depth of the per-session command queue.
    pub command_buffer: usize,
}

impl Default for AcpRuntimeConfig {
    fn default() -> Self {
        Self {
            client_name: "agent-gateway".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            // Agents launched through `npx` download on first run; a short
            // timeout here would turn a slow first start into a hard failure.
            launch_timeout: Duration::from_secs(120),
            command_buffer: 64,
        }
    }
}

/// Launches ACP agents as child processes owned by the gateway.
#[derive(Clone, Debug, Default)]
pub struct AcpAgentRuntime {
    config: AcpRuntimeConfig,
}

impl AcpAgentRuntime {
    /// A runtime with the given settings.
    #[must_use]
    pub fn new(config: AcpRuntimeConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl AgentRuntime for AcpAgentRuntime {
    async fn launch(&self, request: LaunchRequest) -> Result<LaunchedAgent> {
        let (commands_tx, commands_rx) = mpsc::channel(self.config.command_buffer);
        let (ready_tx, ready_rx) = oneshot::channel();
        let alive = Arc::new(AtomicBool::new(true));

        let task = SessionTask {
            session_id: request.session_id.clone(),
            sink: Arc::clone(&request.sink),
            scope: WorkspaceScope::new(&request.cwd, &request.additional_directories),
            cwd: request.cwd,
            additional_directories: request.additional_directories,
            descriptor: request.descriptor,
            config: self.config.clone(),
            alive: Arc::clone(&alive),
        };
        tokio::spawn(task.run(commands_rx, ready_tx));

        let acp_session_id = match tokio::time::timeout(self.config.launch_timeout, ready_rx).await
        {
            Ok(Ok(Ok(id))) => id,
            Ok(Ok(Err(error))) => return Err(error),
            // The task ended before reporting: the process died during startup.
            Ok(Err(_)) => {
                return Err(GatewayError::AgentUnavailable(
                    "agent exited before the session was ready".to_owned(),
                ));
            }
            Err(_) => {
                alive.store(false, Ordering::Release);
                return Err(GatewayError::AgentUnavailable(format!(
                    "agent did not answer initialize within {:?}",
                    self.config.launch_timeout
                )));
            }
        };

        Ok(LaunchedAgent {
            acp_session_id: acp_session_id.to_string(),
            handle: Arc::new(AcpSessionHandle {
                commands: commands_tx,
                alive,
            }),
        })
    }
}

/// What the manager can ask a live ACP session to do.
#[derive(Debug)]
enum Command {
    Prompt(Vec<PromptBlock>),
    Cancel,
    Decide {
        id: PermissionId,
        decision: PermissionDecision,
    },
    Shutdown,
}

/// The manager-facing handle. Cheap, cloneable, and free of ACP types.
#[derive(Debug)]
struct AcpSessionHandle {
    commands: mpsc::Sender<Command>,
    alive: Arc<AtomicBool>,
}

impl AcpSessionHandle {
    async fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| GatewayError::AgentUnavailable("the agent connection is gone".to_owned()))
    }
}

#[async_trait]
impl AgentSessionHandle for AcpSessionHandle {
    async fn submit_prompt(&self, blocks: Vec<PromptBlock>) -> Result<()> {
        self.send(Command::Prompt(blocks)).await
    }

    async fn cancel(&self) -> Result<()> {
        self.send(Command::Cancel).await
    }

    async fn resolve_permission(
        &self,
        permission_id: &PermissionId,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.send(Command::Decide {
            id: permission_id.clone(),
            decision,
        })
        .await
    }

    async fn shutdown(&self) -> Result<()> {
        // Best effort: a dead connection is already shut down.
        self.commands.send(Command::Shutdown).await.ok();
        self.alive.store(false, Ordering::Release);
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.commands.is_closed()
    }
}

/// A permission request the agent is blocked on.
struct Pending {
    request: PermissionRequest,
    responder: Responder<RequestPermissionResponse>,
}

/// Outstanding permission requests, shared between the ACP handler that
/// receives them and the command loop that answers them.
type Pendings = Arc<Mutex<HashMap<PermissionId, Pending>>>;

/// Everything one session's task needs.
struct SessionTask {
    session_id: SessionId,
    sink: Arc<dyn EventSink>,
    scope: WorkspaceScope,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    descriptor: gateway_core::agent::AgentDescriptor,
    config: AcpRuntimeConfig,
    alive: Arc<AtomicBool>,
}

impl SessionTask {
    async fn run(
        self,
        commands: mpsc::Receiver<Command>,
        ready: oneshot::Sender<Result<AcpSessionId>>,
    ) {
        let session_id = self.session_id.clone();
        let sink = Arc::clone(&self.sink);
        let alive = Arc::clone(&self.alive);

        let outcome = self.connect(commands, ready).await;
        alive.store(false, Ordering::Release);

        match outcome {
            Ok(()) => info!(session_id = %session_id, "agent connection closed"),
            Err(error) => {
                warn!(session_id = %session_id, %error, "agent connection failed");
                sink.emit(&session_id, mapper::failure_event(error, true))
                    .await
                    .ok();
            }
        }
    }

    async fn connect(
        self,
        mut commands: mpsc::Receiver<Command>,
        ready: oneshot::Sender<Result<AcpSessionId>>,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let Self {
            session_id,
            sink,
            scope,
            cwd,
            additional_directories,
            descriptor,
            config,
            alive: _,
        } = self;

        let transport = AcpAgent::new(
            AcpAgentConfig::new(&descriptor.command)
                .args(descriptor.args.clone())
                .envs(descriptor.env.clone()),
        )
        .with_debug({
            let session_id = session_id.clone();
            move |line, direction| log_wire_line(&session_id, line, direction)
        });

        let pendings: Pendings = Arc::new(Mutex::new(HashMap::new()));

        let notification_sink = Arc::clone(&sink);
        let notification_session = session_id.clone();
        let permission_sink = Arc::clone(&sink);
        let permission_session = session_id.clone();
        let permission_pendings = Arc::clone(&pendings);
        let read_scope = scope.clone();
        let write_scope = scope;

        Client
            .builder()
            .name(config.client_name.clone())
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    let draft = mapper::event_for_update(&notification.update);
                    if let Err(error) = notification_sink
                        .emit(&notification_session, draft)
                        .await
                    {
                        warn!(session_id = %notification_session, %error, "dropping agent update");
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            _cx| {
                    let id = PermissionId::generate();
                    let (domain, draft) =
                        mapper::permission_request(id.clone(), &request, chrono::Utc::now());
                    // Registered before the event is published so a decision
                    // that arrives immediately always finds the responder.
                    permission_pendings.lock().expect("permission map").insert(
                        id.clone(),
                        Pending {
                            request: domain,
                            responder,
                        },
                    );
                    info!(session_id = %permission_session, permission_id = %id, "permission requested");
                    if let Err(error) = permission_sink.emit(&permission_session, draft).await {
                        // Nobody will ever see the request, so refuse it rather
                        // than leaving the agent blocked forever.
                        warn!(%error, "cannot record permission request");
                        if let Some(pending) =
                            permission_pendings.lock().expect("permission map").remove(&id)
                        {
                            pending
                                .responder
                                .respond(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Cancelled,
                                ))
                                .ok();
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ReadTextFileRequest,
                            responder: Responder<ReadTextFileResponse>,
                            _cx| {
                    match read_scope
                        .read_text(&request.path, request.line, request.limit)
                        .await
                    {
                        Ok(content) => responder.respond(ReadTextFileResponse::new(content)),
                        Err(message) => {
                            debug!(%message, "refusing fs/read_text_file");
                            responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(message),
                            )
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: WriteTextFileRequest,
                            responder: Responder<WriteTextFileResponse>,
                            _cx| {
                    match write_scope.write_text(&request.path, &request.content).await {
                        Ok(()) => responder.respond(WriteTextFileResponse::new()),
                        Err(message) => {
                            debug!(%message, "refusing fs/write_text_file");
                            responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(message),
                            )
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                let acp_session_id = match handshake(
                    &cx,
                    &config,
                    cwd,
                    additional_directories,
                )
                .await
                {
                    Ok(id) => {
                        ready.send(Ok(id.clone())).ok();
                        id
                    }
                    Err(error) => {
                        ready
                            .send(Err(GatewayError::AgentUnavailable(error.to_string())))
                            .ok();
                        return Err(error);
                    }
                };

                info!(session_id = %session_id, agent = %descriptor.id, "acp session established");

                loop {
                    let command = tokio::select! {
                        command = commands.recv() => command,
                        // The agent went away: stop driving a dead connection
                        // instead of waiting for a command that can never be
                        // delivered.
                        () = cx.incoming_closed() => None,
                    };
                    let Some(command) = command else { break };

                    match command {
                        Command::Prompt(blocks) => {
                            let request = mapper::prompt_request(&acp_session_id, &blocks);
                            let sink = Arc::clone(&sink);
                            let session_id = session_id.clone();
                            cx.send_request(request).on_receiving_result(
                                move |result| async move {
                                    let draft = match result {
                                        Ok(response) => {
                                            mapper::turn_completed_event(response.stop_reason)
                                        }
                                        // A failed turn is not a failed session:
                                        // the user can prompt again.
                                        Err(error) => mapper::failure_event(error, false),
                                    };
                                    sink.emit(&session_id, draft).await.ok();
                                    Ok(())
                                },
                            )?;
                        }
                        Command::Cancel => {
                            cx.send_notification(CancelNotification::new(acp_session_id.clone()))?;
                            // ACP requires the client to answer every pending
                            // permission request when it cancels a turn.
                            let cancelled: Vec<(PermissionId, Pending)> = pendings
                                .lock()
                                .expect("permission map")
                                .drain()
                                .collect();
                            for (id, pending) in cancelled {
                                let outcome = RequestPermissionOutcome::Cancelled;
                                pending
                                    .responder
                                    .respond(RequestPermissionResponse::new(outcome.clone()))
                                    .ok();
                                sink.emit(
                                    &session_id,
                                    mapper::permission_response_event(&id, &outcome),
                                )
                                .await
                                .ok();
                            }
                        }
                        Command::Decide { id, decision } => {
                            let pending =
                                pendings.lock().expect("permission map").remove(&id);
                            if let Some(pending) = pending {
                                let outcome = mapper::outcome_for(&pending.request, &decision);
                                pending
                                    .responder
                                    .respond(RequestPermissionResponse::new(outcome.clone()))
                                    .ok();
                                sink.emit(
                                    &session_id,
                                    mapper::permission_response_event(&id, &outcome),
                                )
                                .await
                                .ok();
                            } else {
                                debug!(permission_id = %id, "decision for an unknown permission");
                            }
                        }
                        Command::Shutdown => break,
                    }
                }

                Ok(())
            })
            .await
    }
}

/// `initialize` + `session/new`, the only two steps a launch must complete.
async fn handshake(
    cx: &ConnectionTo<Agent>,
    config: &AcpRuntimeConfig,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
) -> std::result::Result<AcpSessionId, agent_client_protocol::Error> {
    let capabilities = ClientCapabilities::default()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        // Terminals are not offered yet: the gateway would have to run the
        // commands itself. Agents fall back to reporting command output as
        // tool-call content, which the gateway forwards unchanged.
        .terminal(false);

    let initialize = cx
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(capabilities)
                .client_info(Implementation::new(
                    config.client_name.clone(),
                    config.client_version.clone(),
                )),
        )
        .block_task()
        .await?;
    debug!(agent_info = ?initialize.agent_info, "agent initialized");

    let session = cx
        .send_request(NewSessionRequest::new(cwd).additional_directories(additional_directories))
        .block_task()
        .await?;
    Ok(session.session_id)
}

/// Log wire traffic without ever writing agent content at `info` or above.
fn log_wire_line(session_id: &SessionId, line: &str, direction: LineDirection) {
    let truncated: String = line.chars().take(LOG_LINE_LIMIT).collect();
    match direction {
        // stderr is the agent's own diagnostics: useful when an agent refuses
        // to start, so it gets `debug` rather than `trace`.
        LineDirection::Stderr => {
            debug!(session_id = %session_id, line = %truncated, "agent stderr")
        }
        LineDirection::Stdin | LineDirection::Stdout => {
            trace!(session_id = %session_id, line = %truncated, ?direction, "acp wire");
        }
    }
}
