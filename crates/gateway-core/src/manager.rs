//! [`SessionManager`] — the driving port of the gateway.
//!
//! [`SessionManager`]——Gateway 的驱动端口。
//!
//! Everything a client can do to a session goes through this type, and the
//! event pipeline it owns is where the system's hardest guarantees live.
//!
//! 客户端对 Session 能做的一切都经由本类型；它拥有的事件管道，正是系统里最硬的那些
//! 保证所在之处。
//!
//! ## The append pipeline / 追加管道
//!
//! ```text
//!  producer ─► append_event ─┬─► allocate seq   (under the session's append lock)
//!                            ├─► persist to EventStore
//!                            ├─► fold into session state (status/title/last_seq)
//!                            ├─► persist session row
//!                            └─► broadcast to subscribers
//! ```
//!
//! The lock is held across the whole sequence. That is what makes `seq`
//! gap-free *and* makes "an event is durable before anyone sees it" true: a
//! client that receives `seq = N` can always replay `after_seq = N-1` and get
//! the same history.
//!
//! 锁贯穿整个流程。这既使 `seq` 无空洞，*也*使“事件在被任何人看到之前已持久化”成立：
//! 收到 `seq = N` 的客户端总能用 `after_seq = N-1` 回放出同样的历史。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::agent::{
    AgentDescriptor, AgentRuntime, AgentSessionHandle, EventSink, LaunchRequest, PromptBlock,
};
use crate::bus::{EventBus, EventSubscription, SessionLifecycle};
use crate::error::{GatewayError, Result};
use crate::event::{AgentEvent, EventDraft, EventType};
use crate::ids::{AgentId, EventId, MachineId, PermissionId, SessionId};
use crate::permission::{PermissionDecision, PermissionRequest};
use crate::ports::{Clock, EventStore, SessionRepository};
use crate::session::{AgentSession, SessionOrigin, SessionStatus};

/// Read-only view of the agents this gateway is allowed to launch.
///
/// Discovery is explicit by design (see PRD §33): the gateway never scans the
/// machine for executables.
///
/// 本 Gateway 允许启动的 Agent 的只读视图。
///
/// “发现”故意设计为显式的（见 PRD §33）：Gateway 绝不扫描本机的可执行文件。
#[derive(Clone, Debug, Default)]
pub struct AgentCatalog {
    agents: BTreeMap<AgentId, AgentDescriptor>,
}

impl AgentCatalog {
    /// Build a catalog from configured descriptors.
    ///
    /// 用配置中的描述符构造目录。
    #[must_use]
    pub fn new(agents: impl IntoIterator<Item = AgentDescriptor>) -> Self {
        Self {
            agents: agents
                .into_iter()
                .map(|descriptor| (descriptor.id.clone(), descriptor))
                .collect(),
        }
    }

    /// Look up one agent.
    ///
    /// 查找单个 Agent。
    #[must_use]
    pub fn get(&self, id: &AgentId) -> Option<&AgentDescriptor> {
        self.agents.get(id)
    }

    /// All configured agents, ordered by id.
    ///
    /// 所有已配置的 Agent，按 id 排序。
    pub fn iter(&self) -> impl Iterator<Item = &AgentDescriptor> {
        self.agents.values()
    }

    /// Whether the catalog is empty.
    ///
    /// 目录是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

/// Parameters for creating a gateway-owned session.
///
/// 创建一个 Gateway 自有 Session 的参数。
#[derive(Clone, Debug)]
pub struct CreateSessionSpec {
    /// Which configured agent to launch. / 要启动哪个已配置的 Agent。
    pub agent_id: AgentId,
    /// Project root. / 项目根目录。
    pub workspace: PathBuf,
    /// Working directory for the agent (defaults to `workspace`).
    /// Agent 的工作目录（默认为 `workspace`）。
    pub cwd: Option<PathBuf>,
    /// Extra directories the agent may touch. / Agent 可额外访问的目录。
    pub additional_directories: Vec<PathBuf>,
}

/// Parameters for adopting a session an IDE bridge already owns.
///
/// 接纳一个 IDE Bridge 已拥有的 Session 所需的参数。
#[derive(Debug)]
pub struct AdoptSessionSpec {
    /// Agent id reported by the bridge. / Bridge 上报的 Agent id。
    pub agent_id: AgentId,
    /// Display name reported by the bridge. / Bridge 上报的展示名称。
    pub agent_name: String,
    /// ACP session id assigned by the agent. / Agent 分配的 ACP session id。
    pub acp_session_id: String,
    /// Project root. / 项目根目录。
    pub workspace: PathBuf,
    /// Working directory the IDE passed to `session/new`.
    /// IDE 传给 `session/new` 的工作目录。
    pub cwd: PathBuf,
    /// Handle the manager will use to drive the session through the bridge.
    /// manager 用它经 Bridge 驱动这个 Session。
    pub handle: Arc<dyn AgentSessionHandle>,
}

/// A session plus its live runtime facts, as returned by the API.
///
/// Session 及其运行时信息，API 返回的形式。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Persistent projection. / 持久化投影。
    #[serde(flatten)]
    pub session: AgentSession,
    /// Permission requests still awaiting an answer. / 仍在等待回答的权限请求。
    pub pending_permissions: Vec<PermissionRequest>,
    /// Whether an agent connection is currently attached. / 当前是否有 Agent 连接。
    pub connected: bool,
    /// Live event subscribers. / 当前事件订阅者数。
    pub subscribers: usize,
}

/// Tunables for [`SessionManager`].
///
/// [`SessionManager`] 的可调参数。
#[derive(Clone, Debug)]
pub struct SessionManagerConfig {
    /// Identity of this machine. / 本机身份。
    pub machine_id: MachineId,
    /// Per-session broadcast ring buffer size. / 每个 Session 的广播环形缓冲区大小。
    pub channel_capacity: usize,
    /// Maximum events returned by one replay call. / 单次回放返回的最大事件数。
    pub max_replay_batch: u32,
    /// Maximum sessions returned by `list_sessions`. / `list_sessions` 返回的最大 Session 数。
    pub max_session_list: u32,
}

impl SessionManagerConfig {
    /// Defaults for a given machine.
    ///
    /// 指定机器的默认配置。
    #[must_use]
    pub fn new(machine_id: MachineId) -> Self {
        Self {
            machine_id,
            channel_capacity: crate::bus::DEFAULT_CHANNEL_CAPACITY,
            max_replay_batch: 2_000,
            max_session_list: 200,
        }
    }
}

/// Runtime state of one session, held in memory for its whole life.
///
/// 单个 Session 的运行时状态，在其整个生命周期内驻留内存。
#[derive(Debug)]
struct ManagedSession {
    meta: RwLock<AgentSession>,
    /// Serialises the whole append pipeline for this session.
    /// 为该 Session 串行化整个追加管道。
    append_lock: Mutex<()>,
    bus: EventBus,
    handle: RwLock<Option<Arc<dyn AgentSessionHandle>>>,
    pending_permissions: DashMap<PermissionId, PermissionRequest>,
}

impl ManagedSession {
    fn new(meta: AgentSession, capacity: usize) -> Self {
        Self {
            meta: RwLock::new(meta),
            append_lock: Mutex::new(()),
            bus: EventBus::with_capacity(capacity),
            handle: RwLock::new(None),
            pending_permissions: DashMap::new(),
        }
    }
}

/// Orchestrates sessions, events and permissions.
///
/// 编排 Session、事件与权限。
#[derive(Debug)]
pub struct SessionManager {
    config: SessionManagerConfig,
    sessions: DashMap<SessionId, Arc<ManagedSession>>,
    session_repo: Arc<dyn SessionRepository>,
    event_store: Arc<dyn EventStore>,
    runtime: Arc<dyn AgentRuntime>,
    catalog: AgentCatalog,
    clock: Arc<dyn Clock>,
    created_total: AtomicUsize,
    /// Gateway-wide status changes. Carries no session content, so a listener
    /// (the relay push worker, a session-list view) can watch every session
    /// without receiving every token of every answer.
    ///
    /// 全局的状态变化。不携带 Session 内容，因此监听者（Relay push worker、
    /// Session 列表视图）可以监听所有 Session，而不必接收每个回答的每个 token。
    lifecycle: tokio::sync::broadcast::Sender<SessionLifecycle>,
}

impl SessionManager {
    /// Assemble a manager from its ports.
    ///
    /// 用各个端口组装一个 manager。
    #[must_use]
    pub fn new(
        config: SessionManagerConfig,
        session_repo: Arc<dyn SessionRepository>,
        event_store: Arc<dyn EventStore>,
        runtime: Arc<dyn AgentRuntime>,
        catalog: AgentCatalog,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            sessions: DashMap::new(),
            session_repo,
            event_store,
            runtime,
            catalog,
            clock,
            created_total: AtomicUsize::new(0),
            lifecycle: tokio::sync::broadcast::channel(256).0,
        })
    }

    /// Watch status changes across every session on this machine.
    ///
    /// 监听本机所有 Session 的状态变化。
    #[must_use]
    pub fn subscribe_lifecycle(&self) -> tokio::sync::broadcast::Receiver<SessionLifecycle> {
        self.lifecycle.subscribe()
    }

    /// The agents this gateway can launch.
    ///
    /// 本 Gateway 可启动的 Agent。
    #[must_use]
    pub fn catalog(&self) -> &AgentCatalog {
        &self.catalog
    }

    /// Identity of the machine this manager runs on.
    ///
    /// 本 manager 所在机器的身份。
    #[must_use]
    pub fn machine_id(&self) -> &MachineId {
        &self.config.machine_id
    }

    /// Sessions created since process start.
    ///
    /// 进程启动以来创建的 Session 数。
    #[must_use]
    pub fn created_total(&self) -> usize {
        self.created_total.load(Ordering::Relaxed)
    }

    /// Sessions held in memory.
    ///
    /// 当前内存中的 Session 数。
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    // ---------------------------------------------------------------- startup
    // ---------------------------------------------------------------- 启动

    /// Reconcile persisted state with reality after a restart.
    ///
    /// A session that was `Running` when the daemon died has no agent behind it
    /// any more. Marking those `Disconnected` (rather than silently leaving
    /// them `Running`) is what lets the mobile session list stay honest.
    ///
    /// 重启后把持久化状态与现实对齐。
    ///
    /// 守护进程死掉时处于 `Running` 的 Session，背后已经没有 Agent 了。把它们标为
    /// `Disconnected`（而不是默默留在 `Running`），才能让手机上的 Session 列表不说谎。
    pub async fn recover_after_restart(&self) -> Result<u64> {
        let now = self.clock.now();
        let recovered = self
            .session_repo
            .mark_non_terminal(&self.config.machine_id, SessionStatus::Disconnected, now)
            .await?;
        if recovered > 0 {
            info!(
                sessions = recovered,
                "marked orphaned sessions as disconnected after restart"
            );
        }
        Ok(recovered)
    }

    /// Re-launch a Gateway-owned session whose agent disappeared with the
    /// previous daemon process. The persistent session id and event history
    /// are retained, so bindings such as the embedded WeChat adapter remain
    /// valid across a Gateway restart.
    pub async fn resume_gateway_session(self: &Arc<Self>, id: &SessionId) -> Result<AgentSession> {
        if let Some(managed) = self.sessions.get(id) {
            return Ok(managed.meta.read().await.clone());
        }

        let session = self
            .session_repo
            .get(id)
            .await?
            .ok_or_else(|| GatewayError::SessionNotFound(id.clone()))?;
        if session.origin != SessionOrigin::Gateway {
            return Err(GatewayError::InvalidSessionState {
                session: id.clone(),
                action: "resume",
                status: session.status.as_str(),
            });
        }
        if session.status.is_terminal() && session.status != SessionStatus::Disconnected {
            return Err(GatewayError::InvalidSessionState {
                session: id.clone(),
                action: "resume",
                status: session.status.as_str(),
            });
        }

        let descriptor = self
            .catalog
            .get(&session.agent_id)
            .ok_or_else(|| {
                GatewayError::AgentUnavailable(format!(
                    "agent `{}` is not configured",
                    session.agent_id
                ))
            })?
            .clone();
        let managed = Arc::new(ManagedSession::new(
            session.clone(),
            self.config.channel_capacity,
        ));
        self.sessions.insert(id.clone(), Arc::clone(&managed));

        info!(session_id = %id, agent = %descriptor.id, "resuming Gateway session");
        let launched = match self
            .runtime
            .launch(LaunchRequest {
                session_id: session.id.clone(),
                descriptor: descriptor.clone(),
                cwd: session.cwd.clone(),
                additional_directories: Vec::new(),
                sink: Arc::clone(self) as Arc<dyn EventSink>,
            })
            .await
        {
            Ok(launched) => launched,
            Err(error) => {
                self.sessions.remove(id);
                warn!(session_id = %id, %error, "agent resume failed");
                return Err(error);
            }
        };

        {
            let mut meta = managed.meta.write().await;
            meta.acp_session_id = Some(launched.acp_session_id.clone());
        }
        *managed.handle.write().await = Some(launched.handle);
        self.append_event(
            id,
            EventDraft::from_value(
                EventType::SessionCreated,
                serde_json::json!({
                    "session_id": session.id,
                    "agent_id": descriptor.id,
                    "agent_name": descriptor.name,
                    "acp_session_id": launched.acp_session_id,
                    "workspace": session.workspace,
                    "cwd": session.cwd,
                    "origin": SessionOrigin::Gateway,
                    "resumed": true,
                }),
            ),
        )
        .await?;
        self.snapshot_meta(id).await
    }

    // ------------------------------------------------------- session creation
    // ------------------------------------------------------- Session 创建

    /// Create a session and launch its agent.
    ///
    /// 创建一个 Session 并启动其 Agent。
    pub async fn create_session(self: &Arc<Self>, spec: CreateSessionSpec) -> Result<AgentSession> {
        let descriptor = self
            .catalog
            .get(&spec.agent_id)
            .ok_or_else(|| {
                GatewayError::AgentUnavailable(format!(
                    "agent `{}` is not configured",
                    spec.agent_id
                ))
            })?
            .clone();

        let now = self.clock.now();
        let cwd = spec.cwd.unwrap_or_else(|| spec.workspace.clone());
        let session = AgentSession {
            id: SessionId::generate(),
            machine_id: self.config.machine_id.clone(),
            agent_id: descriptor.id.clone(),
            agent_name: descriptor.name.clone(),
            acp_session_id: None,
            workspace: spec.workspace,
            cwd: cwd.clone(),
            title: None,
            origin: SessionOrigin::Gateway,
            status: SessionStatus::Idle,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };

        self.session_repo.insert(&session).await?;
        let managed = Arc::new(ManagedSession::new(
            session.clone(),
            self.config.channel_capacity,
        ));
        self.sessions.insert(session.id.clone(), managed.clone());
        self.created_total.fetch_add(1, Ordering::Relaxed);

        info!(
            session_id = %session.id,
            agent = %descriptor.id,
            "launching agent"
        );

        let launched = match self
            .runtime
            .launch(LaunchRequest {
                session_id: session.id.clone(),
                descriptor: descriptor.clone(),
                cwd,
                additional_directories: spec.additional_directories,
                sink: Arc::clone(self) as Arc<dyn EventSink>,
            })
            .await
        {
            Ok(launched) => launched,
            Err(error) => {
                // The row already exists, so surface the failure through the
                // same channel every other failure uses instead of rolling
                // back: a session that failed to start is useful history.
                //
                // 数据库行已经存在，因此不回滚，而是跟其他失败一样通过事件流上报：
                // 一个启动失败的 Session 本身就是有用的历史。
                warn!(session_id = %session.id, %error, "agent launch failed");
                self.append_event(
                    &session.id,
                    EventDraft::from_value(
                        EventType::SessionFailed,
                        serde_json::json!({ "reason": error.to_string(), "fatal": true }),
                    ),
                )
                .await
                .ok();
                return Err(error);
            }
        };

        {
            let mut meta = managed.meta.write().await;
            meta.acp_session_id = Some(launched.acp_session_id.clone());
        }
        *managed.handle.write().await = Some(launched.handle);

        self.append_event(
            &session.id,
            EventDraft::from_value(
                EventType::SessionCreated,
                serde_json::json!({
                    "session_id": session.id,
                    "agent_id": descriptor.id,
                    "agent_name": descriptor.name,
                    "acp_session_id": launched.acp_session_id,
                    "workspace": session.workspace,
                    "cwd": session.cwd,
                    "origin": SessionOrigin::Gateway,
                }),
            ),
        )
        .await?;

        self.snapshot_meta(&session.id).await
    }

    /// Adopt a session owned by an IDE bridge.
    ///
    /// The bridge has already completed `initialize` and `session/new` with the
    /// real agent; the gateway records the session so remote clients can watch
    /// and drive it through the bridge's control channel.
    ///
    /// 接纳一个属于 IDE Bridge 的 Session。
    ///
    /// Bridge 已经与真正的 Agent 完成了 `initialize` 和 `session/new`；Gateway 把这个
    /// Session 记录下来，使远程客户端能通过 Bridge 的控制通道观察并驱动它。
    pub async fn adopt_bridge_session(
        self: &Arc<Self>,
        spec: AdoptSessionSpec,
    ) -> Result<AgentSession> {
        if let Some(session) = self.reattach_bridge_session(&spec).await? {
            return Ok(session);
        }
        let now = self.clock.now();
        let session = AgentSession {
            id: SessionId::generate(),
            machine_id: self.config.machine_id.clone(),
            agent_id: spec.agent_id.clone(),
            agent_name: spec.agent_name.clone(),
            acp_session_id: Some(spec.acp_session_id.clone()),
            workspace: spec.workspace,
            cwd: spec.cwd,
            title: None,
            origin: SessionOrigin::IdeBridge,
            status: SessionStatus::Idle,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };

        self.session_repo.insert(&session).await?;
        let managed = Arc::new(ManagedSession::new(
            session.clone(),
            self.config.channel_capacity,
        ));
        *managed.handle.write().await = Some(spec.handle);
        self.sessions.insert(session.id.clone(), managed);
        self.created_total.fetch_add(1, Ordering::Relaxed);

        info!(
            session_id = %session.id,
            agent = %spec.agent_id,
            "adopted IDE bridge session"
        );

        self.append_event(
            &session.id,
            EventDraft::from_value(
                EventType::SessionCreated,
                serde_json::json!({
                    "session_id": session.id,
                    "agent_id": spec.agent_id,
                    "agent_name": spec.agent_name,
                    "acp_session_id": spec.acp_session_id,
                    "workspace": session.workspace,
                    "cwd": session.cwd,
                    "origin": SessionOrigin::IdeBridge,
                }),
            ),
        )
        .await?;

        self.snapshot_meta(&session.id).await
    }

    /// Reattach a bridge to a retained IDE session when the bridge transport
    /// was interrupted but the ACP agent kept the same session identity.
    async fn reattach_bridge_session(
        &self,
        spec: &AdoptSessionSpec,
    ) -> Result<Option<AgentSession>> {
        let mut candidates: Vec<Arc<ManagedSession>> = self
            .sessions
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect();

        for session in self
            .session_repo
            .list(&self.config.machine_id, self.config.max_session_list)
            .await?
        {
            if session.origin == SessionOrigin::IdeBridge
                && !matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Failed
                )
                && session.agent_id == spec.agent_id
                && session.acp_session_id.as_deref() == Some(spec.acp_session_id.as_str())
                && session.workspace == spec.workspace
                && session.cwd == spec.cwd
                && !self.sessions.contains_key(&session.id)
            {
                let session_id = session.id.clone();
                let managed = Arc::new(ManagedSession::new(session, self.config.channel_capacity));
                self.sessions.insert(session_id, Arc::clone(&managed));
                candidates.push(managed);
                break;
            }
        }

        for managed in candidates {
            let session = managed.meta.read().await.clone();
            if session.origin != SessionOrigin::IdeBridge
                || matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Failed
                )
                || session.agent_id != spec.agent_id
                || session.acp_session_id.as_deref() != Some(spec.acp_session_id.as_str())
                || session.workspace != spec.workspace
                || session.cwd != spec.cwd
            {
                continue;
            }
            *managed.handle.write().await = Some(Arc::clone(&spec.handle));
            if session.status == SessionStatus::Disconnected {
                self.set_status(
                    &session.id,
                    SessionStatus::Idle,
                    Some("IDE bridge reattached"),
                )
                .await?;
            }
            info!(
                session_id = %session.id,
                acp_session_id = %spec.acp_session_id,
                "reattached IDE bridge session"
            );
            return self.snapshot_meta(&session.id).await.map(Some);
        }
        Ok(None)
    }

    // -------------------------------------------------------------- accessors
    // -------------------------------------------------------------- 读取接口

    /// Read a session's persistent projection, from memory or the store.
    ///
    /// 读取 Session 的持久化投影，优先从内存，否则从存储。
    pub async fn get_session(&self, id: &SessionId) -> Result<AgentSession> {
        if let Some(managed) = self.sessions.get(id) {
            return Ok(managed.meta.read().await.clone());
        }
        self.session_repo
            .get(id)
            .await?
            .ok_or_else(|| GatewayError::SessionNotFound(id.clone()))
    }

    /// Read a session together with its live runtime facts.
    ///
    /// 读取 Session 及其运行时信息。
    pub async fn get_snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        let session = self.get_session(id).await?;
        let (pending, connected, subscribers) = match self.sessions.get(id) {
            Some(managed) => (
                managed
                    .pending_permissions
                    .iter()
                    .map(|entry| entry.value().clone())
                    .collect(),
                managed
                    .handle
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|handle| handle.is_alive()),
                managed.bus.subscriber_count(),
            ),
            None => (Vec::new(), false, 0),
        };
        Ok(SessionSnapshot {
            session,
            pending_permissions: pending,
            connected,
            subscribers,
        })
    }

    /// List sessions of this machine, most recently active first.
    ///
    /// 列出本机的 Session，最近活跃的在前。
    pub async fn list_sessions(&self) -> Result<Vec<AgentSession>> {
        self.session_repo
            .list(&self.config.machine_id, self.config.max_session_list)
            .await
    }

    /// Subscribe to a session's live event stream.
    ///
    /// 订阅某个 Session 的实时事件流。
    pub fn subscribe(&self, id: &SessionId) -> Result<EventSubscription> {
        self.sessions
            .get(id)
            .map(|managed| managed.bus.subscribe())
            .ok_or_else(|| GatewayError::SessionNotFound(id.clone()))
    }

    /// Replay persisted events after `after_seq`.
    ///
    /// 回放 `after_seq` 之后的已持久化事件。
    pub async fn replay_events(
        &self,
        id: &SessionId,
        after_seq: u64,
        limit: Option<u32>,
    ) -> Result<Vec<AgentEvent>> {
        // Make sure the session exists before touching the event table, so a
        // typo'd id is a 404 rather than an empty list.
        //
        // 先确认 Session 存在再查事件表，让写错的 id 得到 404 而不是空列表。
        self.get_session(id).await?;
        let limit = limit
            .unwrap_or(self.config.max_replay_batch)
            .min(self.config.max_replay_batch);
        self.event_store.read_after(id, after_seq, limit).await
    }

    // ----------------------------------------------------------------- driving
    // ----------------------------------------------------------------- 驱动接口

    /// Submit a prompt turn.
    ///
    /// 提交一个 prompt 回合。
    pub async fn send_prompt(&self, id: &SessionId, blocks: Vec<PromptBlock>) -> Result<()> {
        self.send_prompt_from(id, blocks, None).await
    }

    /// Submit a prompt and record its source in the user-message echo.
    ///
    /// A source is intentionally optional so existing API clients keep the
    /// exact historical payload. Channel adapters can mark their own echo so
    /// downstream consumers can identify where the prompt originated.
    pub async fn send_prompt_from(
        &self,
        id: &SessionId,
        blocks: Vec<PromptBlock>,
        source: Option<&str>,
    ) -> Result<()> {
        if blocks.is_empty() {
            return Err(GatewayError::invalid_request("prompt must not be empty"));
        }
        let managed = self.managed(id)?;
        let status = managed.meta.read().await.status;
        if !status.is_drivable() {
            return Err(GatewayError::InvalidSessionState {
                session: id.clone(),
                action: "prompt",
                status: status.as_str(),
            });
        }
        let handle = self.handle_of(&managed).await?;

        // The echo is recorded first so that every observer — including the
        // client that sent the prompt — sees the turn start at the same `seq`.
        //
        // 先记录回显，让所有观察者（包括发送 prompt 的那个客户端）都在同一个 `seq`
        // 上看到回合开始。
        let mut payload = serde_json::json!({ "content": blocks });
        if let Some(source) = source
            && let Some(object) = payload.as_object_mut()
        {
            object.insert(
                "source".to_owned(),
                serde_json::Value::String(source.to_owned()),
            );
        }
        self.append_event(id, EventDraft::from_value(EventType::UserMessage, payload))
            .await?;

        handle.submit_prompt(blocks).await
    }

    /// Ask the agent to stop the current turn.
    ///
    /// 请求 Agent 停止当前回合。
    pub async fn cancel_session(&self, id: &SessionId) -> Result<()> {
        let managed = self.managed(id)?;
        let handle = self.handle_of(&managed).await?;
        self.set_status(
            id,
            SessionStatus::Cancelling,
            Some("client requested cancel"),
        )
        .await?;
        handle.cancel().await
    }

    /// Answer an outstanding permission request.
    ///
    /// The `permission_response` event is emitted by the runtime that actually
    /// applies the decision, not here: in bridge mode the IDE may answer first,
    /// and only the runtime knows which decision won.
    ///
    /// 回答一个待处理的权限请求。
    ///
    /// `permission_response` 事件由真正应用该决定的运行时发出，而不是这里：Bridge 模式下
    /// IDE 可能先回答，只有运行时知道哪个决定胜出。
    pub async fn respond_permission(
        &self,
        id: &SessionId,
        permission_id: &PermissionId,
        decision: PermissionDecision,
    ) -> Result<()> {
        let managed = self.managed(id)?;
        if !managed.pending_permissions.contains_key(permission_id) {
            return Err(GatewayError::PermissionNotFound(permission_id.clone()));
        }
        let handle = self.handle_of(&managed).await?;
        handle.resolve_permission(permission_id, decision).await
    }

    /// Detach the agent connection and mark the session `Disconnected`.
    ///
    /// Called when a bridge drops or an agent process dies. History survives.
    ///
    /// 断开 Agent 连接，并将 Session 标为 `Disconnected`。
    ///
    /// 当 Bridge 掉线或 Agent 进程死亡时调用。历史保留。
    pub async fn detach_session(&self, id: &SessionId, reason: &str) -> Result<()> {
        let Some(managed) = self.sessions.get(id).map(|entry| Arc::clone(&entry)) else {
            return Ok(());
        };
        // A bridge can reconnect before the old WebSocket task observes its
        // EOF. In that race the old task must not detach the replacement
        // handle that has already been installed by reattachment.
        if managed
            .handle
            .read()
            .await
            .as_ref()
            .is_some_and(|handle| handle.is_alive())
        {
            return Ok(());
        }
        *managed.handle.write().await = None;
        for entry in managed.pending_permissions.iter() {
            debug!(
                session_id = %id,
                permission_id = %entry.key(),
                "dropping pending permission on detach"
            );
        }
        managed.pending_permissions.clear();
        self.set_status(id, SessionStatus::Disconnected, Some(reason))
            .await
    }

    /// Close a session and shut its agent down.
    ///
    /// 关闭 Session 并停掉其 Agent。
    pub async fn close_session(&self, id: &SessionId) -> Result<()> {
        let managed = self.managed(id)?;
        if let Some(handle) = managed.handle.write().await.take() {
            handle.shutdown().await.ok();
        }
        managed.pending_permissions.clear();
        self.set_status(id, SessionStatus::Completed, Some("closed by client"))
            .await
    }

    /// Shut every live session down. Used on daemon exit.
    ///
    /// 关闭所有存活 Session，用于守护进程退出。
    pub async fn shutdown_all(&self) {
        let ids: Vec<SessionId> = self
            .sessions
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for id in ids {
            if let Some(managed) = self.sessions.get(&id).map(|entry| Arc::clone(&entry))
                && let Some(handle) = managed.handle.write().await.take()
                && let Err(error) = handle.shutdown().await
            {
                warn!(session_id = %id, %error, "agent shutdown failed");
            }
        }
    }

    /// Record an explicit status change as a `session_status` event.
    ///
    /// 以 `session_status` 事件记录一次显式的状态变更。
    pub async fn set_status(
        &self,
        id: &SessionId,
        status: SessionStatus,
        reason: Option<&str>,
    ) -> Result<()> {
        self.append_event(
            id,
            EventDraft::from_value(
                EventType::SessionStatus,
                serde_json::json!({ "status": status, "reason": reason }),
            ),
        )
        .await
        .map(|_| ())
    }

    // ------------------------------------------------------------- the pipeline
    // ------------------------------------------------------------- 事件管道

    /// Sequence, persist, fold and broadcast one event.
    ///
    /// 为一个事件分配序号、持久化、折入状态并广播。
    pub async fn append_event(&self, id: &SessionId, draft: EventDraft) -> Result<AgentEvent> {
        let managed = self.managed(id)?;
        let _guard = managed.append_lock.lock().await;
        self.append_locked(id, &managed, draft).await
    }

    async fn append_locked(
        &self,
        id: &SessionId,
        managed: &Arc<ManagedSession>,
        draft: EventDraft,
    ) -> Result<AgentEvent> {
        let now = self.clock.now();
        let mut meta = managed.meta.write().await;
        let previous_status = meta.status;
        let seq = meta.last_seq + 1;
        let event = AgentEvent {
            id: EventId::generate(),
            session_id: id.clone(),
            seq,
            timestamp: now,
            event_type: draft.event_type.as_str().to_owned(),
            payload: draft.payload,
        };

        // 1. Durability first. If this fails nothing was observed, so the
        //    sequence number is simply not consumed.
        //    先保证持久化。失败时无人观测到任何东西，序号也就没被消耗。
        self.event_store.append(&event).await?;

        // 2. Fold the event into session state.
        //    把事件折入 Session 状态。
        meta.last_seq = seq;
        meta.updated_at = now;
        if let Some(status) = status_after(draft.event_type, &event.payload) {
            meta.status = status;
        }
        if let Some(title) = title_from(&event) {
            meta.title = Some(title);
        }
        let (status, last_seq, acp_session_id, title) = (
            meta.status,
            meta.last_seq,
            meta.acp_session_id.clone(),
            meta.title.clone(),
        );
        drop(meta);

        self.session_repo
            .update_runtime_state(
                id,
                status,
                last_seq,
                acp_session_id.as_deref(),
                title.as_deref(),
                now,
            )
            .await?;

        // 3. Track permission lifecycle so `pending_permissions` is always a
        //    projection of the event log rather than a parallel source of truth.
        //    跟踪权限生命周期，使 `pending_permissions` 始终是事件日志的投影，
        //    而不是另一个平行的事实源。
        self.fold_permissions(managed, &event);

        let created = draft.event_type == EventType::SessionCreated;
        if created || status != previous_status {
            // A closed channel just means nobody is watching.
            // channel 已关闭只说明没人在监听。
            self.lifecycle
                .send(SessionLifecycle {
                    session_id: id.clone(),
                    status,
                    created,
                })
                .ok();
        }

        // 4. Only now may anyone observe it.
        //    到此才允许任何人观测到它。
        managed.bus.publish(Arc::new(event.clone()));
        debug!(session_id = %id, seq, event_type = %event.event_type, "event appended");
        Ok(event)
    }

    fn fold_permissions(&self, managed: &Arc<ManagedSession>, event: &AgentEvent) {
        if event.event_type == EventType::PermissionRequest.as_str() {
            match serde_json::from_value::<PermissionRequest>(event.payload.clone()) {
                Ok(request) => {
                    managed
                        .pending_permissions
                        .insert(request.id.clone(), request);
                }
                Err(error) => warn!(%error, "malformed permission_request payload"),
            }
        } else if event.event_type == EventType::PermissionResponse.as_str()
            && let Some(id) = event.payload.get("id").and_then(serde_json::Value::as_str)
        {
            managed.pending_permissions.remove(&PermissionId::new(id));
        }
    }

    // ---------------------------------------------------------------- internals
    // ---------------------------------------------------------------- 内部实现

    fn managed(&self, id: &SessionId) -> Result<Arc<ManagedSession>> {
        self.sessions
            .get(id)
            .map(|entry| Arc::clone(&entry))
            .ok_or_else(|| GatewayError::SessionNotFound(id.clone()))
    }

    async fn handle_of(
        &self,
        managed: &Arc<ManagedSession>,
    ) -> Result<Arc<dyn AgentSessionHandle>> {
        managed
            .handle
            .read()
            .await
            .clone()
            .ok_or_else(|| GatewayError::AgentUnavailable("session has no live agent".to_owned()))
    }

    async fn snapshot_meta(&self, id: &SessionId) -> Result<AgentSession> {
        let managed = self.managed(id)?;
        let meta = managed.meta.read().await.clone();
        Ok(meta)
    }
}

#[async_trait]
impl EventSink for SessionManager {
    async fn emit(&self, session_id: &SessionId, draft: EventDraft) -> Result<()> {
        self.append_event(session_id, draft).await.map(|_| ())
    }
}

/// The one place session status is derived from the event stream.
///
/// Returning `None` means "this event does not change status", which keeps the
/// rule table exhaustive and reviewable instead of scattering `set_status`
/// calls through the adapters.
///
/// 从事件流推导 Session 状态的唯一地方。
///
/// 返回 `None` 表示“该事件不改变状态”。这使规则表完整且可审阅，而不是把 `set_status`
/// 调用洒得所有适配器里都是。
fn status_after(event_type: EventType, payload: &serde_json::Value) -> Option<SessionStatus> {
    match event_type {
        EventType::SessionCreated => Some(SessionStatus::Idle),
        EventType::UserMessage => Some(SessionStatus::Running),
        EventType::PermissionRequest => Some(SessionStatus::WaitingPermission),
        EventType::PermissionResponse => Some(SessionStatus::Running),
        EventType::SessionCompleted => Some(SessionStatus::Idle),
        EventType::SessionFailed => {
            // A turn can fail without ending the session; only an explicitly
            // fatal failure takes the session out of service.
            //
            // 单个回合失败并不意味着 Session 结束；只有显式标记为致命的失败
            // 才会让 Session 退出服务。
            let fatal = payload
                .get("fatal")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Some(if fatal {
                SessionStatus::Failed
            } else {
                SessionStatus::Idle
            })
        }
        EventType::SessionStatus => payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(SessionStatus::from_str_opt),
        _ => None,
    }
}

fn title_from(event: &AgentEvent) -> Option<String> {
    if event.event_type != EventType::SessionUpdate.as_str() {
        return None;
    }
    event
        .payload
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{RecordingRuntime, memory_manager};

    #[tokio::test]
    async fn append_assigns_gap_free_sequence_numbers() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        for _ in 0..10 {
            manager
                .append_event(
                    &session.id,
                    EventDraft::from_value(
                        EventType::AgentMessageChunk,
                        serde_json::json!({ "text": "x" }),
                    ),
                )
                .await
                .unwrap();
        }

        let events = manager.replay_events(&session.id, 0, None).await.unwrap();
        let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn events_are_durable_before_they_are_broadcast() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        let mut subscription = manager.subscribe(&session.id).unwrap();
        manager
            .append_event(
                &session.id,
                EventDraft::from_value(EventType::AgentMessageChunk, serde_json::json!({})),
            )
            .await
            .unwrap();

        let live = subscription.recv().await.unwrap();
        let replayed = manager
            .replay_events(&session.id, live.seq - 1, None)
            .await
            .unwrap();
        assert_eq!(replayed.first().map(|event| event.seq), Some(live.seq));
    }

    #[tokio::test]
    async fn status_follows_the_event_stream() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(
            manager.get_session(&session.id).await.unwrap().status,
            SessionStatus::Idle
        );

        manager
            .send_prompt(&session.id, vec![PromptBlock::text("hi")])
            .await
            .unwrap();
        assert_eq!(
            manager.get_session(&session.id).await.unwrap().status,
            SessionStatus::Running
        );

        manager
            .append_event(
                &session.id,
                EventDraft::from_value(
                    EventType::SessionCompleted,
                    serde_json::json!({ "stop_reason": "end_turn" }),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            manager.get_session(&session.id).await.unwrap().status,
            SessionStatus::Idle
        );
    }

    #[tokio::test]
    async fn a_non_fatal_turn_failure_keeps_the_session_usable() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        manager
            .append_event(
                &session.id,
                EventDraft::from_value(
                    EventType::SessionFailed,
                    serde_json::json!({ "reason": "tool error", "fatal": false }),
                ),
            )
            .await
            .unwrap();

        assert_eq!(
            manager.get_session(&session.id).await.unwrap().status,
            SessionStatus::Idle
        );
    }

    #[tokio::test]
    async fn prompts_are_rejected_once_the_session_is_terminal() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        manager.close_session(&session.id).await.unwrap();
        let error = manager
            .send_prompt(&session.id, vec![PromptBlock::text("hi")])
            .await
            .unwrap_err();
        assert!(matches!(error, GatewayError::InvalidSessionState { .. }));
    }

    #[tokio::test]
    async fn unknown_agents_are_reported_as_unavailable() {
        let (manager, _) = memory_manager().await;
        let error = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("nope"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, GatewayError::AgentUnavailable(_)));
    }

    #[tokio::test]
    async fn prompts_reach_the_runtime() {
        let (manager, runtime) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();
        manager
            .send_prompt(&session.id, vec![PromptBlock::text("check redis")])
            .await
            .unwrap();
        assert_eq!(
            RecordingRuntime::prompts(&runtime),
            vec!["check redis".to_owned()]
        );
    }

    #[tokio::test]
    async fn sourced_prompts_mark_the_echo_for_channel_adapters() {
        let (manager, _) = memory_manager().await;
        let session = manager
            .create_session(CreateSessionSpec {
                agent_id: AgentId::new("mock"),
                workspace: PathBuf::from("/tmp/project"),
                cwd: None,
                additional_directories: Vec::new(),
            })
            .await
            .unwrap();

        manager
            .send_prompt_from(
                &session.id,
                vec![PromptBlock::text("from WeChat")],
                Some("ahp"),
            )
            .await
            .unwrap();
        let events = manager.replay_events(&session.id, 0, None).await.unwrap();
        let user_message = events
            .iter()
            .find(|event| event.event_type == EventType::UserMessage.as_str())
            .unwrap();
        assert_eq!(user_message.payload["source"], "ahp");
    }
}
