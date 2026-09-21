//! Daemon supervision for the desktop app.
//!
//! 桌面 App 的守护进程管理。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::state::{DaemonStatus, UiUpdateSender};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub binary: PathBuf,
}

pub fn spawn_supervisor(
    runtime: Arc<tokio::runtime::Runtime>,
    config: DaemonConfig,
    ui_tx: UiUpdateSender,
) -> oneshot::Sender<()> {
    let (stop_tx, mut stop_rx) = oneshot::channel();

    runtime.spawn(async move {
        let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Starting));

        let mut command = Command::new(&config.binary);
        command
            .arg("run")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Failed {
                    message: error.to_string(),
                }));
                return;
            }
        };

        let pid = child.id();
        let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Running {
            pid,
        }));

        if let Some(stdout) = child.stdout.take() {
            let tx = ui_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(crate::state::UiUpdate::DaemonLog(line));
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let tx = ui_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(crate::state::UiUpdate::DaemonLog(format!(
                        "[stderr] {line}"
                    )));
                }
            });
        }

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) if status.success() => {
                        debug!(?status, "daemon exited cleanly");
                        let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Stopped));
                    }
                    Ok(status) => {
                        let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Failed {
                            message: format!("daemon exited with {status}"),
                        }));
                    }
                    Err(error) => {
                        let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Failed {
                            message: error.to_string(),
                        }));
                    }
                }
            }
            _ = &mut stop_rx => {
                warn!("stopping daemon on request");
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = ui_tx.send(crate::state::UiUpdate::Daemon(DaemonStatus::Stopped));
            }
        }
    });

    stop_tx
}
