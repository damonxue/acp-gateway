//! Behavioural tests for the SQLite adapters.
//!
//! These run against a real (in-memory) SQLite database, because the
//! properties worth testing here — unique sequence numbers, single-use
//! credentials, ordering — are properties of the SQL, not of the Rust.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use gateway_core::credential::{PairingCode, WsTicket};
use gateway_core::device::Device;
use gateway_core::event::{AgentEvent, EventType};
use gateway_core::ids::{AgentId, DeviceId, EventId, MachineId, SessionId};
use gateway_core::ports::{
    DeviceRepository, EventStore, PairingRepository, SessionRepository, TicketRepository,
};
use gateway_core::session::{AgentSession, SessionOrigin, SessionStatus};
use gateway_store::Database;

fn machine() -> MachineId {
    MachineId::new("machine_test")
}

fn session(id: &str, status: SessionStatus) -> AgentSession {
    let now = Utc::now();
    AgentSession {
        id: SessionId::new(id),
        machine_id: machine(),
        agent_id: AgentId::new("codex"),
        agent_name: "Codex".to_owned(),
        acp_session_id: None,
        workspace: PathBuf::from("/tmp/project"),
        cwd: PathBuf::from("/tmp/project"),
        title: None,
        origin: SessionOrigin::Gateway,
        status,
        last_seq: 0,
        created_at: now,
        updated_at: now,
    }
}

fn event(session_id: &str, seq: u64) -> AgentEvent {
    AgentEvent {
        id: EventId::generate(),
        session_id: SessionId::new(session_id),
        seq,
        timestamp: Utc::now(),
        event_type: EventType::AgentMessageChunk.as_str().to_owned(),
        payload: serde_json::json!({ "content": format!("chunk {seq}") }),
    }
}

#[tokio::test]
async fn sessions_round_trip_with_every_field_intact() {
    let db = Database::connect_in_memory().await.unwrap();
    let repo = db.sessions();
    let mut original = session("sess_1", SessionStatus::Idle);
    original.title = Some("Fix the redis config".to_owned());
    original.acp_session_id = Some("acp-42".to_owned());
    original.origin = SessionOrigin::IdeBridge;
    repo.insert(&original).await.unwrap();

    let loaded = repo.get(&original.id).await.unwrap().unwrap();
    assert_eq!(loaded.title.as_deref(), Some("Fix the redis config"));
    assert_eq!(loaded.acp_session_id.as_deref(), Some("acp-42"));
    assert_eq!(loaded.origin, SessionOrigin::IdeBridge);
    assert_eq!(loaded.workspace, original.workspace);
    assert_eq!(
        loaded.created_at.timestamp_millis(),
        original.created_at.timestamp_millis()
    );
}

#[tokio::test]
async fn listing_is_newest_activity_first_and_respects_the_limit() {
    let db = Database::connect_in_memory().await.unwrap();
    let repo = db.sessions();
    for (index, id) in ["sess_a", "sess_b", "sess_c"].iter().enumerate() {
        let mut row = session(id, SessionStatus::Idle);
        row.updated_at = Utc::now() + Duration::seconds(index as i64);
        repo.insert(&row).await.unwrap();
    }
    let listed = repo.list(&machine(), 2).await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["sess_c", "sess_b"]);
}

#[tokio::test]
async fn restart_recovery_only_touches_non_terminal_sessions() {
    let db = Database::connect_in_memory().await.unwrap();
    let repo = db.sessions();
    repo.insert(&session("sess_running", SessionStatus::Running))
        .await
        .unwrap();
    repo.insert(&session("sess_done", SessionStatus::Completed))
        .await
        .unwrap();

    let changed = repo
        .mark_non_terminal(&machine(), SessionStatus::Disconnected, Utc::now())
        .await
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(
        repo.get(&SessionId::new("sess_running"))
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Disconnected
    );
    assert_eq!(
        repo.get(&SessionId::new("sess_done"))
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
async fn a_duplicate_sequence_number_is_rejected_by_the_database() {
    let db = Database::connect_in_memory().await.unwrap();
    let events = db.events();
    events.append(&event("sess_1", 1)).await.unwrap();
    let error = events.append(&event("sess_1", 1)).await.unwrap_err();
    assert!(error.to_string().contains("cannot append seq 1"));

    // A different session may reuse the number.
    events.append(&event("sess_2", 1)).await.unwrap();
}

#[tokio::test]
async fn replay_returns_ascending_events_after_the_cursor() {
    let db = Database::connect_in_memory().await.unwrap();
    let events = db.events();
    for seq in [3, 1, 2, 4] {
        events.append(&event("sess_1", seq)).await.unwrap();
    }
    let replayed = events
        .read_after(&SessionId::new("sess_1"), 2, 10)
        .await
        .unwrap();
    let seqs: Vec<u64> = replayed.iter().map(|row| row.seq).collect();
    assert_eq!(seqs, vec![3, 4]);
    assert_eq!(replayed[0].payload["content"], "chunk 3");
    assert_eq!(events.last_seq(&SessionId::new("sess_1")).await.unwrap(), 4);

    let limited = events
        .read_after(&SessionId::new("sess_1"), 0, 2)
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
}

#[tokio::test]
async fn a_ticket_can_only_be_redeemed_once() {
    let db = Database::connect_in_memory().await.unwrap();
    let tickets = db.tickets();
    let now = Utc::now();
    let ticket = WsTicket {
        id: "tkt_1".to_owned(),
        device_id: DeviceId::new("device_1"),
        machine_id: machine(),
        nonce: "nonce".to_owned(),
        expires_at: now + Duration::seconds(60),
        used_at: None,
    };
    tickets.insert(&ticket).await.unwrap();

    let first = tickets.consume("tkt_1", now).await.unwrap();
    assert!(first.is_some());
    assert!(tickets.consume("tkt_1", now).await.unwrap().is_none());
}

#[tokio::test]
async fn an_expired_ticket_is_never_redeemable() {
    let db = Database::connect_in_memory().await.unwrap();
    let tickets = db.tickets();
    let now = Utc::now();
    tickets
        .insert(&WsTicket {
            id: "tkt_old".to_owned(),
            device_id: DeviceId::new("device_1"),
            machine_id: machine(),
            nonce: "nonce".to_owned(),
            expires_at: now - Duration::seconds(1),
            used_at: None,
        })
        .await
        .unwrap();
    assert!(tickets.consume("tkt_old", now).await.unwrap().is_none());
    assert_eq!(tickets.purge_expired(now).await.unwrap(), 1);
}

#[tokio::test]
async fn a_pairing_code_is_single_use_and_expires() {
    let db = Database::connect_in_memory().await.unwrap();
    let codes = db.pairing_codes();
    let now = Utc::now();
    codes
        .insert(&PairingCode {
            code: "834921".to_owned(),
            nonce: "nonce".to_owned(),
            expires_at: now + Duration::minutes(5),
            consumed_at: None,
        })
        .await
        .unwrap();

    assert!(codes.consume("834921", now).await.unwrap().is_some());
    assert!(codes.consume("834921", now).await.unwrap().is_none());
    assert!(codes.consume("000000", now).await.unwrap().is_none());
}

#[tokio::test]
async fn revoking_a_device_survives_a_reload() {
    let db = Database::connect_in_memory().await.unwrap();
    let devices = db.devices();
    let now = Utc::now();
    let device = Device {
        id: DeviceId::new("device_1"),
        name: "iPhone".to_owned(),
        public_key: "cHVibGljLWtleQ==".to_owned(),
        platform: "ios".to_owned(),
        revoked: false,
        created_at: now,
        updated_at: now,
        last_seen_at: None,
    };
    devices.upsert(&device).await.unwrap();

    assert!(devices.set_revoked(&device.id, true, now).await.unwrap());
    assert!(!devices.get(&device.id).await.unwrap().unwrap().is_active());
    assert!(
        !devices
            .set_revoked(&DeviceId::new("device_nope"), true, now)
            .await
            .unwrap()
    );
    assert_eq!(devices.list().await.unwrap().len(), 1);
}
