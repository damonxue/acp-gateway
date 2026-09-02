//! In-memory adapters for tests.
//!
//! 供测试使用的内存适配器。
//!
//! Compiled for this crate's own tests and, behind the `testing` feature, for
//! downstream crates so they can exercise their adapters against a real
//! [`SessionManager`] without dragging SQLite or a child process into a unit
//! test.
//!
//! 它会在本 crate 自己的测试中编译；开启 `testing` feature 时也供下游 crate 使用，
//! 使它们能在不引入 SQLite 或子进程的前提下，面对真实的 [`SessionManager`] 测试自己的适配器。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::agent::{
    AgentDescriptor, AgentRuntime, AgentSessionHandle, LaunchRequest, LaunchedAgent, PromptBlock,
};
use crate::error::{GatewayError, Result};
use crate::event::AgentEvent;
use crate::ids::{AgentId, MachineId, PermissionId, SessionId};
use crate::manager::{AgentCatalog, SessionManager, SessionManagerConfig};
use crate::permission::PermissionDecision;
use crate::ports::{Clock, EventStore, SessionRepository, SystemClock};
use crate::session::{AgentSession, SessionStatus};

/// A [`SessionRepository`] backed by a `HashMap`.
///
/// 基于 `HashMap` 的 [`SessionRepository`]。
#[derive(Debug, Default)]
pub struct MemorySessionRepository {
    rows: Mutex<HashMap<SessionId, AgentSession>>,
}

#[async_trait]
impl SessionRepository for MemorySessionRepository {
    async fn insert(&self, session: &AgentSession) -> Result<()> {
        self.rows
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn get(&self, id: &SessionId) -> Result<Option<AgentSession>> {
        Ok(self.rows.lock().unwrap().get(id).cloned())
    }

    async fn list(&self, machine_id: &MachineId, limit: u32) -> Result<Vec<AgentSession>> {
        let mut rows: Vec<AgentSession> = self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter(|session| &session.machine_id == machine_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn update_runtime_state(
        &self,
        id: &SessionId,
        status: SessionStatus,
        last_seq: u64,
        acp_session_id: Option<&str>,
        title: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .get_mut(id)
            .ok_or_else(|| GatewayError::SessionNotFound(id.clone()))?;
        row.status = status;
        row.last_seq = last_seq;
        row.acp_session_id = acp_session_id.map(ToOwned::to_owned);
        row.title = title.map(ToOwned::to_owned);
        row.updated_at = updated_at;
        Ok(())
    }

    async fn mark_non_terminal(
        &self,
        machine_id: &MachineId,
        status: SessionStatus,
        updated_at: DateTime<Utc>,
    ) -> Result<u64> {
        let mut count = 0;
        for row in self.rows.lock().unwrap().values_mut() {
            if &row.machine_id == machine_id && !row.status.is_terminal() {
                row.status = status;
                row.updated_at = updated_at;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// An [`EventStore`] backed by a `Vec`.
///
/// 基于 `Vec` 的 [`EventStore`]。
#[derive(Debug, Default)]
pub struct MemoryEventStore {
    rows: Mutex<Vec<AgentEvent>>,
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(&self, event: &AgentEvent) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        if rows
            .iter()
            .any(|row| row.session_id == event.session_id && row.seq == event.seq)
        {
            return Err(GatewayError::EventStore(format!(
                "duplicate seq {} for session {}",
                event.seq, event.session_id
            )));
        }
        rows.push(event.clone());
        Ok(())
    }

    async fn read_after(
        &self,
        session_id: &SessionId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<AgentEvent>> {
        let mut rows: Vec<AgentEvent> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| &row.session_id == session_id && row.seq > after_seq)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.seq);
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn last_seq(&self, session_id: &SessionId) -> Result<u64> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| &row.session_id == session_id)
            .map(|row| row.seq)
            .max()
            .unwrap_or(0))
    }

    async fn delete_for_session(&self, session_id: &SessionId) -> Result<u64> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|row| &row.session_id != session_id);
        Ok((before - rows.len()) as u64)
    }
}

/// What a [`RecordingRuntime`] observed.
///
/// [`RecordingRuntime`] 观测到的内容。
#[derive(Debug, Default)]
pub struct RuntimeLog {
    /// Prompt previews, in submission order. / prompt 预览，按提交顺序。
    pub prompts: Vec<String>,
    /// Number of cancels received. / 收到的取消次数。
    pub cancels: usize,
    /// Permission decisions received. / 收到的权限决定。
    pub decisions: Vec<(PermissionId, PermissionDecision)>,
    /// Whether shutdown was called. / 是否调用过 shutdown。
    pub shutdown: bool,
}

/// An [`AgentRuntime`] that records what it is told instead of spawning anything.
///
/// 一个只记录指令、不真正启动进程的 [`AgentRuntime`]。
#[derive(Debug, Default)]
pub struct RecordingRuntime {
    log: Arc<Mutex<RuntimeLog>>,
}

impl RecordingRuntime {
    /// A fresh runtime.
    ///
    /// 新建一个运行时。
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Prompt previews recorded so far.
    ///
    /// 目前已记录的 prompt 预览。
    #[must_use]
    pub fn prompts(runtime: &Arc<Self>) -> Vec<String> {
        runtime.log.lock().unwrap().prompts.clone()
    }

    /// The full log.
    ///
    /// 完整的记录。
    #[must_use]
    pub fn log(runtime: &Arc<Self>) -> Arc<Mutex<RuntimeLog>> {
        Arc::clone(&runtime.log)
    }
}

#[async_trait]
impl AgentRuntime for RecordingRuntime {
    async fn launch(&self, request: LaunchRequest) -> Result<LaunchedAgent> {
        Ok(LaunchedAgent {
            acp_session_id: format!("acp_{}", request.session_id),
            handle: Arc::new(RecordingHandle {
                log: Arc::clone(&self.log),
            }),
        })
    }
}

#[derive(Debug)]
struct RecordingHandle {
    log: Arc<Mutex<RuntimeLog>>,
}

#[async_trait]
impl AgentSessionHandle for RecordingHandle {
    async fn submit_prompt(&self, blocks: Vec<PromptBlock>) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .prompts
            .push(PromptBlock::preview(&blocks));
        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        self.log.lock().unwrap().cancels += 1;
        Ok(())
    }

    async fn resolve_permission(
        &self,
        permission_id: &PermissionId,
        decision: PermissionDecision,
    ) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .decisions
            .push((permission_id.clone(), decision));
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        self.log.lock().unwrap().shutdown = true;
        Ok(())
    }

    fn is_alive(&self) -> bool {
        true
    }
}

/// A fully in-memory [`SessionManager`] with one configured agent, `mock`.
///
/// 一个完全内存的 [`SessionManager`]，预配一个名为 `mock` 的 Agent。
pub async fn memory_manager() -> (Arc<SessionManager>, Arc<RecordingRuntime>) {
    let runtime = RecordingRuntime::new();
    let manager = SessionManager::new(
        SessionManagerConfig::new(MachineId::new("machine_test")),
        Arc::new(MemorySessionRepository::default()),
        Arc::new(MemoryEventStore::default()),
        Arc::clone(&runtime) as Arc<dyn AgentRuntime>,
        AgentCatalog::new([AgentDescriptor {
            id: AgentId::new("mock"),
            name: "Mock".to_owned(),
            command: "true".to_owned(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }]),
        Arc::new(SystemClock) as Arc<dyn Clock>,
    );
    (manager, runtime)
}

/// A throwaway workspace path for tests.
///
/// 测试用的一次性工作区路径。
#[must_use]
pub fn test_workspace() -> PathBuf {
    PathBuf::from("/tmp/agent-gateway-test")
}
