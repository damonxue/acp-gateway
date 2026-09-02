//! A scriptable ACP agent used to test the gateway end to end.
//!
//! Real agents need API keys, a network and minutes of patience, which makes
//! them useless in CI. This binary speaks real ACP over stdio and produces the
//! exact sequences the gateway must survive — streaming chunks, a tool call, a
//! permission request that blocks until answered, and cancellation.
//!
//! The behaviour is chosen by the prompt text:
//!
//! | Prompt contains | Behaviour |
//! |---|---|
//! | `permission` | requests permission, then reports the outcome and ends the turn |
//! | `slow` | streams a chunk every 50ms until cancelled |
//! | anything else | streams two chunks and one tool call, then ends the turn |

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Result, Stdio};

#[tokio::main]
async fn main() -> Result<()> {
    // Shared by the prompt handler and the cancel handler: ACP cancellation is
    // cooperative, so the agent has to notice it itself.
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancelled);

    Agent
        .builder()
        .name("fake-acp-agent")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _cx| {
                responder.respond(NewSessionResponse::new(SessionId::from("fake-session-1")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_notification: CancelNotification, _cx| {
                cancel_flag.store(true, Ordering::Release);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let cancelled = Arc::clone(&cancelled);
                cancelled.store(false, Ordering::Release);
                let text = request
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let session_id = request.session_id.clone();

                // Run the turn outside the dispatch loop, so incoming
                // `session/cancel` notifications keep being processed.
                cx.spawn({
                    let cx = cx.clone();
                    async move {
                        let outcome = run_turn(&cx, &session_id, &text, &cancelled).await?;
                        responder.respond(PromptResponse::new(outcome))
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}

async fn run_turn(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    prompt: &str,
    cancelled: &AtomicBool,
) -> Result<StopReason> {
    if prompt.contains("permission") {
        return permission_turn(cx, session_id).await;
    }
    if prompt.contains("slow") {
        return slow_turn(cx, session_id, cancelled).await;
    }

    for chunk in ["Looking at ", "your project."] {
        send_chunk(cx, session_id, chunk)?;
    }
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "cargo test".to_owned()).status(ToolCallStatus::Completed),
        ),
    ))?;
    Ok(StopReason::EndTurn)
}

async fn permission_turn(cx: &ConnectionTo<Client>, session_id: &SessionId) -> Result<StopReason> {
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            "tool-danger",
            ToolCallUpdateFields::new().title("rm -rf target".to_owned()),
        ),
        vec![
            PermissionOption::new("allow", "Allow".to_owned(), PermissionOptionKind::AllowOnce),
            PermissionOption::new("deny", "Deny".to_owned(), PermissionOptionKind::RejectOnce),
        ],
    );
    let response = cx.send_request(request).block_task().await?;
    let verdict = match response.outcome {
        RequestPermissionOutcome::Selected(selected) => selected.option_id.to_string(),
        RequestPermissionOutcome::Cancelled => "cancelled".to_owned(),
        _ => "unknown".to_owned(),
    };
    send_chunk(cx, session_id, &format!("permission:{verdict}"))?;
    Ok(StopReason::EndTurn)
}

async fn slow_turn(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    cancelled: &AtomicBool,
) -> Result<StopReason> {
    for index in 0..100 {
        if cancelled.load(Ordering::Acquire) {
            return Ok(StopReason::Cancelled);
        }
        send_chunk(cx, session_id, &format!("tick {index}"))?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(StopReason::EndTurn)
}

fn send_chunk(cx: &ConnectionTo<Client>, session_id: &SessionId, text: &str) -> Result<()> {
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text.to_owned(),
        )))),
    ))
}
