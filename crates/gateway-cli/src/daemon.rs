//! `agent-gateway run` — wiring and lifecycle.

use std::sync::Arc;
use std::time::Duration;

use crate::adapters::AdapterRuntime;
use anyhow::{Context, Result};
use chrono::Utc;
use gateway_acp::AcpAgentRuntime;
use gateway_ahp::{AhpStatus, AhpSupervisor};
use gateway_auth::{AuthConfig, AuthService, MachineIdentity};
use gateway_config::{GatewayConfig, TunnelMode};
use gateway_core::agent::AgentRuntime;
use gateway_core::machine::Machine;
use gateway_core::manager::{AgentCatalog, SessionManager, SessionManagerConfig};
use gateway_core::ports::{Clock, MachineRepository, SystemClock};
use gateway_core::session::SessionOrigin;
use gateway_relay::{RelayClient, RelayStatus, RelayWorker};
use gateway_remote::{AppState, ComponentHealth, HealthSource, RemoteServer, SessionBinder};
use gateway_store::Database;
use gateway_tunnel::{TunnelLaunch, TunnelSpec, TunnelStatus, TunnelSupervisor};
use gateway_wechat::{
    Binding as WechatBinding, CredentialStore, HttpWeixinApi, Journal, KeychainCredentialStore,
    SqliteJournal, WechatSupervisor,
};
use tracing::{info, warn};

/// How often expired pairing codes and tickets are swept.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

/// Start every subsystem and serve until the process is asked to stop.
pub(crate) async fn run(config: GatewayConfig) -> Result<()> {
    let data_dir = config
        .ensure_data_dir()
        .context("the data directory is not usable")?;
    info!(?data_dir, "starting agent gateway");

    let database = Database::connect(&config.database_path())
        .await
        .context("cannot open the gateway database")?;
    let identity = MachineIdentity::load_or_create(&config.identity_dir())
        .context("cannot load the machine identity")?;
    let clock = Arc::new(SystemClock) as Arc<dyn Clock>;

    // The tunnel is started early: its hostname is part of the machine record
    // that the relay and the pairing QR code advertise.
    let tunnel = Arc::new(start_tunnel(&config));
    let machine = machine_record(&config, &identity, tunnel.public_endpoint());
    database
        .machines()
        .upsert(&machine)
        .await
        .context("cannot record this machine")?;

    let manager = SessionManager::new(
        SessionManagerConfig::new(identity.machine_id().clone()),
        database.sessions(),
        database.events(),
        Arc::new(AcpAgentRuntime::default()) as Arc<dyn AgentRuntime>,
        AgentCatalog::new(config.agent_descriptors()),
        Arc::clone(&clock),
    );
    manager
        .recover_after_restart()
        .await
        .context("cannot reconcile sessions after restart")?;

    let auth = Arc::new(AuthService::new(
        identity.clone(),
        database.devices(),
        database.pairing_codes(),
        database.tickets(),
        Arc::clone(&clock),
        AuthConfig {
            ticket_ttl: config.security.ticket_ttl,
            pairing_ttl: config.security.pairing_ttl,
        },
    ));

    let relay = Arc::new(start_relay(&config, &identity, &manager, &machine)?);
    let ahp = Arc::new(start_ahp(&config, &identity, &manager)?);
    let wechat = Arc::new(start_wechat(&config, &identity, &manager, &database).await?);
    let wechat_binder = Arc::new(WechatChannelBinder {
        ahp: Arc::clone(&ahp),
        wechat: Arc::clone(&wechat),
    });
    let mut adapters = AdapterRuntime::start(&config, Arc::clone(&manager))?;
    let wechat_auto_bind = wechat
        .is_enabled()
        .then(|| tokio::spawn(auto_bind_wechat(Arc::clone(&manager), Arc::clone(&wechat))));

    let mut health_sources: Vec<Arc<dyn HealthSource>> = vec![
        Arc::new(TunnelHealth(Arc::clone(&tunnel))) as Arc<dyn HealthSource>,
        Arc::new(RelayHealth(Arc::clone(&relay))) as Arc<dyn HealthSource>,
        Arc::new(AhpHealth(Arc::clone(&ahp))) as Arc<dyn HealthSource>,
        Arc::new(WechatHealth(Arc::clone(&wechat))) as Arc<dyn HealthSource>,
    ];
    if config.lark.as_ref().is_some_and(|section| section.enabled) {
        health_sources.push(Arc::new(ConfiguredHealth {
            name: "lark",
            state: "configured",
        }));
    }
    if config
        .telegram
        .as_ref()
        .is_some_and(|section| section.enabled)
    {
        health_sources.push(Arc::new(ConfiguredHealth {
            name: "telegram",
            state: "configured",
        }));
    }

    let state = AppState::new(
        Arc::clone(&manager),
        Arc::clone(&auth),
        machine.clone(),
        config.security.trust_loopback,
        health_sources,
    )
    .with_ahp(Arc::clone(&ahp))
    .with_wechat_binder(wechat_binder);

    let maintenance = tokio::spawn(maintenance_loop(
        Arc::clone(&auth),
        database.clone(),
        machine.id.clone(),
    ));

    let server = RemoteServer::bind(config.bind_addr(), state)
        .await
        .context("cannot bind the gateway API")?;
    info!(
        address = %server.local_addr(),
        agents = config.agents.len(),
        ahp = %format!("ws://{}/ahp", server.local_addr()),
        "gateway ready; AHP Host endpoint available"
    );
    if let Some(endpoint) = tunnel.public_endpoint() {
        info!(%endpoint, "public endpoint");
    }

    let result = server.serve(shutdown_signal()).await;

    info!("shutting down");
    maintenance.abort();
    relay.shutdown();
    tunnel.shutdown();
    ahp.shutdown();
    wechat.shutdown();
    adapters.shutdown();
    if let Some(task) = wechat_auto_bind {
        task.abort();
    }
    manager.shutdown_all().await;
    database.close().await;
    result.context("the gateway API stopped unexpectedly")
}

async fn start_wechat(
    config: &GatewayConfig,
    identity: &MachineIdentity,
    manager: &Arc<SessionManager>,
    database: &Database,
) -> Result<WechatSupervisor> {
    let Some(section) = config.wechat.as_ref().filter(|section| section.enabled) else {
        return Ok(WechatSupervisor::disabled());
    };
    let configured_binding = section.binding.as_ref();
    let store = KeychainCredentialStore::default();
    let credentials = match store.load() {
        Ok(Some(credentials)) => credentials,
        Ok(None) => {
            warn!("wechat is enabled but not logged in; run `agent-gateway wechat login`");
            return Ok(WechatSupervisor::disabled());
        }
        Err(error) => {
            warn!(error = %error, "wechat credential store unavailable; adapter is idle");
            return Ok(WechatSupervisor::disabled());
        }
    };
    let api = Arc::new(
        HttpWeixinApi::with_timeout(&credentials.base_url, section.poll_timeout)
            .context("invalid stored WeChat API base")?,
    );
    let journal = Arc::new(SqliteJournal::new(database.pool().clone()));
    let session_id = active_ide_session(manager)
        .await
        .or_else(|| {
            configured_binding
                .map(|binding| gateway_core::SessionId::new(binding.session_id.clone()))
        })
        .unwrap_or_else(|| gateway_core::SessionId::new("__auto_ide_session__"));
    let chat_id = configured_binding
        .map(|binding| binding.chat_id.clone())
        .unwrap_or_else(|| format!("local:{session_id}"));
    let binding = WechatBinding {
        // Keep one stable journal namespace while the active Zed session is
        // switched underneath the adapter.
        binding_id: format!("{}:wechat", identity.machine_id()),
        session_id,
        chat_id,
    };
    // Gateway-owned ACP processes do not survive a daemon restart, so restore
    // those sessions before starting the adapter. IDE-owned sessions are
    // restored by the bridge itself; the adapter waits for that reattachment.
    if let Ok(persisted) = manager.get_session(&binding.session_id).await {
        if persisted.origin == SessionOrigin::Gateway
            && let Err(error) = manager.resume_gateway_session(&binding.session_id).await
        {
            warn!(
                session_id = %binding.session_id,
                error = %error,
                "bound Gateway session could not be resumed; WeChat adapter will wait for an IDE session"
            );
        }
        if persisted.origin == SessionOrigin::IdeBridge {
            info!(
                session_id = %binding.session_id,
                "waiting for the IDE bridge to reattach the bound session"
            );
        }
    } else {
        info!(
            session_id = %binding.session_id,
            "WeChat adapter will bind automatically when an active IDE session appears"
        );
    }
    journal
        .recover_sending()
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    info!(session_id = %binding.session_id, "starting embedded WeChat adapter");
    Ok(WechatSupervisor::start_with_limits(
        Arc::clone(manager),
        api,
        journal,
        credentials,
        binding,
        section.max_text_bytes,
        section.max_chunk_bytes,
    ))
}

/// Pick the most recently updated live Zed/IDE session. The persistent list
/// alone is insufficient because a retained bridge session may be disconnected
/// until Zed reattaches its ACP process.
async fn active_ide_session(manager: &Arc<SessionManager>) -> Option<gateway_core::SessionId> {
    let sessions = manager.list_sessions().await.ok()?;
    for session in sessions {
        if session.origin != SessionOrigin::IdeBridge {
            continue;
        }
        let Ok(snapshot) = manager.get_snapshot(&session.id).await else {
            continue;
        };
        if snapshot.connected && snapshot.session.status.is_drivable() {
            return Some(session.id);
        }
    }
    None
}

/// Keep the embedded WeChat adapter pointed at the newest active Zed session.
/// A bridge reattach emits a lifecycle update, so this takes effect without a
/// daemon restart or another `wechat bind` command.
async fn auto_bind_wechat(manager: Arc<SessionManager>, wechat: Arc<WechatSupervisor>) {
    let mut lifecycle = manager.subscribe_lifecycle();
    bind_active_wechat(&manager, &wechat).await;
    loop {
        if lifecycle.recv().await.is_err() {
            return;
        }
        bind_active_wechat(&manager, &wechat).await;
    }
}

async fn bind_active_wechat(manager: &Arc<SessionManager>, wechat: &WechatSupervisor) {
    if let Some(session_id) = active_ide_session(manager).await
        && let Err(error) = wechat.rebind(session_id.clone()).await
    {
        warn!(session_id = %session_id, %error, "automatic WeChat session bind failed");
    }
}

fn start_ahp(
    config: &GatewayConfig,
    identity: &MachineIdentity,
    manager: &Arc<SessionManager>,
) -> Result<AhpSupervisor> {
    let Some(ahp) = &config.ahp else {
        return Ok(AhpSupervisor::disabled());
    };
    AhpSupervisor::start(
        ahp.endpoint.clone(),
        ahp.token.as_ref().map(|token| token.expose().to_owned()),
        identity.machine_id().clone(),
        Arc::clone(manager),
        ahp.reconnect_interval,
    )
    .map_err(|error| anyhow::anyhow!(error))
    .context("cannot start the AHP channel")
}

fn machine_record(
    config: &GatewayConfig,
    identity: &MachineIdentity,
    public_endpoint: Option<String>,
) -> Machine {
    let now = Utc::now();
    let hostname = hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown-host".to_owned());
    Machine {
        id: identity.machine_id().clone(),
        name: config
            .gateway
            .machine_name
            .clone()
            .unwrap_or_else(|| hostname.clone()),
        platform: std::env::consts::OS.to_owned(),
        hostname,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        public_endpoint,
        created_at: now,
        updated_at: now,
        last_seen_at: Some(now),
    }
}

fn start_tunnel(config: &GatewayConfig) -> TunnelSupervisor {
    let Some(tunnel) = &config.tunnel else {
        return TunnelSupervisor::disabled();
    };
    let launch = match tunnel.mode {
        TunnelMode::Token => TunnelLaunch::Token(
            tunnel
                .token
                .as_ref()
                .map(|secret| secret.expose().to_owned())
                .unwrap_or_default(),
        ),
        TunnelMode::Config => TunnelLaunch::Config(
            tunnel
                .config_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_default(),
        ),
    };
    TunnelSupervisor::start(TunnelSpec {
        binary: tunnel.binary.clone(),
        launch,
        hostname: tunnel.hostname.clone(),
    })
}

fn start_relay(
    config: &GatewayConfig,
    identity: &MachineIdentity,
    manager: &Arc<SessionManager>,
    machine: &Machine,
) -> Result<RelayWorker> {
    let Some(relay) = &config.relay else {
        return Ok(RelayWorker::disabled());
    };
    let client = RelayClient::new(
        &ensure_trailing_slash(&relay.endpoint),
        Arc::new(identity.clone()),
        relay.token.as_ref().map(|token| token.expose().to_owned()),
    )
    .context("cannot build the relay client")?;
    Ok(RelayWorker::start(
        client,
        Arc::clone(manager),
        machine.clone(),
        relay.heartbeat_interval,
    ))
}

/// `Url::join` drops the last path segment without a trailing slash, which
/// would turn `https://relay.example.com/api` into `https://relay.example.com/`.
fn ensure_trailing_slash(endpoint: &str) -> String {
    if endpoint.ends_with('/') {
        endpoint.to_owned()
    } else {
        format!("{endpoint}/")
    }
}

async fn maintenance_loop(
    auth: Arc<AuthService>,
    database: Database,
    machine_id: gateway_core::MachineId,
) {
    let machines = database.machines();
    loop {
        tokio::time::sleep(MAINTENANCE_INTERVAL).await;
        match auth.purge_expired().await {
            Ok(removed) if removed > 0 => info!(removed, "purged expired credentials"),
            Ok(_) => {}
            Err(error) => warn!(%error, "credential purge failed"),
        }
        if let Err(error) = machines.touch(&machine_id, Utc::now()).await {
            warn!(%error, "cannot record machine heartbeat");
        }
    }
}

/// Resolve on Ctrl-C or `SIGTERM`.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => warn!(%error, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received ctrl-c"),
        () = terminate => info!("received SIGTERM"),
    }
}

/// Adapts the tunnel supervisor to the API's health report.
#[derive(Debug)]
struct TunnelHealth(Arc<TunnelSupervisor>);

impl HealthSource for TunnelHealth {
    fn health(&self) -> ComponentHealth {
        let status: TunnelStatus = self.0.status();
        ComponentHealth {
            name: "tunnel".to_owned(),
            state: status.state_name().to_owned(),
            detail: status.detail(),
        }
    }
}

/// Adapts the relay worker to the API's health report.
#[derive(Debug)]
struct RelayHealth(Arc<RelayWorker>);

impl HealthSource for RelayHealth {
    fn health(&self) -> ComponentHealth {
        let status: RelayStatus = self.0.status();
        ComponentHealth {
            name: "relay".to_owned(),
            state: status.state_name().to_owned(),
            detail: status.detail(),
        }
    }
}

/// Adapts the outbound AHP channel to the API's health report.
#[derive(Debug)]
struct AhpHealth(Arc<AhpSupervisor>);

impl HealthSource for AhpHealth {
    fn health(&self) -> ComponentHealth {
        let status: AhpStatus = self.0.status();
        ComponentHealth {
            name: "ahp".to_owned(),
            state: status.state_name().to_owned(),
            detail: status.detail(),
        }
    }
}

#[derive(Debug)]
struct WechatHealth(Arc<WechatSupervisor>);

#[derive(Debug)]
struct WechatChannelBinder {
    ahp: Arc<AhpSupervisor>,
    wechat: Arc<WechatSupervisor>,
}

#[async_trait::async_trait]
impl SessionBinder for WechatChannelBinder {
    async fn bind(&self, session_id: gateway_core::SessionId) -> std::result::Result<(), String> {
        if self.wechat.is_enabled() {
            self.wechat.rebind(session_id).await
        } else {
            self.ahp.bind(session_id).await
        }
    }

    async fn unbind(&self) -> std::result::Result<(), String> {
        if self.wechat.is_enabled() {
            Err("embedded WeChat follows the newest active IDE session automatically".to_owned())
        } else {
            self.ahp.unbind().await
        }
    }
}

impl HealthSource for WechatHealth {
    fn health(&self) -> ComponentHealth {
        ComponentHealth {
            name: "wechat".to_owned(),
            state: if self.0.is_enabled() {
                "up"
            } else {
                "disabled"
            }
            .to_owned(),
            detail: self
                .0
                .binding_session_id()
                .map(|session_id| format!("session_id={session_id}")),
        }
    }
}

#[derive(Debug)]
struct ConfiguredHealth {
    name: &'static str,
    state: &'static str,
}

impl HealthSource for ConfiguredHealth {
    fn health(&self) -> ComponentHealth {
        ComponentHealth {
            name: self.name.to_owned(),
            state: self.state.to_owned(),
            detail: Some("webhook/token adapter".to_owned()),
        }
    }
}
