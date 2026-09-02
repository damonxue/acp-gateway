//! A reference relay.
//!
//! Enough of a control plane to run the full remote path end to end — machine
//! directory, presence, pairing hand-off, ticket hand-off and push intake —
//! and no more. A production relay would add user accounts, durable storage
//! and real APNs/FCM delivery; the *shape* below is what such a relay must
//! keep, in particular:
//!
//! * every gateway request is verified against the machine's registered public
//!   key, so the relay stores no credential that could impersonate a machine;
//! * pairing and ticket requests are **forwarded** to the gateway rather than
//!   answered here, because the gateway is the authority for its own devices;
//! * nothing that could carry agent output is stored, and push intake keeps
//!   only the latest state per session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::MachineId;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::protocol::{HeartbeatRequest, MachineSummary, PushRequest, RegisterRequest, challenge};

/// A machine is online if it heartbeated within this window.
const ONLINE_WINDOW: Duration = Duration::from_secs(90);
/// How far a signed request's timestamp may be from the relay's clock.
const SIGNATURE_WINDOW: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
struct MachineRecord {
    summary: MachineSummary,
    public_key: String,
}

/// In-memory relay state.
#[derive(Clone, Debug, Default)]
pub struct RelayState {
    machines: Arc<RwLock<HashMap<MachineId, MachineRecord>>>,
    http: reqwest::Client,
}

impl RelayState {
    /// Fresh, empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, id: &MachineId) -> Result<MachineRecord> {
        self.machines
            .read()
            .expect("relay state lock")
            .get(id)
            .cloned()
            .ok_or_else(|| GatewayError::MachineNotFound(id.to_string()))
    }

    /// Verify a signed gateway request against the registered public key.
    fn verify(
        &self,
        id: &MachineId,
        action: &str,
        issued_at: DateTime<Utc>,
        signature: &str,
        public_key: Option<&str>,
    ) -> Result<()> {
        let skew = (Utc::now() - issued_at).num_seconds().abs();
        if skew > SIGNATURE_WINDOW.as_secs() as i64 {
            return Err(GatewayError::AuthenticationFailed(
                "signed request is stale".into(),
            ));
        }
        let key = match public_key {
            // Registration carries its own key: trust on first use, and from
            // then on the key is pinned.
            Some(key) => key.to_owned(),
            None => self.record(id)?.public_key,
        };
        if let Ok(existing) = self.record(id)
            && public_key.is_some_and(|key| key != existing.public_key)
        {
            return Err(GatewayError::AuthenticationFailed(
                "machine key does not match the registered one".into(),
            ));
        }
        verify_signature(&key, challenge(action, id, issued_at).as_bytes(), signature)
    }

    fn endpoint_of(&self, id: &MachineId) -> Result<String> {
        self.record(id)?.summary.endpoint.ok_or_else(|| {
            GatewayError::transport("machine has no public endpoint; is its tunnel running?")
        })
    }
}

/// Build the relay router.
pub fn router(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        // gateway -> relay
        .route("/machines/register", post(register))
        .route("/machines/{machine_id}/heartbeat", post(heartbeat))
        .route("/machines/{machine_id}/push-events", post(push_events))
        // mobile -> relay
        .route("/machines", get(list_machines))
        .route(
            "/machines/{machine_id}/pairing/consume",
            post(consume_pairing),
        )
        .route("/machines/{machine_id}/ws-ticket", post(ws_ticket))
        .with_state(state)
}

/// Serve the reference relay until `shutdown` resolves.
///
/// # Errors
/// Fails if the address cannot be bound.
pub async fn serve(
    addr: SocketAddr,
    state: RelayState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| GatewayError::transport(format!("cannot bind {addr}: {error}")))?;
    info!(address = %addr, "reference relay listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| GatewayError::transport(format!("relay stopped: {error}")))
}

struct RelayError(GatewayError);

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let status = match self.0.kind() {
            gateway_core::ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
            gateway_core::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            gateway_core::ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
            gateway_core::ErrorKind::Forbidden => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_GATEWAY,
        };
        (
            status,
            Json(json!({ "code": self.0.code(), "message": self.0.to_string() })),
        )
            .into_response()
    }
}

impl From<GatewayError> for RelayError {
    fn from(error: GatewayError) -> Self {
        Self(error)
    }
}

type RelayResult<T> = std::result::Result<Json<T>, RelayError>;

async fn register(
    State(state): State<RelayState>,
    Json(body): Json<RegisterRequest>,
) -> RelayResult<serde_json::Value> {
    state.verify(
        &body.machine_id,
        "register",
        body.issued_at,
        &body.signature,
        Some(&body.public_key),
    )?;
    let now = Utc::now();
    state.machines.write().expect("relay state lock").insert(
        body.machine_id.clone(),
        MachineRecord {
            summary: MachineSummary {
                machine_id: body.machine_id.clone(),
                name: body.name,
                platform: body.platform,
                endpoint: body.endpoint,
                online: true,
                last_seen_at: Some(now),
            },
            public_key: body.public_key,
        },
    );
    info!(machine_id = %body.machine_id, "machine registered");
    Ok(Json(json!({ "registered": true })))
}

async fn heartbeat(
    State(state): State<RelayState>,
    Path(machine_id): Path<String>,
    Json(body): Json<HeartbeatRequest>,
) -> RelayResult<serde_json::Value> {
    let machine_id = MachineId::new(machine_id);
    state.verify(
        &machine_id,
        "heartbeat",
        body.issued_at,
        &body.signature,
        None,
    )?;
    let mut machines = state.machines.write().expect("relay state lock");
    let record = machines
        .get_mut(&machine_id)
        .ok_or_else(|| GatewayError::MachineNotFound(machine_id.to_string()))?;
    record.summary.last_seen_at = Some(Utc::now());
    record.summary.online = true;
    if body.endpoint.is_some() {
        record.summary.endpoint = body.endpoint;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn push_events(
    State(state): State<RelayState>,
    Path(machine_id): Path<String>,
    Json(body): Json<PushRequest>,
) -> RelayResult<serde_json::Value> {
    let machine_id = MachineId::new(machine_id);
    state.verify(&machine_id, "push", body.issued_at, &body.signature, None)?;
    for event in &body.events {
        // A production relay would forward this to APNs/FCM. It must not store
        // anything beyond what is here — which is, deliberately, only a state.
        info!(
            machine_id = %machine_id,
            session_id = %event.session_id,
            kind = ?event.kind,
            "push notification: {}",
            event.kind.message(&event.agent_name)
        );
    }
    Ok(Json(json!({ "accepted": body.events.len() })))
}

async fn list_machines(State(state): State<RelayState>) -> Json<Vec<MachineSummary>> {
    let now = Utc::now();
    let machines = state
        .machines
        .read()
        .expect("relay state lock")
        .values()
        .map(|record| {
            let mut summary = record.summary.clone();
            summary.online = summary
                .last_seen_at
                .is_some_and(|seen| (now - seen).num_seconds() <= ONLINE_WINDOW.as_secs() as i64);
            summary
        })
        .collect();
    Json(machines)
}

/// Forward a pairing attempt to the machine that issued the code.
async fn consume_pairing(
    State(state): State<RelayState>,
    Path(machine_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> RelayResult<serde_json::Value> {
    let machine_id = MachineId::new(machine_id);
    let endpoint = state.endpoint_of(&machine_id)?;
    Ok(Json(
        forward(&state.http, &format!("{endpoint}/pairing/consume"), body).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct TicketProxyRequest {
    device_id: String,
    #[serde(flatten)]
    body: serde_json::Value,
}

/// Forward a signed ticket request to the machine.
///
/// The relay cannot mint tickets: it has no access to the machine's device
/// registry, which is exactly the property that keeps a compromised relay from
/// granting itself a session.
async fn ws_ticket(
    State(state): State<RelayState>,
    Path(machine_id): Path<String>,
    Json(request): Json<TicketProxyRequest>,
) -> RelayResult<serde_json::Value> {
    let machine_id = MachineId::new(machine_id);
    let endpoint = state.endpoint_of(&machine_id)?;
    let url = format!("{endpoint}/devices/{}/ws-ticket", request.device_id);
    Ok(Json(forward(&state.http, &url, request.body).await?))
}

async fn forward(
    http: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let response = http
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| GatewayError::transport(format!("cannot reach the machine: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        warn!(%status, %url, "machine refused a forwarded request");
        return Err(GatewayError::transport(format!(
            "machine returned {status}"
        )));
    }
    response
        .json()
        .await
        .map_err(|error| GatewayError::transport(format!("malformed machine response: {error}")))
}

fn verify_signature(public_key: &str, message: &[u8], signature: &str) -> Result<()> {
    let engine = base64::engine::general_purpose::STANDARD;
    let key_bytes: [u8; 32] = engine
        .decode(public_key)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| GatewayError::AuthenticationFailed("public key is invalid".into()))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| GatewayError::AuthenticationFailed("public key is invalid".into()))?;
    let signature = engine
        .decode(signature)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or_else(|| GatewayError::AuthenticationFailed("signature is malformed".into()))?;
    key.verify(message, &signature)
        .map_err(|_| GatewayError::AuthenticationFailed("signature does not verify".into()))
}
