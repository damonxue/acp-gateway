//! End-to-end tests against a real ACP agent process.
//!
//! The agent is `fake-acp-agent`, a binary in this crate that speaks genuine
//! ACP over stdio. These tests therefore exercise the whole local path:
//! `SessionManager` → `AcpAgentRuntime` → child process → mapper → event log →
//! subscribers, including process spawning and JSON-RPC framing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gateway_acp::AcpAgentRuntime;
use gateway_core::agent::{AgentDescriptor, AgentRuntime, PromptBlock};
use gateway_core::bus::EventSubscription;
use gateway_core::event::{AgentEvent, EventType};
use gateway_core::ids::AgentId;
use gateway_core::manager::{
    AgentCatalog, CreateSessionSpec, SessionManager, SessionManagerConfig,
};
use gateway_core::permission::PermissionDecision;
use gateway_core::ports::{Clock, EventStore, SessionRepository, SystemClock};
use gateway_core::session::SessionStatus;
use gateway_core::testing::{MemoryEventStore, MemorySessionRepository};
use gateway_core::{GatewayError, MachineId};

/// Generous: the first run of a spawned binary can be slow on a loaded CI box.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

fn manager(command: &str) -> Arc<SessionManager> {
    SessionManager::new(
        SessionManagerConfig::new(MachineId::new("machine_test")),
        Arc::new(MemorySessionRepository::default()) as Arc<dyn SessionRepository>,
        Arc::new(MemoryEventStore::default()) as Arc<dyn EventStore>,
        Arc::new(AcpAgentRuntime::default()) as Arc<dyn AgentRuntime>,
        AgentCatalog::new([AgentDescriptor {
            id: AgentId::new("fake"),
            name: "Fake".to_owned(),
            command: command.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }]),
        Arc::new(SystemClock) as Arc<dyn Clock>,
    )
}

fn fake_agent_manager() -> Arc<SessionManager> {
    manager(env!("CARGO_BIN_EXE_fake-acp-agent"))
}

fn spec() -> CreateSessionSpec {
    CreateSessionSpec {
        agent_id: AgentId::new("fake"),
        workspace: std::env::temp_dir(),
        cwd: None,
        additional_directories: Vec::new(),
    }
}

/// Await the first event matching `predicate`, failing the test on timeout.
async fn wait_for(
    events: &mut EventSubscription,
    what: &str,
    predicate: impl Fn(&AgentEvent) -> bool,
) -> AgentEvent {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("event stream stayed open");
        if predicate(&next) {
            return (*next).clone();
        }
    }
}

fn is_type(event: &AgentEvent, event_type: EventType) -> bool {
    event.event_type == event_type.as_str()
}

#[tokio::test]
async fn a_prompt_streams_chunks_and_tool_calls_back_to_subscribers() {
    let manager = fake_agent_manager();
    let session = manager.create_session(spec()).await.unwrap();
    let mut events = manager.subscribe(&session.id).unwrap();

    manager
        .send_prompt(&session.id, vec![PromptBlock::text("what is failing?")])
        .await
        .unwrap();

    let chunk = wait_for(&mut events, "an agent chunk", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;
    assert_eq!(chunk.payload["content"]["text"], "Looking at ");

    let tool_call = wait_for(&mut events, "a tool call", |event| {
        is_type(event, EventType::ToolCall)
    })
    .await;
    assert_eq!(tool_call.payload["title"], "cargo test");

    let completed = wait_for(&mut events, "turn completion", |event| {
        is_type(event, EventType::SessionCompleted)
    })
    .await;
    assert_eq!(completed.payload["stop_reason"], "end_turn");

    // Replay must show the same history, gap-free, in order.
    let replayed = manager.replay_events(&session.id, 0, None).await.unwrap();
    let seqs: Vec<u64> = replayed.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(
        manager.get_session(&session.id).await.unwrap().status,
        SessionStatus::Idle
    );

    manager.close_session(&session.id).await.unwrap();
}

#[tokio::test]
async fn a_permission_request_blocks_the_agent_until_a_client_answers() {
    let manager = fake_agent_manager();
    let session = manager.create_session(spec()).await.unwrap();
    let mut events = manager.subscribe(&session.id).unwrap();

    manager
        .send_prompt(&session.id, vec![PromptBlock::text("needs permission")])
        .await
        .unwrap();

    let request = wait_for(&mut events, "a permission request", |event| {
        is_type(event, EventType::PermissionRequest)
    })
    .await;
    assert_eq!(request.payload["title"], "rm -rf target");

    let snapshot = manager.get_snapshot(&session.id).await.unwrap();
    assert_eq!(snapshot.session.status, SessionStatus::WaitingPermission);
    let pending = snapshot
        .pending_permissions
        .first()
        .expect("the request is pending");

    manager
        .respond_permission(&session.id, &pending.id, PermissionDecision::approved(true))
        .await
        .unwrap();

    let verdict = wait_for(&mut events, "the agent's verdict", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;
    assert_eq!(verdict.payload["content"]["text"], "permission:allow");

    wait_for(&mut events, "turn completion", |event| {
        is_type(event, EventType::SessionCompleted)
    })
    .await;
    let snapshot = manager.get_snapshot(&session.id).await.unwrap();
    assert!(snapshot.pending_permissions.is_empty());
    assert_eq!(snapshot.session.status, SessionStatus::Idle);

    manager.close_session(&session.id).await.unwrap();
}

#[tokio::test]
async fn denying_a_permission_tells_the_agent_no() {
    let manager = fake_agent_manager();
    let session = manager.create_session(spec()).await.unwrap();
    let mut events = manager.subscribe(&session.id).unwrap();

    manager
        .send_prompt(&session.id, vec![PromptBlock::text("needs permission")])
        .await
        .unwrap();
    wait_for(&mut events, "a permission request", |event| {
        is_type(event, EventType::PermissionRequest)
    })
    .await;

    let pending = manager
        .get_snapshot(&session.id)
        .await
        .unwrap()
        .pending_permissions
        .remove(0);
    manager
        .respond_permission(
            &session.id,
            &pending.id,
            PermissionDecision::approved(false),
        )
        .await
        .unwrap();

    let verdict = wait_for(&mut events, "the agent's verdict", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;
    assert_eq!(verdict.payload["content"]["text"], "permission:deny");

    manager.close_session(&session.id).await.unwrap();
}

#[tokio::test]
async fn cancelling_stops_a_running_turn_without_killing_the_session() {
    let manager = fake_agent_manager();
    let session = manager.create_session(spec()).await.unwrap();
    let mut events = manager.subscribe(&session.id).unwrap();

    manager
        .send_prompt(&session.id, vec![PromptBlock::text("slow please")])
        .await
        .unwrap();
    wait_for(&mut events, "the first tick", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;

    manager.cancel_session(&session.id).await.unwrap();

    let completed = wait_for(&mut events, "the cancelled turn to end", |event| {
        is_type(event, EventType::SessionCompleted)
    })
    .await;
    assert_eq!(completed.payload["stop_reason"], "cancelled");

    // The session survives its cancelled turn and accepts the next prompt.
    manager
        .send_prompt(&session.id, vec![PromptBlock::text("and now?")])
        .await
        .unwrap();
    wait_for(&mut events, "the next turn", |event| {
        is_type(event, EventType::SessionCompleted)
    })
    .await;

    manager.close_session(&session.id).await.unwrap();
}

#[tokio::test]
async fn a_missing_agent_binary_fails_the_launch_with_a_useful_error() {
    let manager = manager("definitely-not-an-installed-agent");
    let error = manager.create_session(spec()).await.unwrap_err();
    assert!(
        matches!(error, GatewayError::AgentUnavailable(_)),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn two_subscribers_observe_the_same_stream() {
    let manager = fake_agent_manager();
    let session = manager.create_session(spec()).await.unwrap();
    let mut phone = manager.subscribe(&session.id).unwrap();
    let mut laptop = manager.subscribe(&session.id).unwrap();

    manager
        .send_prompt(&session.id, vec![PromptBlock::text("hello")])
        .await
        .unwrap();

    let on_phone = wait_for(&mut phone, "a chunk on the phone", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;
    let on_laptop = wait_for(&mut laptop, "the same chunk on the laptop", |event| {
        is_type(event, EventType::AgentMessageChunk)
    })
    .await;
    assert_eq!(on_phone.seq, on_laptop.seq);
    assert_eq!(on_phone.id, on_laptop.id);

    manager.close_session(&session.id).await.unwrap();
}

#[tokio::test]
async fn the_agent_can_read_and_write_files_inside_the_workspace() {
    // The fake agent does not exercise fs/*, so this checks the scope directly:
    // the guard is the security-relevant half of that feature.
    let dir = tempfile::tempdir().unwrap();
    let scope = gateway_acp::WorkspaceScope::new(dir.path(), &[]);
    let inside = dir.path().join("src/main.rs");
    scope.write_text(&inside, "fn main() {}").await.unwrap();
    assert_eq!(
        scope.read_text(&inside, None, None).await.unwrap(),
        "fn main() {}"
    );
    assert!(
        scope
            .read_text(&PathBuf::from("/etc/hosts"), None, None)
            .await
            .is_err()
    );
}
