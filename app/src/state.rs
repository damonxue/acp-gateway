//! Central UI state for the desktop app.
//!
//! 桌面 App 的中心状态。

use std::collections::{HashMap, VecDeque};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, Task, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::input::InputState;
use tokio::sync::{mpsc, oneshot};

use crate::views::{h_flex, v_flex};

use gateway_auth::PairingOffer;
use gateway_core::device::Device;
use gateway_core::event::AgentEvent;
use gateway_core::machine::Machine;
use gateway_core::manager::SessionSnapshot;
use gateway_core::session::{AgentSession, SessionStatus};
use gateway_core::transcript::{Transcript, fold_transcript};
use gateway_remote::protocol::{AgentSummary, PermissionAnswer, PromptInput, ServerMessage};

use crate::daemon::{self, DaemonConfig};
use crate::gateway_client::{AhpStatusSnapshot, GatewayClient, GatewayHealth, RemoteFrame};
use crate::views;
use crate::zed_settings::{ZedSettingsManager, ZedSettingsSnapshot};

#[derive(Clone, Debug)]
pub struct AppServices {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub client: GatewayClient,
    pub daemon_binary: PathBuf,
    pub zed: ZedSettingsManager,
}

impl AppServices {
    pub fn discover(runtime: Arc<tokio::runtime::Runtime>) -> Result<Self> {
        let client = GatewayClient::new(GatewayClient::discover_base_url());
        let daemon_binary = discover_daemon_binary();
        let zed = ZedSettingsManager::discover()?;
        Ok(Self {
            runtime,
            client,
            daemon_binary,
            zed,
        })
    }
}

fn discover_daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_GATEWAY_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("agent-gateway");
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from("agent-gateway")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppTab {
    Sessions,
    Machines,
    Agents,
    Devices,
    Integrations,
    Logs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Loading,
    Online,
    Offline,
}

impl ConnectionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonStatus {
    Stopped,
    Starting,
    Running { pid: Option<u32> },
    Failed { message: String },
}

pub type UiUpdateSender = mpsc::UnboundedSender<UiUpdate>;
type UiUpdateReceiver = mpsc::UnboundedReceiver<UiUpdate>;

#[derive(Debug)]
pub enum UiUpdate {
    Overview {
        health: GatewayHealth,
        sessions: Vec<AgentSession>,
        agents: Vec<AgentSummary>,
        devices: Vec<Device>,
        ahp: AhpStatusSnapshot,
    },
    Connection {
        status: ConnectionStatus,
        detail: Option<String>,
    },
    Daemon(DaemonStatus),
    DaemonLog(String),
    Transcript {
        session_id: String,
        events: Vec<AgentEvent>,
    },
    SessionSnapshot {
        session_id: String,
        snapshot: SessionSnapshot,
    },
    SelectedSession(Option<String>),
    SelectedAgent(Option<String>),
    PairingOffer(PairingOffer),
    ZedSnapshot(ZedSettingsSnapshot),
    Error(String),
    ClearPrompt,
    ClearForm,
}

pub struct AppState {
    pub(crate) services: Arc<AppServices>,
    pub(crate) tab: AppTab,
    pub(crate) connection: ConnectionStatus,
    pub(crate) connection_detail: Option<String>,
    pub(crate) daemon_status: DaemonStatus,
    pub(crate) health: Option<GatewayHealth>,
    pub(crate) sessions: Vec<AgentSession>,
    pub(crate) agents: Vec<AgentSummary>,
    pub(crate) devices: Vec<Device>,
    pub(crate) ahp_status: AhpStatusSnapshot,
    pub(crate) selected_session_id: Option<String>,
    pub(crate) selected_session_snapshot: Option<SessionSnapshot>,
    pub(crate) selected_transcript: Transcript,
    pub(crate) session_events: HashMap<String, Vec<AgentEvent>>,
    pub(crate) selected_agent_id: Option<String>,
    pub(crate) prompt_input: Entity<InputState>,
    pub(crate) workspace_input: Entity<InputState>,
    pub(crate) cwd_input: Entity<InputState>,
    pub(crate) pairing_offer: Option<PairingOffer>,
    pub(crate) zed_snapshot: ZedSettingsSnapshot,
    pub(crate) logs: VecDeque<String>,
    pub(crate) status_message: Option<String>,
    pub(crate) updates_tx: UiUpdateSender,
    pub(crate) updates_rx: Option<UiUpdateReceiver>,
    pub(crate) ui_task: Option<Task<()>>,
    pub(crate) selected_stream_task: Option<Task<()>>,
    pub(crate) daemon_stop: Option<oneshot::Sender<()>>,
}

impl AppState {
    pub fn new(
        services: Arc<AppServices>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir().to_path_buf());
        let prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 8)
                .placeholder("Send a prompt...")
        });
        let cwd_placeholder = cwd.to_string_lossy().to_string();
        let workspace_input = cx.new(|cx| InputState::new(window, cx).placeholder(cwd_placeholder));
        let cwd_input = cx.new(|cx| InputState::new(window, cx).placeholder("Optional cwd"));

        let (updates_tx, updates_rx) = mpsc::unbounded_channel();
        let zed_snapshot = services
            .zed
            .snapshot()
            .unwrap_or_else(|_| ZedSettingsSnapshot {
                settings_path: services.zed.settings_path().to_path_buf(),
                backup_path: services.zed.backup_path().to_path_buf(),
                record_path: services.zed.record_path().to_path_buf(),
                ..Default::default()
            });

        Self {
            services,
            tab: AppTab::Sessions,
            connection: ConnectionStatus::Loading,
            connection_detail: None,
            daemon_status: DaemonStatus::Stopped,
            health: None,
            sessions: Vec::new(),
            agents: Vec::new(),
            devices: Vec::new(),
            ahp_status: AhpStatusSnapshot::default(),
            selected_session_id: None,
            selected_session_snapshot: None,
            selected_transcript: Transcript::default(),
            session_events: HashMap::new(),
            selected_agent_id: None,
            prompt_input,
            workspace_input,
            cwd_input,
            pairing_offer: None,
            zed_snapshot,
            logs: VecDeque::new(),
            status_message: None,
            updates_tx,
            updates_rx: Some(updates_rx),
            ui_task: None,
            selected_stream_task: None,
            daemon_stop: None,
        }
    }

    pub fn start(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.ui_task.is_none() {
            let rx = self.updates_rx.take().expect("start called twice");
            let task = cx.spawn(async move |this, mut async_cx| {
                let mut rx = rx;
                while let Some(update) = rx.recv().await {
                    if this.upgrade().is_none() {
                        break;
                    }
                    let _ = this.update(async_cx, |state, cx| state.apply_update(update, cx));
                }
            });
            self.ui_task = Some(task);
        }

        self.reload_zed_snapshot(cx);

        if !self.api_reachable() {
            self.start_daemon();
        } else {
            // The daemon may have been started outside this app. Reflect the
            // reachable API immediately while keeping ownership explicit so
            // Stop only signals a child supervised by this process.
            self.daemon_status = DaemonStatus::Running { pid: None };
        }

        self.refresh_overview(window, cx);
        self.start_status_poll();
    }

    fn start_status_poll(&self) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                match load_overview(&client).await {
                    Ok((health, sessions, agents, devices, ahp)) => {
                        let _ = tx.send(UiUpdate::Overview {
                            health,
                            sessions,
                            agents,
                            devices,
                            ahp,
                        });
                        let _ = tx.send(UiUpdate::Connection {
                            status: ConnectionStatus::Online,
                            detail: None,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(UiUpdate::Connection {
                            status: ConnectionStatus::Offline,
                            detail: Some(error.to_string()),
                        });
                    }
                }
            }
        });
    }

    pub fn set_tab(&mut self, tab: AppTab, cx: &mut gpui::Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    pub fn refresh_overview(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);

        runtime.spawn(async move {
            let mut attempts = 0usize;
            loop {
                match load_overview(&client).await {
                    Ok((health, sessions, agents, devices, ahp)) => {
                        let _ = tx.send(UiUpdate::Overview {
                            health,
                            sessions,
                            agents,
                            devices,
                            ahp,
                        });
                        let _ = tx.send(UiUpdate::Connection {
                            status: ConnectionStatus::Online,
                            detail: None,
                        });
                        break;
                    }
                    Err(error) => {
                        attempts += 1;
                        let _ = tx.send(UiUpdate::Connection {
                            status: ConnectionStatus::Offline,
                            detail: Some(error.to_string()),
                        });
                        if attempts > 30 {
                            let _ = tx.send(UiUpdate::Error(error.to_string()));
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        });
    }

    pub fn select_agent(&mut self, agent_id: String, cx: &mut gpui::Context<Self>) {
        self.selected_agent_id = Some(agent_id);
        cx.notify();
    }

    pub fn select_session(
        &mut self,
        session_id: String,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.selected_session_id.as_ref() == Some(&session_id) {
            return;
        }
        self.selected_session_id = Some(session_id.clone());
        self.spawn_selected_stream(session_id);
        cx.notify();
    }

    pub fn launch_session(
        &mut self,
        agent_id: String,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let workspace = input_value(&self.workspace_input, cx);
        let cwd = input_value(&self.cwd_input, cx);
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client
                .create_session(
                    &agent_id,
                    workspace.clone(),
                    if cwd.trim().is_empty() {
                        None
                    } else {
                        Some(cwd)
                    },
                    Vec::new(),
                )
                .await
            {
                Ok(session) => {
                    let _ = tx.send(UiUpdate::Overview {
                        health: match load_health_only(&client).await {
                            Ok(health) => health,
                            Err(_) => GatewayHealth {
                                status: "ok".to_owned(),
                                version: String::new(),
                                machine: session_to_machine(&session),
                                sessions: crate::gateway_client::SessionCounts {
                                    active: 0,
                                    created_total: 0,
                                },
                                components: Vec::new(),
                            },
                        },
                        sessions: vec![session.clone()],
                        agents: Vec::new(),
                        devices: Vec::new(),
                        ahp: AhpStatusSnapshot::default(),
                    });
                    let _ = tx.send(UiUpdate::SelectedSession(Some(session.id.to_string())));
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn submit_prompt(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(session_id) = self.selected_session_id.clone() else {
            return;
        };
        let prompt = input_value(&self.prompt_input, cx);
        let prompt = prompt.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let _ = self
            .prompt_input
            .update(cx, |input, cx| input.set_value("", _window, cx));
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            let result = client.prompt(&session_id, PromptInput::Text(prompt)).await;
            if let Err(error) = result {
                let _ = tx.send(UiUpdate::Error(error.to_string()));
            }
        });
    }

    pub fn cancel_selected_session(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        let Some(session_id) = self.selected_session_id.clone() else {
            return;
        };
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client.cancel(&session_id).await {
                Ok(()) => {}
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn begin_pairing(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client.begin_pairing().await {
                Ok(offer) => {
                    let _ = tx.send(UiUpdate::PairingOffer(offer));
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn revoke_device(
        &mut self,
        device_id: String,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client.revoke_device(&device_id).await {
                Ok(()) => {
                    let _ = tx.send(UiUpdate::SelectedAgent(None));
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn answer_permission(
        &mut self,
        request_id: String,
        option_id: Option<String>,
        approved: bool,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) {
        let Some(session_id) = self.selected_session_id.clone() else {
            return;
        };
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            let answer = PermissionAnswer {
                request_id: request_id.into(),
                option_id,
                approved: Some(approved),
            };
            match client.permission(&session_id, answer).await {
                Ok(()) => {}
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn enable_zed(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        let agents: Vec<_> = self
            .agents
            .iter()
            .map(|agent| gateway_core::agent::AgentDescriptor {
                id: agent.id.clone().into(),
                name: agent.name.clone(),
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
            })
            .collect();
        if let Err(error) = self
            .services
            .zed
            .enable_for_agents(&agents, &self.services.daemon_binary)
        {
            self.status_message = Some(error.to_string());
        }
        self.reload_zed_snapshot_sync();
    }

    pub fn disable_zed(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        if let Err(error) = self.services.zed.disable() {
            self.status_message = Some(error.to_string());
        }
        self.reload_zed_snapshot_sync();
    }

    pub fn restore_zed_backup(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        if let Err(error) = self.services.zed.restore_backup() {
            self.status_message = Some(error.to_string());
        }
        self.reload_zed_snapshot_sync();
    }

    pub fn create_session_from_inputs(
        &mut self,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(agent_id) = self
            .selected_agent_id
            .clone()
            .or_else(|| self.agents.first().map(|agent| agent.id.clone()))
        else {
            return;
        };
        let workspace = input_value(&self.workspace_input, cx);
        if workspace.trim().is_empty() {
            self.status_message = Some("workspace is required".to_owned());
            return;
        }
        let cwd = input_value(&self.cwd_input, cx);
        let cwd = if cwd.trim().is_empty() {
            None
        } else {
            Some(cwd)
        };
        let _ = self
            .workspace_input
            .update(cx, |input, cx| input.set_value("", _window, cx));
        let _ = self
            .cwd_input
            .update(cx, |input, cx| input.set_value("", _window, cx));
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client
                .create_session(&agent_id, workspace, cwd, Vec::new())
                .await
            {
                Ok(session) => {
                    let _ = tx.send(UiUpdate::SelectedSession(Some(session.id.to_string())));
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    fn reload_zed_snapshot(&mut self, cx: &mut gpui::Context<Self>) {
        self.reload_zed_snapshot_sync();
        cx.notify();
    }

    fn reload_zed_snapshot_sync(&mut self) {
        self.zed_snapshot = self
            .services
            .zed
            .snapshot()
            .unwrap_or_else(|_| ZedSettingsSnapshot {
                settings_path: self.services.zed.settings_path().to_path_buf(),
                backup_path: self.services.zed.backup_path().to_path_buf(),
                record_path: self.services.zed.record_path().to_path_buf(),
                ..Default::default()
            });
    }

    pub fn start_daemon(&mut self) {
        if self.daemon_stop.is_some() {
            return;
        }
        let stop = daemon::spawn_supervisor(
            Arc::clone(&self.services.runtime),
            DaemonConfig {
                binary: self.services.daemon_binary.clone(),
            },
            self.updates_tx.clone(),
        );
        self.daemon_stop = Some(stop);
    }

    pub fn stop_daemon(&mut self) {
        if let Some(stop) = self.daemon_stop.take() {
            let _ = stop.send(());
        }
    }

    pub fn bind_ahp(
        &mut self,
        session_id: String,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client.ahp_bind(&session_id).await {
                Ok(()) => {
                    let _ = tx.send(UiUpdate::Connection {
                        status: ConnectionStatus::Online,
                        detail: Some(format!("微信已绑定到 {}", session_id)),
                    });
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    pub fn unbind_ahp(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) {
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            match client.ahp_unbind().await {
                Ok(()) => {
                    let _ = tx.send(UiUpdate::Connection {
                        status: ConnectionStatus::Online,
                        detail: Some("微信绑定已解除".into()),
                    });
                }
                Err(error) => {
                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                }
            }
        });
    }

    fn spawn_selected_stream(&mut self, session_id: String) {
        if let Some(task) = self.selected_stream_task.take() {
            drop(task);
        }
        let tx = self.updates_tx.clone();
        let client = self.services.client.clone();
        let runtime = Arc::clone(&self.services.runtime);
        runtime.spawn(async move {
            let mut after_seq = 0;
            loop {
                match client.subscribe_message(&session_id, after_seq).await {
                    Ok(mut socket) => {
                        while let Some(message) = socket.next().await {
                            let Ok(message) = message else {
                                break;
                            };
                            let text = match message {
                                tokio_tungstenite::tungstenite::Message::Text(text) => {
                                    text.to_string()
                                }
                                tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                                    match String::from_utf8(bytes.to_vec()) {
                                        Ok(text) => text,
                                        Err(_) => continue,
                                    }
                                }
                                _ => continue,
                            };
                            let value: serde_json::Value = match serde_json::from_str(&text) {
                                Ok(value) => value,
                                Err(error) => {
                                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                                    continue;
                                }
                            };
                            match GatewayClient::parse_remote_frame(value).await {
                                Ok(RemoteFrame::Control(ServerMessage::Subscribed {
                                    snapshot,
                                    ..
                                })) => {
                                    after_seq = snapshot.session.last_seq;
                                    let _ = tx.send(UiUpdate::SessionSnapshot {
                                        session_id: session_id.clone(),
                                        snapshot,
                                    });
                                }
                                Ok(RemoteFrame::Control(ServerMessage::Error {
                                    message, ..
                                })) => {
                                    let _ = tx.send(UiUpdate::Error(message));
                                }
                                Ok(RemoteFrame::Event(event)) => {
                                    after_seq = event.seq;
                                    let event_type = event.event_type.clone();
                                    let _ = tx.send(UiUpdate::Transcript {
                                        session_id: session_id.clone(),
                                        events: vec![event.clone()],
                                    });
                                    if matches!(
                                        event_type.as_str(),
                                        "session_status"
                                            | "session_completed"
                                            | "session_failed"
                                            | "session_update"
                                    ) {
                                        if let Ok(snapshot) = client.get_session(&session_id).await
                                        {
                                            let _ = tx.send(UiUpdate::SessionSnapshot {
                                                session_id: session_id.clone(),
                                                snapshot,
                                            });
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ = tx.send(UiUpdate::Error(error.to_string()));
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(UiUpdate::Error(error.to_string()));
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    fn apply_update(&mut self, update: UiUpdate, cx: &mut gpui::Context<Self>) {
        match update {
            UiUpdate::Overview {
                health,
                sessions,
                agents,
                devices,
                ahp,
            } => {
                let had_selected_session = self.selected_session_id.is_some();
                self.health = Some(health);
                self.sessions = sessions;
                self.agents = agents;
                self.devices = devices;
                self.ahp_status = ahp;
                self.connection = ConnectionStatus::Online;
                if self.selected_agent_id.is_none() {
                    self.selected_agent_id = self.agents.first().map(|agent| agent.id.clone());
                }
                if self.selected_session_id.is_none() {
                    self.selected_session_id =
                        self.sessions.first().map(|session| session.id.to_string());
                }
                if !had_selected_session && let Some(session_id) = self.selected_session_id.clone()
                {
                    self.spawn_selected_stream(session_id);
                }
            }
            UiUpdate::Connection { status, detail } => {
                self.connection = status;
                self.connection_detail = detail;
            }
            UiUpdate::Daemon(status) => {
                self.daemon_status = status;
                if !matches!(self.daemon_status, DaemonStatus::Running { .. }) {
                    self.daemon_stop = None;
                }
            }
            UiUpdate::DaemonLog(line) => {
                self.logs.push_back(line);
                while self.logs.len() > 300 {
                    self.logs.pop_front();
                }
            }
            UiUpdate::Transcript { session_id, events } => {
                self.session_events
                    .entry(session_id.clone())
                    .or_default()
                    .extend(events);
                if self.selected_session_id.as_ref() == Some(&session_id) {
                    self.rebuild_selected_transcript();
                }
            }
            UiUpdate::SessionSnapshot {
                session_id,
                snapshot,
            } => {
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|session| session.id.to_string() == session_id)
                {
                    *existing = snapshot.session.clone();
                }
                if self.selected_session_id.as_ref() == Some(&session_id) {
                    self.selected_session_snapshot = Some(snapshot);
                    self.rebuild_selected_transcript();
                }
            }
            UiUpdate::SelectedSession(session_id) => {
                self.selected_session_id = session_id.clone();
                if let Some(session_id) = session_id {
                    self.spawn_selected_stream(session_id);
                }
            }
            UiUpdate::SelectedAgent(agent_id) => {
                self.selected_agent_id = agent_id;
            }
            UiUpdate::PairingOffer(offer) => {
                self.pairing_offer = Some(offer);
            }
            UiUpdate::ZedSnapshot(snapshot) => {
                self.zed_snapshot = snapshot;
            }
            UiUpdate::Error(message) => {
                self.status_message = Some(message);
                self.connection = ConnectionStatus::Offline;
            }
            UiUpdate::ClearPrompt => {}
            UiUpdate::ClearForm => {}
        }
        cx.notify();
    }

    fn rebuild_selected_transcript(&mut self) {
        let Some(session_id) = self.selected_session_id.clone() else {
            self.selected_transcript = Transcript::default();
            return;
        };
        let events = self
            .session_events
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let mut transcript = fold_transcript(&events);
        if let Some(snapshot) = self.selected_session_snapshot.as_ref() {
            for request in &snapshot.pending_permissions {
                if !transcript
                    .pending
                    .iter()
                    .any(|candidate| candidate.id == request.id)
                {
                    transcript.pending.push(request.clone());
                }
            }
        }
        self.selected_transcript = transcript;
    }

    pub fn selected_session_view(&self) -> Option<SelectedSessionView> {
        let id = self.selected_session_id.as_ref()?;
        let session = self
            .sessions
            .iter()
            .find(|session| session.id.to_string() == *id)?;
        Some(SelectedSessionView {
            title: session.title.clone(),
            agent_name: session.agent_name.clone(),
            workspace: session.workspace.display().to_string(),
            status: session.status,
        })
    }

    pub fn pending_permissions(&self) -> Vec<gateway_core::permission::PermissionRequest> {
        let mut pending = self.selected_transcript.pending.clone();
        pending.sort_by_key(|request| request.requested_at);
        pending
    }

    pub fn daemon_status_label(&self) -> String {
        match &self.daemon_status {
            DaemonStatus::Stopped => "stopped".to_owned(),
            DaemonStatus::Starting => "starting".to_owned(),
            DaemonStatus::Running { pid } => match pid {
                Some(pid) => format!("running ({pid})"),
                None => "running".to_owned(),
            },
            DaemonStatus::Failed { message } => format!("failed: {message}"),
        }
    }

    pub fn connection_label(&self) -> String {
        match self.connection {
            ConnectionStatus::Loading => "loading".to_owned(),
            ConnectionStatus::Online => "online".to_owned(),
            ConnectionStatus::Offline => self
                .connection_detail
                .clone()
                .unwrap_or_else(|| "offline".to_owned()),
        }
    }

    pub fn zed_status_label(&self) -> String {
        if self.zed_snapshot.enabled {
            "enabled".to_owned()
        } else if self.zed_snapshot.has_backup {
            "backup ready".to_owned()
        } else {
            "disabled".to_owned()
        }
    }

    fn api_reachable(&self) -> bool {
        let base = self.services.client.base_url();
        let Some(base) = base.strip_prefix("http://") else {
            return false;
        };
        let mut parts = base.split(':');
        let host = parts.next().unwrap_or("127.0.0.1");
        let port = parts
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(48100);
        if host != "127.0.0.1" && host != "localhost" {
            return false;
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok()
    }
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let content = match self.tab {
            AppTab::Sessions => views::sessions::render(self, window, cx).into_any_element(),
            AppTab::Machines => views::machines::render(self, window, cx).into_any_element(),
            AppTab::Agents => views::agents::render(self, window, cx).into_any_element(),
            AppTab::Devices => views::devices::render(self, window, cx).into_any_element(),
            AppTab::Integrations => {
                views::integrations::render(self, window, cx).into_any_element()
            }
            AppTab::Logs => render_logs(self).into_any_element(),
        };

        h_flex()
            .size_full()
            .bg(rgb(0x0b1220))
            .child(views::sidebar::render(self, window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(0x27303a))
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div().text_size(px(12.)).text_color(rgb(0x9ca3af)).child(
                                            self.status_message
                                                .clone()
                                                .unwrap_or_else(|| "ready".to_owned()),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(0x9ca3af))
                                            .child(self.connection_label()),
                                    ),
                            ),
                    )
                    .child(content),
            )
    }
}

fn render_logs(state: &AppState) -> impl IntoElement {
    v_flex().flex_1().min_w_0().px_4().py_4().gap_2().children(
        state
            .logs
            .iter()
            .map(|line| div().text_size(px(12.)).child(line.clone())),
    )
}

fn input_value(input: &Entity<InputState>, cx: &gpui::Context<AppState>) -> String {
    input.read(cx).value().to_string()
}

fn load_health_only(
    client: &GatewayClient,
) -> impl std::future::Future<Output = Result<GatewayHealth>> + '_ {
    async move { client.health().await }
}

async fn load_overview(
    client: &GatewayClient,
) -> Result<(
    GatewayHealth,
    Vec<AgentSession>,
    Vec<AgentSummary>,
    Vec<Device>,
    AhpStatusSnapshot,
)> {
    let health = client.health().await?;
    let agents = client.list_agents().await?;
    let sessions = client.list_sessions().await?;
    let devices = client.list_devices().await?;
    let ahp = client.ahp_status().await.unwrap_or_default();
    Ok((health, sessions, agents, devices, ahp))
}

fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Clone, Debug)]
pub struct SelectedSessionView {
    pub title: Option<String>,
    pub agent_name: String,
    pub workspace: String,
    pub status: SessionStatus,
}

fn session_to_machine(session: &AgentSession) -> Machine {
    Machine {
        id: session.machine_id.clone(),
        name: "Agent Gateway".to_owned(),
        platform: std::env::consts::OS.to_owned(),
        hostname: "localhost".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        public_endpoint: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        last_seen_at: Some(chrono::Utc::now()),
    }
}
