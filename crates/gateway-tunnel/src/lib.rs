//! # gateway-tunnel
//!
//! Supervises `cloudflared` so a phone on another network can reach the
//! gateway without the user configuring port forwarding.
//!
//! The gateway does **not** implement the tunnel protocol. It owns a child
//! process, restarts it with backoff, and reports what it knows:
//!
//! ```text
//!   Disabled ──configure──► Starting ──"Registered tunnel connection"──► Up
//!                             ▲                                        │
//!                             └──────── backoff restart ◄── exited ────┘
//! ```
//!
//! ## Secret handling
//!
//! A tunnel token is a bearer credential for the user's Cloudflare tunnel. It
//! is passed to the child process as an argument and is **never** written to a
//! log: [`TunnelSpec`] has a hand-written `Debug`, and
//! [`TunnelSpec::redacted_command`] is the only rendering of the command line
//! this crate will produce.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The line `cloudflared` prints once a connection is established.
const READY_MARKER: &str = "Registered tunnel connection";
/// First restart delay; doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// Longest restart delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A run lasting at least this long is treated as healthy: the next failure
/// starts from the minimum backoff again.
const STABLE_RUN: Duration = Duration::from_secs(30);

/// How the tunnel is launched.
#[derive(Clone)]
pub enum TunnelLaunch {
    /// `cloudflared tunnel --no-autoupdate run --token <token>`
    Token(String),
    /// `cloudflared tunnel --config <path> run`
    Config(PathBuf),
}

impl std::fmt::Debug for TunnelLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("Token(***)"),
            Self::Config(path) => write!(f, "Config({})", path.display()),
        }
    }
}

/// Everything needed to run the tunnel.
#[derive(Clone, Debug)]
pub struct TunnelSpec {
    /// Path to (or name of) the `cloudflared` executable.
    pub binary: String,
    /// Launch mode.
    pub launch: TunnelLaunch,
    /// Public hostname the tunnel exposes, for display and relay registration.
    pub hostname: Option<String>,
}

impl TunnelSpec {
    fn args(&self) -> Vec<String> {
        match &self.launch {
            TunnelLaunch::Token(token) => vec![
                "tunnel".to_owned(),
                "--no-autoupdate".to_owned(),
                "run".to_owned(),
                "--token".to_owned(),
                token.clone(),
            ],
            TunnelLaunch::Config(path) => vec![
                "tunnel".to_owned(),
                "--config".to_owned(),
                path.display().to_string(),
                "run".to_owned(),
            ],
        }
    }

    /// The command line with any secret replaced by `***`.
    #[must_use]
    pub fn redacted_command(&self) -> String {
        let mut parts = vec![self.binary.clone()];
        let mut redact_next = false;
        for arg in self.args() {
            if redact_next {
                parts.push("***".to_owned());
                redact_next = false;
                continue;
            }
            redact_next = arg == "--token";
            parts.push(arg);
        }
        parts.join(" ")
    }

    /// The public HTTPS endpoint, if a hostname is configured.
    #[must_use]
    pub fn public_endpoint(&self) -> Option<String> {
        self.hostname
            .as_ref()
            .map(|hostname| format!("https://{hostname}"))
    }
}

/// What the supervisor currently knows about the tunnel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TunnelStatus {
    /// No tunnel configured.
    Disabled,
    /// The process is running but has not reported a connection yet.
    Starting,
    /// At least one tunnel connection is registered.
    Up {
        /// Public hostname, when known.
        hostname: Option<String>,
    },
    /// The process exited and a restart is scheduled.
    Restarting {
        /// Why the last run ended.
        reason: String,
    },
    /// The tunnel cannot run at all (missing binary, bad configuration).
    Failed {
        /// What went wrong.
        reason: String,
    },
}

impl TunnelStatus {
    /// Short state name, for `/health`.
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Starting => "starting",
            Self::Up { .. } => "up",
            Self::Restarting { .. } => "restarting",
            Self::Failed { .. } => "failed",
        }
    }

    /// Human-readable detail, safe to expose.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Up { hostname } => hostname.clone(),
            Self::Restarting { reason } | Self::Failed { reason } => Some(reason.clone()),
            _ => None,
        }
    }
}

/// Keeps `cloudflared` running.
///
/// Dropping the supervisor cancels the loop; the child is killed with it,
/// because a tunnel outliving the gateway would expose a dead origin.
#[derive(Debug)]
pub struct TunnelSupervisor {
    status: Arc<RwLock<TunnelStatus>>,
    cancel: CancellationToken,
    spec: Option<TunnelSpec>,
}

impl TunnelSupervisor {
    /// A supervisor for a gateway without a tunnel.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            status: Arc::new(RwLock::new(TunnelStatus::Disabled)),
            cancel: CancellationToken::new(),
            spec: None,
        }
    }

    /// Start supervising in the background.
    #[must_use]
    pub fn start(spec: TunnelSpec) -> Self {
        let status = Arc::new(RwLock::new(TunnelStatus::Starting));
        let cancel = CancellationToken::new();
        info!(command = %spec.redacted_command(), "starting cloudflared");
        tokio::spawn(supervise(spec.clone(), Arc::clone(&status), cancel.clone()));
        Self {
            status,
            cancel,
            spec: Some(spec),
        }
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> TunnelStatus {
        self.status.read().expect("tunnel status lock").clone()
    }

    /// The public endpoint the tunnel exposes, if configured.
    #[must_use]
    pub fn public_endpoint(&self) -> Option<String> {
        self.spec.as_ref().and_then(TunnelSpec::public_endpoint)
    }

    /// Stop the tunnel and the supervision loop.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

impl Drop for TunnelSupervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn supervise(spec: TunnelSpec, status: Arc<RwLock<TunnelStatus>>, cancel: CancellationToken) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = tokio::time::Instant::now();
        let outcome = run_once(&spec, &status, &cancel).await;
        if cancel.is_cancelled() {
            set(&status, TunnelStatus::Disabled);
            return;
        }

        let reason = match outcome {
            Ok(reason) => reason,
            Err(error) => {
                // A missing binary will not fix itself; say so and stop
                // instead of retrying forever.
                set(
                    &status,
                    TunnelStatus::Failed {
                        reason: error.clone(),
                    },
                );
                warn!(%error, "cloudflared cannot be started");
                return;
            }
        };

        if started.elapsed() >= STABLE_RUN {
            backoff = MIN_BACKOFF;
        }
        warn!(%reason, retry_in = ?backoff, "cloudflared exited");
        set(&status, TunnelStatus::Restarting { reason });

        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = cancel.cancelled() => {
                set(&status, TunnelStatus::Disabled);
                return;
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Run the child once. `Ok(reason)` means it exited; `Err` means it could not
/// be started at all.
async fn run_once(
    spec: &TunnelSpec,
    status: &Arc<RwLock<TunnelStatus>>,
    cancel: &CancellationToken,
) -> Result<String, String> {
    set(status, TunnelStatus::Starting);
    let mut child: Child = Command::new(&spec.binary)
        .args(spec.args())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot run `{}`: {error}", spec.binary))?;

    // cloudflared reports progress on stderr; stdout is usually empty.
    let stderr = child.stderr.take();
    let hostname = spec.hostname.clone();
    let watcher_status = Arc::clone(status);
    let watcher = tokio::spawn(async move {
        let Some(stderr) = stderr else { return };
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains(READY_MARKER) {
                info!("cloudflared tunnel is up");
                set(
                    &watcher_status,
                    TunnelStatus::Up {
                        hostname: hostname.clone(),
                    },
                );
            }
            // cloudflared does not print the token, but the line is still
            // third-party output: keep it at debug.
            tracing::debug!(target: "cloudflared", "{line}");
        }
    });

    let exit = tokio::select! {
        status = child.wait() => match status {
            Ok(code) => format!("exited with {code}"),
            Err(error) => format!("wait failed: {error}"),
        },
        () = cancel.cancelled() => {
            child.start_kill().ok();
            child.wait().await.ok();
            "stopped by the gateway".to_owned()
        }
    };
    watcher.abort();
    Ok(exit)
}

fn set(status: &Arc<RwLock<TunnelStatus>>, next: TunnelStatus) {
    *status.write().expect("tunnel status lock") = next;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_spec() -> TunnelSpec {
        TunnelSpec {
            binary: "cloudflared".to_owned(),
            launch: TunnelLaunch::Token("super-secret-token".to_owned()),
            hostname: Some("gw.example.com".to_owned()),
        }
    }

    #[test]
    fn the_token_never_appears_in_debug_output_or_logs() {
        let spec = token_spec();
        assert!(!format!("{spec:?}").contains("super-secret-token"));
        assert_eq!(
            spec.redacted_command(),
            "cloudflared tunnel --no-autoupdate run --token ***"
        );
        // …while the real argument list still carries it.
        assert!(spec.args().contains(&"super-secret-token".to_owned()));
    }

    #[test]
    fn config_mode_builds_the_documented_command() {
        let spec = TunnelSpec {
            binary: "cloudflared".to_owned(),
            launch: TunnelLaunch::Config(PathBuf::from("/etc/cloudflared/config.yml")),
            hostname: None,
        };
        assert_eq!(
            spec.redacted_command(),
            "cloudflared tunnel --config /etc/cloudflared/config.yml run"
        );
        assert_eq!(spec.public_endpoint(), None);
    }

    #[tokio::test]
    async fn a_missing_binary_fails_fast_instead_of_looping() {
        let supervisor = TunnelSupervisor::start(TunnelSpec {
            binary: "definitely-not-cloudflared".to_owned(),
            launch: TunnelLaunch::Token("t".to_owned()),
            hostname: None,
        });
        for _ in 0..50 {
            if matches!(supervisor.status(), TunnelStatus::Failed { .. }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("expected the supervisor to report a failure");
    }

    #[tokio::test]
    async fn a_disabled_supervisor_reports_disabled() {
        let supervisor = TunnelSupervisor::disabled();
        assert_eq!(supervisor.status(), TunnelStatus::Disabled);
        assert_eq!(supervisor.public_endpoint(), None);
    }
}
