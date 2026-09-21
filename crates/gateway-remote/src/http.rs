//! The local HTTP API.
//!
//! Two audiences, two access rules:
//!
//! * **Local callers** (the CLI, an IDE plugin, `curl`) get the full REST API
//!   over loopback.
//! * **Remote callers** (a phone, through the tunnel) get only what pairing
//!   requires — [`consume_pairing`] and [`issue_ticket`], each authenticated by
//!   its own credential — plus the WebSocket. Everything a remote client needs
//!   afterwards is available over that socket, so there is no second
//!   authenticated REST surface to secure.

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use gateway_core::agent::PromptBlock;
use gateway_core::error::{GatewayError, Result};
use gateway_core::event::AgentEvent;
use gateway_core::ids::{DeviceId, SessionId};
use gateway_core::machine::Machine;
use gateway_core::manager::{CreateSessionSpec, SessionSnapshot};
use gateway_core::session::AgentSession;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::protocol::{AgentSummary, PermissionAnswer, PromptInput};
use crate::state::{Access, AppState, ComponentHealth};
use crate::{bridge, web_ui, ws};

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // --- reachable from anywhere the gateway is reachable ---
        .route("/health", get(health))
        // The web client: public static assets, see `web_ui`.
        .route("/app", get(web_ui::index))
        .route("/app/", get(web_ui::index))
        .route("/app/{*path}", get(web_ui::asset))
        .route("/pairing/consume", post(consume_pairing))
        .route("/devices/{device_id}/ws-ticket", post(issue_ticket))
        .route("/remote", get(ws::remote_socket))
        .route("/ahp", get(ws::ahp_socket))
        // --- loopback only ---
        .route("/machines/current", get(current_machine))
        .route("/agents", get(list_agents))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/refresh", post(refresh_sessions))
        .route("/sessions/{session_id}", get(get_session))
        .route("/sessions/{session_id}/prompt", post(prompt))
        .route("/sessions/{session_id}/cancel", post(cancel))
        .route("/sessions/{session_id}/permission", post(permission))
        .route("/sessions/{session_id}/close", post(close_session))
        .route("/sessions/{session_id}/events", get(events))
        .route("/sessions/{session_id}/stream", get(ws::session_socket))
        .route("/ahp/status", get(ahp_status))
        .route("/ahp/bind", post(ahp_bind))
        .route("/ahp/unbind", post(ahp_unbind))
        .route("/wechat/bind", post(wechat_bind))
        .route("/wechat/unbind", post(wechat_unbind))
        .route(gateway_core::bridge::BRIDGE_PATH, get(bridge::socket))
        .route("/pairing/begin", post(begin_pairing))
        .route("/devices", get(list_devices))
        .route("/devices/{device_id}/revoke", post(revoke_device))
        .with_state(state)
}

/// `GET /health`
#[derive(Debug, Serialize)]
pub struct Health {
    status: &'static str,
    version: &'static str,
    pid: u32,
    machine: Machine,
    sessions: SessionCounts,
    components: Vec<ComponentHealth>,
}

#[derive(Debug, Serialize)]
struct SessionCounts {
    active: usize,
    created_total: usize,
}

async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        machine: state.machine(),
        sessions: SessionCounts {
            active: state.manager().active_count(),
            created_total: state.manager().created_total(),
        },
        components: state.component_health(),
    })
}

/// `GET /ahp/status` — local desktop control and QR login state.
async fn ahp_status(access: Access, State(state): State<AppState>) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let status = state.ahp().map(|ahp| ahp.status());
    Ok(Json(
        serde_json::json!({ "enabled": status.is_some(), "status": status }),
    ))
}

#[derive(Debug, Deserialize)]
struct AhpBindBody {
    session_id: String,
}

/// `POST /ahp/bind` — explicitly bind one existing session to AHP.
async fn ahp_bind(
    access: Access,
    State(state): State<AppState>,
    Json(body): Json<AhpBindBody>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let ahp = state
        .ahp()
        .ok_or_else(|| GatewayError::invalid_request("AHP is not configured"))?;
    ahp.bind(SessionId::new(body.session_id))
        .await
        .map_err(GatewayError::transport)?;
    Ok(Json(serde_json::json!({ "bound": true })))
}

/// `POST /ahp/unbind` — keep the WeChat login while detaching the session.
async fn ahp_unbind(access: Access, State(state): State<AppState>) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let ahp = state
        .ahp()
        .ok_or_else(|| GatewayError::invalid_request("AHP is not configured"))?;
    ahp.unbind().await.map_err(GatewayError::transport)?;
    Ok(Json(serde_json::json!({ "unbound": true })))
}

/// `POST /wechat/bind` — switch whichever local WeChat adapter is active.
async fn wechat_bind(
    access: Access,
    State(state): State<AppState>,
    Json(body): Json<AhpBindBody>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let session_id = SessionId::new(body.session_id);
    state.manager().get_session(&session_id).await?;
    let binder = state
        .wechat_binder()
        .ok_or_else(|| GatewayError::invalid_request("WeChat adapter is not configured"))?;
    binder
        .bind(session_id)
        .await
        .map_err(GatewayError::transport)?;
    Ok(Json(serde_json::json!({ "bound": true })))
}

/// `POST /wechat/unbind` — detach the local WeChat adapter.
async fn wechat_unbind(
    access: Access,
    State(state): State<AppState>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let binder = state
        .wechat_binder()
        .ok_or_else(|| GatewayError::invalid_request("WeChat adapter is not configured"))?;
    binder.unbind().await.map_err(GatewayError::transport)?;
    Ok(Json(serde_json::json!({ "unbound": true })))
}

async fn current_machine(access: Access, State(state): State<AppState>) -> ApiResult<Machine> {
    access.require_local()?;
    Ok(Json(state.machine()))
}

async fn list_agents(
    access: Access,
    State(state): State<AppState>,
) -> ApiResult<Vec<AgentSummary>> {
    access.require_local()?;
    Ok(Json(agent_summaries(&state)))
}

/// Agents advertised to clients.
pub(crate) fn agent_summaries(state: &AppState) -> Vec<AgentSummary> {
    state
        .manager()
        .catalog()
        .iter()
        .map(|descriptor| AgentSummary {
            id: descriptor.id.to_string(),
            name: descriptor.name.clone(),
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct SessionList {
    sessions: Vec<AgentSession>,
}

async fn list_sessions(access: Access, State(state): State<AppState>) -> ApiResult<SessionList> {
    access.require_local()?;
    Ok(Json(SessionList {
        sessions: state.manager().list_sessions().await?,
    }))
}

/// `POST /sessions/refresh` — ask attached ACP agents for fresh metadata.
async fn refresh_sessions(access: Access, State(state): State<AppState>) -> ApiResult<SessionList> {
    access.require_local()?;
    Ok(Json(SessionList {
        sessions: state.manager().refresh_sessions().await?,
    }))
}

/// `POST /sessions`
#[derive(Debug, Deserialize)]
pub struct CreateSessionBody {
    /// Configured agent id.
    pub agent_id: String,
    /// Project root.
    pub workspace: PathBuf,
    /// Working directory; defaults to `workspace`.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Extra directories the agent may touch.
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
}

async fn create_session(
    access: Access,
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> ApiResult<AgentSession> {
    access.require_local()?;
    let session = state
        .manager()
        .create_session(CreateSessionSpec {
            agent_id: body.agent_id.into(),
            workspace: body.workspace,
            cwd: body.cwd,
            additional_directories: body.additional_directories,
        })
        .await?;
    Ok(Json(session))
}

async fn get_session(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<SessionSnapshot> {
    access.require_local()?;
    Ok(Json(
        state
            .manager()
            .get_snapshot(&SessionId::new(session_id))
            .await?,
    ))
}

/// `POST /sessions/{id}/prompt`
#[derive(Debug, Deserialize)]
pub struct PromptBody {
    /// Text or content blocks.
    pub prompt: PromptInput,
}

async fn prompt(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<PromptBody>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    let blocks: Vec<PromptBlock> = body.prompt.into_blocks();
    state
        .manager()
        .send_prompt(&SessionId::new(session_id), blocks)
        .await?;
    Ok(Json(json!({ "accepted": true })))
}

async fn cancel(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    state
        .manager()
        .cancel_session(&SessionId::new(session_id))
        .await?;
    Ok(Json(json!({ "cancelling": true })))
}

async fn permission(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(answer): Json<PermissionAnswer>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    state
        .manager()
        .respond_permission(
            &SessionId::new(session_id),
            &answer.request_id,
            answer.decision(),
        )
        .await?;
    Ok(Json(json!({ "recorded": true })))
}

async fn close_session(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    state
        .manager()
        .close_session(&SessionId::new(session_id))
        .await?;
    Ok(Json(json!({ "closed": true })))
}

/// `GET /sessions/{id}/events`
#[derive(Debug, Deserialize)]
pub struct EventQuery {
    /// Return events with `seq` greater than this. Defaults to 0.
    #[serde(default)]
    pub after_seq: u64,
    /// Maximum number of events.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct EventPage {
    events: Vec<AgentEvent>,
}

async fn events(
    access: Access,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<EventPage> {
    access.require_local()?;
    let events = state
        .manager()
        .replay_events(&SessionId::new(session_id), query.after_seq, query.limit)
        .await?;
    Ok(Json(EventPage { events }))
}

/// `POST /pairing/begin`
#[derive(Debug, Deserialize)]
pub struct BeginPairingBody {
    /// Endpoint to advertise in the QR payload.
    #[serde(default)]
    pub endpoint: Option<String>,
}

async fn begin_pairing(
    access: Access,
    State(state): State<AppState>,
    body: Option<Json<BeginPairingBody>>,
) -> ApiResult<gateway_auth::PairingOffer> {
    access.require_local()?;
    let endpoint = body
        .and_then(|Json(body)| body.endpoint)
        .or_else(|| state.machine().public_endpoint);
    Ok(Json(state.auth().begin_pairing(endpoint).await?))
}

async fn consume_pairing(
    State(state): State<AppState>,
    Json(request): Json<gateway_auth::PairingRequest>,
) -> ApiResult<gateway_core::device::Device> {
    // Deliberately reachable from the tunnel: the pairing code *is* the
    // credential, and it is single-use and short-lived.
    Ok(Json(state.auth().complete_pairing(request).await?))
}

/// `POST /devices/{id}/ws-ticket`
#[derive(Debug, Deserialize)]
pub struct TicketRequest {
    /// RFC 3339 timestamp the device signed.
    pub issued_at: chrono::DateTime<chrono::Utc>,
    /// Base64 Ed25519 signature over the ticket challenge.
    pub signature: String,
}

async fn issue_ticket(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<TicketRequest>,
) -> ApiResult<gateway_core::credential::WsTicket> {
    let ticket = state
        .auth()
        .issue_ticket_for_signed_request(&DeviceId::new(device_id), body.issued_at, &body.signature)
        .await?;
    Ok(Json(ticket))
}

#[derive(Debug, Serialize)]
struct DeviceList {
    devices: Vec<gateway_core::device::Device>,
}

async fn list_devices(access: Access, State(state): State<AppState>) -> ApiResult<DeviceList> {
    access.require_local()?;
    Ok(Json(DeviceList {
        devices: state.auth().list_devices().await?,
    }))
}

async fn revoke_device(
    access: Access,
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    access.require_local()?;
    state
        .auth()
        .revoke_device(&DeviceId::new(device_id))
        .await?;
    Ok(Json(json!({ "revoked": true })))
}

/// Parse a session id from a path segment, rejecting empty ones.
pub(crate) fn parse_session_id(raw: &str) -> Result<SessionId> {
    if raw.trim().is_empty() {
        return Err(GatewayError::invalid_request(
            "session id must not be empty",
        ));
    }
    Ok(SessionId::new(raw))
}
