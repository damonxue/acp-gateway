//! AHP Host-side WebSocket implementation.
//!
//! The Microsoft Rust crates currently provide the client SDK only.  This
//! module is the small host adapter needed by the gateway: it speaks the
//! official JSON-RPC envelopes and uses `ahp-types` for the wire vocabulary.
//! Session state remains authoritative in `SessionManager`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::Message;
use futures::{SinkExt, StreamExt};
use gateway_core::agent::PromptBlock;
use gateway_core::ids::SessionId;
use gateway_core::manager::{CreateSessionSpec, SessionManager};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const ROOT: &str = "ahp-root://";
const PROTOCOL_VERSION: &str = ahp_types::PROTOCOL_VERSION;
static SERVER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serve one already-upgraded AHP WebSocket connection.
pub async fn serve_connection<S>(socket: S, manager: Arc<SessionManager>)
where
    S: futures::Stream<Item = Result<Message, axum::Error>>
        + futures::Sink<Message, Error = axum::Error>
        + Unpin
        + Send
        + 'static,
{
    let (mut sink, mut incoming) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(256);
    let writer = tokio::spawn(async move {
        while let Some(value) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&value) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let mut subscriptions: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut initialized = false;
    while let Some(Ok(message)) = incoming.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let offered = params.get("protocolVersions").and_then(Value::as_array);
                let supported = offered.is_some_and(|items| {
                    items.iter().any(|v| v.as_str() == Some(PROTOCOL_VERSION))
                });
                if !supported {
                    if let Some(id) = id {
                        send_error(
                            &out_tx,
                            id,
                            -32005,
                            "unsupported protocol version",
                            json!({"supportedVersions":[PROTOCOL_VERSION]}),
                        )
                        .await;
                    }
                    continue;
                }
                initialized = true;
                let mut snapshots = Vec::new();
                if let Some(items) = params.get("initialSubscriptions").and_then(Value::as_array) {
                    for channel in items.iter().filter_map(Value::as_str) {
                        snapshots.push(snapshot(channel, &manager).await);
                    }
                }
                if let Some(id) = id {
                    reply(&out_tx, id, json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "serverSeq": SERVER_SEQ.load(Ordering::Relaxed),
                        "serverInfo": {"name":"agent-gateway","version":env!("CARGO_PKG_VERSION")},
                        "snapshots": snapshots
                    })).await;
                }
            }
            "ping" => {
                if let Some(id) = id {
                    reply(&out_tx, id, json!({})).await
                }
            }
            "subscribe" if initialized => {
                let channel = params
                    .get("channel")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let snap = snapshot(&channel, &manager).await;
                if let Some(id) = id {
                    reply(&out_tx, id, json!({"snapshot": snap})).await;
                }
                if let Some(session_id) = session_id_from_channel(&channel) {
                    if let Ok(mut events) = manager.subscribe(&session_id) {
                        let tx = out_tx.clone();
                        let channel_for_task = channel.clone();
                        let task = tokio::spawn(async move {
                            let mut answer = String::new();
                            while let Ok(event) = events.recv().await {
                                let event_type = event.event_type.as_str();
                                if event_type == "agent_message_chunk" {
                                    let content = event
                                        .payload
                                        .get("content")
                                        .and_then(|v| v.get("text"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    answer.push_str(content);
                                    send_action(&tx, &channel_for_task, json!({"type":"chat/delta","turnId":event.id.to_string(),"partId":"answer","content":content}), next_seq(), None).await;
                                } else if event_type == "session_completed" && !answer.is_empty() {
                                    send_action(&tx, &channel_for_task, json!({"type":"chat/turnComplete","turnId":event.id.to_string(),"state":"completed"}), next_seq(), None).await;
                                    answer.clear();
                                }
                            }
                        });
                        subscriptions.insert(channel, task);
                    }
                }
            }
            "unsubscribe" => {
                if let Some(channel) = params.get("channel").and_then(Value::as_str) {
                    if let Some(task) = subscriptions.remove(channel) {
                        task.abort();
                    }
                }
                if let Some(id) = id {
                    reply(&out_tx, id, json!({})).await;
                }
            }
            "createSession" if initialized => {
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let workspace = params
                    .pointer("/workingDirectories/0")
                    .and_then(Value::as_str)
                    .unwrap_or(".");
                let result = manager
                    .create_session(CreateSessionSpec {
                        agent_id: provider.to_owned().into(),
                        workspace: workspace.into(),
                        cwd: None,
                        additional_directories: Vec::new(),
                    })
                    .await;
                match result {
                    Ok(session) => {
                        if let Some(id) = id {
                            reply(
                                &out_tx,
                                id,
                                json!({"channel":format!("ahp-session:/{}", session.id)}),
                            )
                            .await;
                        }
                    }
                    Err(error) => {
                        if let Some(id) = id {
                            send_error(&out_tx, id, -32000, &error.to_string(), Value::Null).await;
                        }
                    }
                }
            }
            "disposeSession" if initialized => {
                if let Some(session_id) = params
                    .get("channel")
                    .and_then(Value::as_str)
                    .and_then(session_id_from_channel)
                {
                    if let Err(error) = manager.close_session(&session_id).await {
                        if let Some(id) = id {
                            send_error(&out_tx, id, -32000, &error.to_string(), Value::Null).await;
                        }
                        continue;
                    }
                }
                if let Some(id) = id {
                    reply(&out_tx, id, json!({})).await;
                }
            }
            "dispatchAction" if initialized => {
                let channel = params
                    .get("channel")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let action = params.get("action").cloned().unwrap_or(Value::Null);
                if action.get("type").and_then(Value::as_str) == Some("chat/turnStarted") {
                    if let Some(session_id) = session_id_from_channel(channel) {
                        let text = action
                            .pointer("/message/text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() {
                            if let Err(error) = manager
                                .send_prompt_from(
                                    &session_id,
                                    vec![PromptBlock::text(text)],
                                    Some("ahp"),
                                )
                                .await
                            {
                                if let Some(id) = id {
                                    send_error(
                                        &out_tx,
                                        id,
                                        -32000,
                                        &error.to_string(),
                                        Value::Null,
                                    )
                                    .await;
                                }
                                continue;
                            }
                        }
                    }
                }
                send_action(&out_tx, channel, action, next_seq(), None).await;
            }
            "reconnect" => {
                if let Some(id) = id {
                    reply(&out_tx, id, json!({"type":"snapshot","snapshots":[]})).await
                }
            }
            _ => {
                if let Some(id) = id {
                    send_error(&out_tx, id, -32601, "method not found", Value::Null).await
                }
            }
        }
    }
    for task in subscriptions.into_values() {
        task.abort();
    }
    writer.abort();
}

fn next_seq() -> u64 {
    SERVER_SEQ.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

async fn reply(tx: &mpsc::Sender<Value>, id: Value, result: Value) {
    let _ = tx
        .send(json!({"jsonrpc":"2.0","id":id,"result":result}))
        .await;
}
async fn send_error(tx: &mpsc::Sender<Value>, id: Value, code: i64, message: &str, data: Value) {
    let _ = tx
        .send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}}))
        .await;
}
async fn send_action(
    tx: &mpsc::Sender<Value>,
    channel: &str,
    action: Value,
    server_seq: u64,
    origin: Option<Value>,
) {
    let mut envelope = json!({"channel":channel,"action":action,"serverSeq":server_seq});
    if let Some(origin) = origin {
        envelope["origin"] = origin;
    }
    let _ = tx
        .send(json!({"jsonrpc":"2.0","method":"action","params":envelope}))
        .await;
}

fn session_id_from_channel(channel: &str) -> Option<SessionId> {
    channel
        .strip_prefix("ahp-session:/")
        .or_else(|| channel.strip_prefix("ahp-chat:/"))
        .map(|id| SessionId::new(id.to_owned()))
}

async fn snapshot(channel: &str, manager: &SessionManager) -> Value {
    if channel == ROOT {
        let sessions = manager.list_sessions().await.unwrap_or_default();
        return json!({"resource":ROOT,"state":{"agents":[],"activeSessions":sessions.len()},"fromSeq":0});
    }
    if let Some(id) = session_id_from_channel(channel) {
        if let Ok(session) = manager.get_session(&id).await {
            return json!({"resource":channel,"state":{"provider":session.agent_id.to_string(),"title":session.title.unwrap_or_else(||session.agent_name.clone()),"status":0,"lifecycle":"ready","activeClients":[],"chats":[],"_meta":{"gatewaySessionId":session.id.to_string()}},"fromSeq":session.last_seq});
        }
    }
    json!({"resource":channel,"state":{},"fromSeq":0})
}
