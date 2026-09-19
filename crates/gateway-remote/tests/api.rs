//! Tests against a real bound server: real sockets, real JSON, real tickets.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use gateway_auth::{AuthConfig, AuthService, MachineIdentity, PairingRequest};
use gateway_core::bridge::{BridgeMessage, DaemonMessage};
use gateway_core::event::{EventDraft, EventType};
use gateway_core::ids::{AgentId, MachineId, SessionId};
use gateway_core::machine::Machine;
use gateway_core::manager::CreateSessionSpec;
use gateway_core::ports::{Clock, SystemClock};
use gateway_core::testing::memory_manager;
use gateway_remote::{AppState, RemoteServer};
use gateway_store::Database;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;

struct Harness {
    base: String,
    state: AppState,
    machine_id: MachineId,
    device_key: SigningKey,
}

impl Harness {
    async fn start() -> Self {
        let (manager, _runtime) = memory_manager().await;
        let db = Database::connect_in_memory().await.unwrap();
        let identity = MachineIdentity::ephemeral();
        let machine_id = identity.machine_id().clone();
        let auth = Arc::new(AuthService::new(
            identity,
            db.devices(),
            db.pairing_codes(),
            db.tickets(),
            Arc::new(SystemClock) as Arc<dyn Clock>,
            AuthConfig::default(),
        ));
        let now = Utc::now();
        let machine = Machine {
            id: machine_id.clone(),
            name: "Test Machine".to_owned(),
            platform: "test".to_owned(),
            hostname: "test-host".to_owned(),
            version: "0.1.0".to_owned(),
            public_endpoint: None,
            created_at: now,
            updated_at: now,
            last_seen_at: None,
        };
        let state = AppState::new(manager, auth, machine, true, Vec::new());
        let server = RemoteServer::bind("127.0.0.1:0".parse().unwrap(), state.clone())
            .await
            .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            server.serve(std::future::pending()).await.ok();
        });
        Self {
            base: format!("http://{addr}"),
            state,
            machine_id,
            device_key: SigningKey::generate(&mut rand::rngs::OsRng),
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.replace("http://", "ws://"))
    }

    async fn create_session(&self) -> SessionId {
        let response = self
            .client()
            .post(self.url("/sessions"))
            .json(&json!({ "agent_id": "mock", "workspace": "/tmp/project" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        SessionId::new(body["id"].as_str().unwrap().to_owned())
    }

    /// Pair a device and obtain a fresh single-use ticket.
    async fn ticket(&self) -> String {
        let engine = base64::engine::general_purpose::STANDARD;
        let offer: Value = self
            .client()
            .post(self.url("/pairing/begin"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let device: Value = self
            .client()
            .post(self.url("/pairing/consume"))
            .json(&PairingRequest {
                pairing_code: offer["pairing_code"].as_str().unwrap().to_owned(),
                nonce: offer["nonce"].as_str().unwrap().to_owned(),
                device_name: "iPhone".to_owned(),
                platform: "ios".to_owned(),
                public_key: engine.encode(self.device_key.verifying_key().as_bytes()),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let device_id = device["id"].as_str().unwrap().to_owned();

        let issued_at = Utc::now();
        let challenge = format!(
            "ws-ticket:{}:{}:{}",
            self.machine_id,
            device_id,
            issued_at.timestamp_millis()
        );
        let signature = engine.encode(self.device_key.sign(challenge.as_bytes()).to_bytes());

        let ticket: Value = self
            .client()
            .post(self.url(&format!("/devices/{device_id}/ws-ticket")))
            .json(&json!({ "issued_at": issued_at, "signature": signature }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ticket["id"].as_str().unwrap().to_owned()
    }
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("timed out waiting for a websocket message")
            .expect("socket stayed open")
            .expect("valid frame");
        if let tungstenite::Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

#[tokio::test]
async fn health_reports_the_machine_and_never_requires_a_credential() {
    let harness = Harness::start().await;
    let body: Value = reqwest::get(harness.url("/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["machine"]["hostname"], "test-host");
}

#[tokio::test]
async fn bridge_socket_adopts_a_session_and_receives_daemon_commands() {
    let harness = Harness::start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();

    socket
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Adopt {
                agent_id: AgentId::new("zed-codex"),
                agent_name: "Codex via Zed".to_owned(),
                acp_session_id: "acp-zed-1".to_owned(),
                workspace: "/tmp/project".into(),
                cwd: "/tmp/project".into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    let adopted = next_json(&mut socket).await;
    assert_eq!(adopted["type"], "adopted");
    let session_id = adopted["session_id"].as_str().unwrap().to_owned();

    let snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{session_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["origin"], "ide_bridge");
    assert_eq!(snapshot["connected"], true);

    socket
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Event {
                event_type: EventType::AgentMessageChunk,
                payload: json!({ "content": { "text": "mirrored" } }),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let events: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{session_id}/events?after_seq=0")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events["events"][1]["type"], "agent_message_chunk");
    assert_eq!(
        events["events"][1]["payload"]["content"]["text"],
        "mirrored"
    );

    harness
        .client()
        .post(harness.url(&format!("/sessions/{session_id}/prompt")))
        .json(&json!({ "prompt": "from phone" }))
        .send()
        .await
        .unwrap();
    let command: DaemonMessage = serde_json::from_value(next_json(&mut socket).await).unwrap();
    match command {
        DaemonMessage::Prompt { blocks } => {
            assert_eq!(blocks[0], gateway_core::PromptBlock::text("from phone"));
        }
        other => panic!("expected prompt command, got {other:?}"),
    }
}

#[tokio::test]
async fn bridge_socket_detaches_on_request_and_marks_history_disconnected() {
    let harness = Harness::start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();

    socket
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Adopt {
                agent_id: AgentId::new("zed-codex"),
                agent_name: "Codex via Zed".to_owned(),
                acp_session_id: "acp-zed-2".to_owned(),
                workspace: "/tmp/project".into(),
                cwd: "/tmp/project".into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let adopted: Value = next_json(&mut socket).await;
    let session_id = adopted["session_id"].as_str().unwrap().to_owned();

    socket
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Detach {
                reason: "zed quit".to_owned(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.next()).await;

    let snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{session_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["connected"], false);
    assert_eq!(snapshot["status"], "disconnected");
}

#[tokio::test]
async fn bridge_reconnect_reattaches_the_same_acp_session() {
    let harness = Harness::start().await;
    let adopt = || {
        serde_json::to_string(&BridgeMessage::Adopt {
            agent_id: AgentId::new("zed-codex"),
            agent_name: "Codex via Zed".to_owned(),
            acp_session_id: "acp-reconnect".to_owned(),
            workspace: "/tmp/project".into(),
            cwd: "/tmp/project".into(),
        })
        .unwrap()
    };

    let (mut first, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();
    first
        .send(tungstenite::Message::Text(adopt().into()))
        .await
        .unwrap();
    let first_id = next_json(&mut first).await["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    first
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Detach {
                reason: "temporary daemon disconnect".to_owned(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), first.next()).await;

    let (mut second, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();
    second
        .send(tungstenite::Message::Text(adopt().into()))
        .await
        .unwrap();
    let second_id = next_json(&mut second).await["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(second_id, first_id);

    let snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{second_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["connected"], true);
    assert_eq!(snapshot["status"], "idle");
}

#[tokio::test]
async fn two_bridge_connections_adopt_independent_sessions() {
    let harness = Harness::start().await;
    let (mut left, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();
    let (mut right, _) = tokio_tungstenite::connect_async(harness.ws_url("/bridge"))
        .await
        .unwrap();

    left.send(tungstenite::Message::Text(
        serde_json::to_string(&BridgeMessage::Adopt {
            agent_id: AgentId::new("zed-codex"),
            agent_name: "Codex via Zed".to_owned(),
            acp_session_id: "acp-left".to_owned(),
            workspace: "/tmp/left".into(),
            cwd: "/tmp/left".into(),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    right
        .send(tungstenite::Message::Text(
            serde_json::to_string(&BridgeMessage::Adopt {
                agent_id: AgentId::new("zed-gemini"),
                agent_name: "Gemini via Zed".to_owned(),
                acp_session_id: "acp-right".to_owned(),
                workspace: "/tmp/right".into(),
                cwd: "/tmp/right".into(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    let left_adopted = next_json(&mut left).await;
    let right_adopted = next_json(&mut right).await;
    let left_id = left_adopted["session_id"].as_str().unwrap().to_owned();
    let right_id = right_adopted["session_id"].as_str().unwrap().to_owned();
    assert_ne!(left_id, right_id);

    let left_snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{left_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let right_snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{right_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(left_snapshot["origin"], "ide_bridge");
    assert_eq!(right_snapshot["origin"], "ide_bridge");
    assert_eq!(left_snapshot["connected"], true);
    assert_eq!(right_snapshot["connected"], true);
}

#[tokio::test]
async fn the_documented_session_endpoints_work_end_to_end() {
    let harness = Harness::start().await;
    let session_id = harness.create_session().await;

    let accepted: Value = harness
        .client()
        .post(harness.url(&format!("/sessions/{session_id}/prompt")))
        .json(&json!({ "prompt": "check the failing test" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted["accepted"], true);

    let events: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{session_id}/events?after_seq=0")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = events["events"].as_array().unwrap();
    assert_eq!(events[0]["type"], "session_created");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[1]["type"], "user_message");

    let listed: Value = harness
        .client()
        .get(harness.url("/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_sessions_and_agents_get_the_documented_status_codes() {
    let harness = Harness::start().await;
    let missing = harness
        .client()
        .get(harness.url("/sessions/sess_nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let bad_agent = harness
        .client()
        .post(harness.url("/sessions"))
        .json(&json!({ "agent_id": "nope", "workspace": "/tmp" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_agent.status(), 503);

    let malformed = harness
        .client()
        .post(harness.url("/sessions"))
        .json(&json!({ "workspace": "/tmp" }))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 422);
}

#[tokio::test]
async fn tunnelled_requests_cannot_reach_the_local_api() {
    let harness = Harness::start().await;
    // Same loopback socket, but the Cloudflare headers say it was forwarded.
    let response = harness
        .client()
        .get(harness.url("/sessions"))
        .header("cf-connecting-ip", "203.0.113.7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn a_remote_socket_requires_a_valid_single_use_ticket() {
    let harness = Harness::start().await;
    let ticket = harness.ticket().await;

    let (mut socket, _) =
        tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}")))
            .await
            .expect("the first use of the ticket is accepted");
    let hello = next_json(&mut socket).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["machine"]["hostname"], "test-host");

    // The same ticket a second time is refused.
    let replay =
        tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}"))).await;
    assert!(replay.is_err(), "a ticket must not be reusable");

    let forged =
        tokio_tungstenite::connect_async(harness.ws_url("/remote?ticket=tkt_made_up")).await;
    assert!(forged.is_err(), "an unknown ticket must not be accepted");
}

#[tokio::test]
async fn subscribing_replays_history_then_streams_live_events() {
    let harness = Harness::start().await;
    let session_id = harness.create_session().await;
    let manager = Arc::clone(harness.state.manager());

    // Three events exist before the client ever connects.
    for index in 0..3 {
        manager
            .append_event(
                &session_id,
                EventDraft::from_value(
                    EventType::AgentMessageChunk,
                    json!({ "content": { "text": format!("old {index}") } }),
                ),
            )
            .await
            .unwrap();
    }

    let ticket = harness.ticket().await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}")))
            .await
            .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "hello");

    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "subscribe", "session_id": session_id, "after_seq": 1 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    // Replay starts after seq 1 and ends with a `subscribed` marker.
    let mut seen = Vec::new();
    loop {
        let message = next_json(&mut socket).await;
        if message["type"] == "subscribed" {
            assert_eq!(message["last_seq"], 4);
            break;
        }
        seen.push(message["seq"].as_u64().unwrap());
    }
    assert_eq!(seen, vec![2, 3, 4]);

    // Live events follow, with no gap and no repeat.
    manager
        .append_event(
            &session_id,
            EventDraft::from_value(
                EventType::AgentMessageChunk,
                json!({ "content": { "text": "live" } }),
            ),
        )
        .await
        .unwrap();
    let live = next_json(&mut socket).await;
    assert_eq!(live["seq"], 5);
    assert_eq!(live["type"], "agent_message_chunk");
    assert_eq!(live["payload"]["content"]["text"], "live");
}

#[tokio::test]
async fn a_remote_client_can_drive_a_session_over_the_socket() {
    let harness = Harness::start().await;
    let ticket = harness.ticket().await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}")))
            .await
            .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "hello");

    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "create_session", "agent_id": "mock", "workspace": "/tmp/project" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let created = next_json(&mut socket).await;
    assert_eq!(created["type"], "session_created");
    let session_id = created["session"]["id"].as_str().unwrap().to_owned();

    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "subscribe", "session_id": session_id })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        if next_json(&mut socket).await["type"] == "subscribed" {
            break;
        }
    }

    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "prompt", "session_id": session_id, "prompt": "go" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let echoed = next_json(&mut socket).await;
    assert_eq!(echoed["type"], "user_message");
    assert_eq!(echoed["payload"]["content"][0]["text"], "go");

    // A command for a session that does not exist is an error, not a
    // disconnection.
    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "prompt", "session_id": "sess_nope", "prompt": "go" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let error = next_json(&mut socket).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "session_not_found");

    socket
        .send(tungstenite::Message::Text(
            json!({ "type": "ping" }).to_string().into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "pong");
}

#[tokio::test]
async fn two_remote_clients_see_the_same_events() {
    let harness = Harness::start().await;
    let session_id = harness.create_session().await;

    let mut sockets = Vec::new();
    for _ in 0..2 {
        let ticket = harness.ticket().await;
        let (mut socket, _) =
            tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}")))
                .await
                .unwrap();
        next_json(&mut socket).await;
        socket
            .send(tungstenite::Message::Text(
                json!({ "type": "subscribe", "session_id": session_id })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            if next_json(&mut socket).await["type"] == "subscribed" {
                break;
            }
        }
        sockets.push(socket);
    }

    harness
        .state
        .manager()
        .append_event(
            &session_id,
            EventDraft::from_value(EventType::AgentMessageChunk, json!({ "content": "shared" })),
        )
        .await
        .unwrap();

    for socket in &mut sockets {
        let event = next_json(socket).await;
        assert_eq!(event["payload"]["content"], "shared");
    }
}

#[tokio::test]
async fn the_local_session_stream_endpoint_replays_from_a_cursor() {
    let harness = Harness::start().await;
    let session_id = harness.create_session().await;
    harness
        .state
        .manager()
        .append_event(
            &session_id,
            EventDraft::from_value(EventType::AgentMessageChunk, json!({ "content": "one" })),
        )
        .await
        .unwrap();

    let (mut socket, _) = tokio_tungstenite::connect_async(
        harness.ws_url(&format!("/sessions/{session_id}/stream?after_seq=1")),
    )
    .await
    .unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "hello");
    let replayed = next_json(&mut socket).await;
    assert_eq!(replayed["seq"], 2);
    assert_eq!(replayed["payload"]["content"], "one");
}

#[tokio::test]
async fn a_revoked_device_cannot_open_a_socket() {
    let harness = Harness::start().await;
    let ticket = harness.ticket().await;

    let devices: Value = harness
        .client()
        .get(harness.url("/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let device_id = devices["devices"][0]["id"].as_str().unwrap().to_owned();
    harness
        .client()
        .post(harness.url(&format!("/devices/{device_id}/revoke")))
        .send()
        .await
        .unwrap();

    let refused =
        tokio_tungstenite::connect_async(harness.ws_url(&format!("/remote?ticket={ticket}"))).await;
    assert!(refused.is_err(), "revocation must invalidate live tickets");
}

#[tokio::test]
async fn agents_are_advertised_from_configuration_only() {
    let harness = Harness::start().await;
    let agents: Value = harness
        .client()
        .get(harness.url("/agents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let agents = agents.as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], AgentId::new("mock").as_str());
}

#[tokio::test]
async fn creating_a_session_over_http_uses_the_configured_agent() {
    let harness = Harness::start().await;
    let session_id = harness.create_session().await;
    let snapshot: Value = harness
        .client()
        .get(harness.url(&format!("/sessions/{session_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["agent_id"], "mock");
    assert_eq!(snapshot["status"], "idle");
    assert_eq!(snapshot["connected"], true);

    let _ = CreateSessionSpec {
        agent_id: AgentId::new("mock"),
        workspace: "/tmp".into(),
        cwd: None,
        additional_directories: Vec::new(),
    };
}
