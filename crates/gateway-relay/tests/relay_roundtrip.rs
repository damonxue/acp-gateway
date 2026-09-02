//! The gateway side and the reference relay, talking to each other.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gateway_auth::MachineIdentity;
use gateway_core::ids::SessionId;
use gateway_core::machine::Machine;
use gateway_relay::protocol::{PushEvent, PushKind};
use gateway_relay::{RelayClient, RelayState};
use serde_json::Value;

struct Relay {
    base: String,
}

async fn start_relay() -> Relay {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = gateway_relay::relay_router(RelayState::new());
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    Relay {
        base: format!("http://{addr}"),
    }
}

fn machine(identity: &MachineIdentity, endpoint: Option<&str>) -> Machine {
    let now = Utc::now();
    Machine {
        id: identity.machine_id().clone(),
        name: "MacBook Pro".to_owned(),
        platform: "macos".to_owned(),
        hostname: "host".to_owned(),
        version: "0.1.0".to_owned(),
        public_endpoint: endpoint.map(ToOwned::to_owned),
        created_at: now,
        updated_at: now,
        last_seen_at: None,
    }
}

#[tokio::test]
async fn a_gateway_registers_heartbeats_and_pushes() {
    let relay = start_relay().await;
    let identity = Arc::new(MachineIdentity::ephemeral());
    let client =
        RelayClient::new(&format!("{}/", relay.base), Arc::clone(&identity), None).unwrap();
    let machine = machine(&identity, Some("https://gw.example.com"));

    client.register(&machine).await.unwrap();

    let machines: Vec<Value> = reqwest::get(format!("{}/machines", relay.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0]["name"], "MacBook Pro");
    assert_eq!(machines[0]["online"], true);
    assert_eq!(machines[0]["endpoint"], "https://gw.example.com");

    client.heartbeat(None).await.unwrap();
    client
        .push_events(vec![PushEvent {
            session_id: SessionId::new("sess_1"),
            agent_name: "Claude Code".to_owned(),
            kind: PushKind::PermissionRequired,
            occurred_at: Utc::now(),
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn the_relay_refuses_requests_it_cannot_verify() {
    let relay = start_relay().await;
    let identity = Arc::new(MachineIdentity::ephemeral());
    let client =
        RelayClient::new(&format!("{}/", relay.base), Arc::clone(&identity), None).unwrap();

    // Heartbeat before registration: the relay has no key to check against.
    let error = client.heartbeat(None).await.unwrap_err();
    assert!(error.to_string().contains("404"), "got: {error}");

    client.register(&machine(&identity, None)).await.unwrap();

    // A different machine claiming the same id is rejected.
    let impostor = Arc::new(MachineIdentity::ephemeral());
    let impostor_client =
        RelayClient::new(&format!("{}/", relay.base), Arc::clone(&impostor), None).unwrap();
    let mut stolen = machine(&identity, None);
    stolen.name = "Impostor".to_owned();
    let error = impostor_client.register(&stolen).await.unwrap_err();
    assert!(error.to_string().contains("401"), "got: {error}");
}

#[tokio::test]
async fn forwarded_requests_need_a_reachable_machine_endpoint() {
    let relay = start_relay().await;
    let identity = Arc::new(MachineIdentity::ephemeral());
    let client =
        RelayClient::new(&format!("{}/", relay.base), Arc::clone(&identity), None).unwrap();
    client.register(&machine(&identity, None)).await.unwrap();

    let response = reqwest::Client::new()
        .post(format!(
            "{}/machines/{}/pairing/consume",
            relay.base,
            identity.machine_id()
        ))
        .json(&serde_json::json!({ "pairing_code": "123456" }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("no public endpoint")
    );
}
