//! `agent-gateway sessions …`
//!
//! A minimal client for the local API — enough to run the whole product from a
//! terminal, and the fastest way to check that a newly configured agent works
//! before pointing a phone at it.

use anyhow::{Context, Result};
use clap::Subcommand;
use futures::StreamExt;
use gateway_config::GatewayConfig;
use gateway_core::event::AgentEvent;
use gateway_core::session::AgentSession;
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use crate::client::LocalClient;

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// List sessions on this machine.
    List,
    /// Create a session and launch its agent.
    New {
        /// Configured agent id.
        #[arg(long)]
        agent: String,
        /// Project directory. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Send a prompt to a session.
    Prompt {
        /// Session id.
        session_id: String,
        /// The prompt text.
        text: Vec<String>,
    },
    /// Cancel the running turn.
    Cancel {
        /// Session id.
        session_id: String,
    },
    /// Follow a session's events, replaying history first.
    Watch {
        /// Session id.
        session_id: String,
        /// Start after this sequence number.
        #[arg(long, default_value_t = 0)]
        after_seq: u64,
    },
}

#[derive(Debug, Deserialize)]
struct SessionList {
    sessions: Vec<AgentSession>,
}

/// Execute a `sessions` subcommand.
pub(crate) async fn run(config: &GatewayConfig, command: SessionCommand) -> Result<()> {
    let client = LocalClient::new(config);
    match command {
        SessionCommand::List => list(&client).await,
        SessionCommand::New { agent, workspace } => {
            let workspace = match workspace {
                Some(path) => path,
                None => std::env::current_dir()
                    .context("cannot read the current directory")?
                    .display()
                    .to_string(),
            };
            let session: AgentSession = client
                .post(
                    "/sessions",
                    &json!({ "agent_id": agent, "workspace": workspace }),
                )
                .await?;
            println!("{}", session.id);
            Ok(())
        }
        SessionCommand::Prompt { session_id, text } => {
            let prompt = text.join(" ");
            let _: serde_json::Value = client
                .post(
                    &format!("/sessions/{session_id}/prompt"),
                    &json!({ "prompt": prompt }),
                )
                .await?;
            println!("prompt accepted; follow it with: agent-gateway sessions watch {session_id}");
            Ok(())
        }
        SessionCommand::Cancel { session_id } => {
            let _: serde_json::Value = client
                .post(&format!("/sessions/{session_id}/cancel"), &json!({}))
                .await?;
            println!("cancelling {session_id}");
            Ok(())
        }
        SessionCommand::Watch {
            session_id,
            after_seq,
        } => watch(&client, &session_id, after_seq).await,
    }
}

async fn list(client: &LocalClient) -> Result<()> {
    let list: SessionList = client.get("/sessions").await?;
    if list.sessions.is_empty() {
        println!("no sessions yet; create one with `agent-gateway sessions new --agent <id>`");
        return Ok(());
    }
    println!(
        "{:<40} {:<10} {:<11} {:<18} {:<6} {:<28} WORKSPACE",
        "ID", "AGENT", "ORIGIN", "STATUS", "SEQ", "ACP SESSION"
    );
    for session in list.sessions {
        println!(
            "{:<40} {:<10} {:<11} {:<18} {:<6} {:<28} {}",
            session.id,
            session.agent_id,
            session.origin.as_str(),
            session.status.as_str(),
            session.last_seq,
            session.acp_session_id.as_deref().unwrap_or("-"),
            session.workspace.display()
        );
    }
    Ok(())
}

async fn watch(client: &LocalClient, session_id: &str, after_seq: u64) -> Result<()> {
    let url = client.ws_url(&format!(
        "/sessions/{session_id}/stream?after_seq={after_seq}"
    ));
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("cannot open {url}"))?;
    println!("watching {session_id}; press ctrl-c to stop");

    while let Some(message) = socket.next().await {
        let Message::Text(text) = message.context("the stream failed")? else {
            continue;
        };
        // Control frames (`hello`, `subscribed`, `error`) have no `seq`; events
        // do. Rendering only events keeps the output readable.
        match serde_json::from_str::<AgentEvent>(&text) {
            Ok(event) => println!(
                "{:>5}  {:<24} {}",
                event.seq, event.event_type, event.payload
            ),
            Err(_) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
                    && value["type"] == "error"
                {
                    eprintln!("error: {}", value["message"]);
                }
            }
        }
    }
    Ok(())
}
