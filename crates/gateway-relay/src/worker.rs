//! Keeps the relay's view of this machine current.
//!
//! One task owns three jobs, because they share the same failure mode ("the
//! relay is unreachable") and the same recovery ("try again, and re-register
//! when it comes back"):
//!
//! 1. register on startup,
//! 2. heartbeat on an interval,
//! 3. translate session status changes into content-free push notifications.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use gateway_core::bus::SessionLifecycle;
use gateway_core::machine::Machine;
use gateway_core::manager::SessionManager;
use gateway_core::session::SessionStatus;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::client::RelayClient;
use crate::protocol::{PushEvent, PushKind};

/// What the relay worker knows about its own connectivity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayStatus {
    /// No relay configured.
    Disabled,
    /// Registration has not succeeded yet.
    Connecting,
    /// Registered and heartbeating.
    Registered,
    /// The last call failed.
    Unreachable {
        /// Why.
        reason: String,
    },
}

impl RelayStatus {
    /// Short state name, for `/health`.
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Registered => "registered",
            Self::Unreachable { .. } => "unreachable",
        }
    }

    /// Human-readable detail.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Unreachable { reason } => Some(reason.clone()),
            _ => None,
        }
    }
}

/// Background relay presence and push worker.
#[derive(Debug)]
pub struct RelayWorker {
    status: Arc<RwLock<RelayStatus>>,
    stop: Arc<AtomicBool>,
}

impl RelayWorker {
    /// A worker for a gateway with no relay configured.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            status: Arc::new(RwLock::new(RelayStatus::Disabled)),
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Start registering, heartbeating and pushing in the background.
    #[must_use]
    pub fn start(
        client: RelayClient,
        manager: Arc<SessionManager>,
        machine: Machine,
        heartbeat_interval: Duration,
    ) -> Self {
        let status = Arc::new(RwLock::new(RelayStatus::Connecting));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Self {
            status: Arc::clone(&status),
            stop: Arc::clone(&stop),
        };

        let presence_client = client.clone();
        let presence_status = Arc::clone(&status);
        let presence_stop = Arc::clone(&stop);
        tokio::spawn(async move {
            presence_loop(
                presence_client,
                machine,
                heartbeat_interval,
                presence_status,
                presence_stop,
            )
            .await;
        });

        tokio::spawn(async move {
            push_loop(client, manager, stop).await;
        });

        worker
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> RelayStatus {
        self.status.read().expect("relay status lock").clone()
    }

    /// Ask both loops to stop.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

async fn presence_loop(
    client: RelayClient,
    machine: Machine,
    heartbeat_interval: Duration,
    status: Arc<RwLock<RelayStatus>>,
    stop: Arc<AtomicBool>,
) {
    let mut registered = false;
    while !stop.load(Ordering::Acquire) {
        let result = if registered {
            client.heartbeat(machine.public_endpoint.clone()).await
        } else {
            client.register(&machine).await
        };

        match result {
            Ok(()) => {
                if !registered {
                    info!(machine_id = %machine.id, "registered with the relay");
                }
                registered = true;
                set(&status, RelayStatus::Registered);
            }
            Err(error) => {
                // Fall back to registering: a relay that restarted may have
                // forgotten this machine entirely.
                registered = false;
                warn!(%error, "relay call failed");
                set(
                    &status,
                    RelayStatus::Unreachable {
                        reason: error.to_string(),
                    },
                );
            }
        }

        tokio::time::sleep(heartbeat_interval).await;
    }
    set(&status, RelayStatus::Disabled);
}

async fn push_loop(client: RelayClient, manager: Arc<SessionManager>, stop: Arc<AtomicBool>) {
    let mut lifecycle = manager.subscribe_lifecycle();
    while !stop.load(Ordering::Acquire) {
        let change = match lifecycle.recv().await {
            Ok(change) => change,
            // Missing a status change only costs a notification; the phone
            // still sees the truth when it reconnects and replays.
            Err(RecvError::Lagged(missed)) => {
                warn!(missed, "push worker lagged");
                continue;
            }
            Err(RecvError::Closed) => return,
        };

        let Some(kind) = push_kind(&change) else {
            continue;
        };
        let agent_name = manager
            .get_session(&change.session_id)
            .await
            .map_or_else(|_| "Agent".to_owned(), |session| session.agent_name);

        if let Err(error) = client
            .push_events(vec![PushEvent {
                session_id: change.session_id,
                agent_name,
                kind,
                occurred_at: Utc::now(),
            }])
            .await
        {
            warn!(%error, "cannot deliver push event");
        }
    }
}

/// Which status changes are worth waking a phone for.
///
/// `Running` deliberately produces nothing: the user just sent the prompt.
fn push_kind(change: &SessionLifecycle) -> Option<PushKind> {
    if change.created {
        return None;
    }
    match change.status {
        SessionStatus::WaitingPermission => Some(PushKind::PermissionRequired),
        SessionStatus::Idle => Some(PushKind::TurnCompleted),
        SessionStatus::Failed => Some(PushKind::SessionFailed),
        SessionStatus::Disconnected => Some(PushKind::SessionDisconnected),
        SessionStatus::Running | SessionStatus::Cancelling | SessionStatus::Completed => None,
    }
}

fn set(status: &Arc<RwLock<RelayStatus>>, next: RelayStatus) {
    *status.write().expect("relay status lock") = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::ids::SessionId;

    fn change(status: SessionStatus, created: bool) -> SessionLifecycle {
        SessionLifecycle {
            session_id: SessionId::new("sess_1"),
            status,
            created,
        }
    }

    #[test]
    fn only_states_a_user_cares_about_produce_a_notification() {
        assert_eq!(
            push_kind(&change(SessionStatus::WaitingPermission, false)),
            Some(PushKind::PermissionRequired)
        );
        assert_eq!(
            push_kind(&change(SessionStatus::Idle, false)),
            Some(PushKind::TurnCompleted)
        );
        assert_eq!(push_kind(&change(SessionStatus::Running, false)), None);
        // Session creation is initiated by the user; no notification.
        assert_eq!(push_kind(&change(SessionStatus::Idle, true)), None);
    }
}
