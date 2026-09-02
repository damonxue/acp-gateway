//! Translation between ACP wire types and gateway events.
//!
//! Every ACP → gateway conversion lives here, and nothing else in the workspace
//! is allowed to know what an ACP `SessionUpdate` looks like. That keeps the
//! blast radius of an SDK upgrade to this file, and it is why the remote
//! protocol can stay stable while ACP evolves.
//!
//! ## Payload policy
//!
//! Mapped payloads carry the agent's own JSON structure, unflattened. A phone
//! rendering a diff or a command needs the same detail an IDE has; a summary
//! string produced here would be a permanent loss of information. Unknown
//! updates are preserved verbatim under [`EventType::SessionUpdate`] rather
//! than dropped.

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOption as AcpPermissionOption,
    PermissionOptionKind as AcpPermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, SelectedPermissionOutcome, SessionUpdate, StopReason, TextContent,
};
use chrono::{DateTime, Utc};
use gateway_core::agent::PromptBlock;
use gateway_core::event::{EventDraft, EventType};
use gateway_core::ids::PermissionId;
use gateway_core::permission::{
    PermissionDecision, PermissionOption, PermissionOptionKind, PermissionRequest,
};
use serde_json::json;

/// Convert one ACP session update into a gateway event.
#[must_use]
pub fn event_for_update(update: &SessionUpdate) -> EventDraft {
    let (event_type, payload) = match update {
        SessionUpdate::UserMessageChunk(chunk) => (EventType::UserMessageChunk, value_of(chunk)),
        SessionUpdate::AgentMessageChunk(chunk) => (EventType::AgentMessageChunk, value_of(chunk)),
        SessionUpdate::AgentThoughtChunk(chunk) => (EventType::AgentThoughtChunk, value_of(chunk)),
        SessionUpdate::ToolCall(call) => (EventType::ToolCall, value_of(call)),
        SessionUpdate::ToolCallUpdate(call) => (EventType::ToolCallUpdate, value_of(call)),
        SessionUpdate::Plan(plan) => (EventType::PlanUpdate, value_of(plan)),
        SessionUpdate::AvailableCommandsUpdate(commands) => {
            (EventType::AvailableCommandsUpdate, value_of(commands))
        }
        SessionUpdate::CurrentModeUpdate(mode) => (EventType::CurrentModeUpdate, value_of(mode)),
        // `session_info_update` carries the title the agent picked for the
        // conversation; the manager folds `title` out of this payload.
        SessionUpdate::SessionInfoUpdate(info) => (EventType::SessionUpdate, value_of(info)),
        other => (EventType::SessionUpdate, value_of(other)),
    };
    EventDraft::from_value(event_type, payload)
}

/// Build the domain permission request and the event that announces it.
#[must_use]
pub fn permission_request(
    id: PermissionId,
    request: &RequestPermissionRequest,
    now: DateTime<Utc>,
) -> (PermissionRequest, EventDraft) {
    let domain = PermissionRequest {
        id,
        title: request.tool_call.fields.title.clone(),
        tool_kind: request
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(|kind| value_of(kind).as_str().unwrap_or_default().to_owned()),
        tool_call: value_of(&request.tool_call),
        options: request.options.iter().map(map_option).collect(),
        requested_at: now,
    };
    let draft = EventDraft::from_value(EventType::PermissionRequest, value_of(&domain));
    (domain, draft)
}

/// The event recording how a permission request was answered.
#[must_use]
pub fn permission_response_event(
    id: &PermissionId,
    outcome: &RequestPermissionOutcome,
) -> EventDraft {
    EventDraft::from_value(
        EventType::PermissionResponse,
        json!({ "id": id, "outcome": value_of(outcome) }),
    )
}

/// Resolve a client decision against the options the agent actually offered.
///
/// A decision that names an unknown option is treated as a rejection rather
/// than silently approved: the safe direction is refusing an operation the
/// user may not have intended to allow.
#[must_use]
pub fn outcome_for(
    request: &PermissionRequest,
    decision: &PermissionDecision,
) -> RequestPermissionOutcome {
    match decision {
        PermissionDecision::Cancelled => RequestPermissionOutcome::Cancelled,
        PermissionDecision::Selected { option_id } => {
            if request
                .options
                .iter()
                .any(|option| &option.option_id == option_id)
            {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option_id.clone(),
                ))
            } else {
                reject_or_cancel(request)
            }
        }
        PermissionDecision::Approved { approved } => match request.resolve_option(*approved) {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            None => reject_or_cancel(request),
        },
    }
}

fn reject_or_cancel(request: &PermissionRequest) -> RequestPermissionOutcome {
    match request.resolve_option(false) {
        Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        )),
        None => RequestPermissionOutcome::Cancelled,
    }
}

/// Event emitted when a prompt turn ends normally.
#[must_use]
pub fn turn_completed_event(stop_reason: StopReason) -> EventDraft {
    EventDraft::from_value(
        EventType::SessionCompleted,
        json!({ "stop_reason": value_of(&stop_reason) }),
    )
}

/// Event emitted when a turn or the session itself fails.
///
/// `fatal` distinguishes "this turn broke" from "this session is over", which
/// is the difference between a session the user can retry and one they cannot.
#[must_use]
pub fn failure_event(reason: impl std::fmt::Display, fatal: bool) -> EventDraft {
    EventDraft::from_value(
        EventType::SessionFailed,
        json!({ "reason": reason.to_string(), "fatal": fatal }),
    )
}

/// Build an ACP prompt request from domain prompt blocks.
#[must_use]
pub fn prompt_request(
    session_id: &agent_client_protocol::schema::v1::SessionId,
    blocks: &[PromptBlock],
) -> PromptRequest {
    PromptRequest::new(
        session_id.clone(),
        blocks.iter().map(content_block).collect(),
    )
}

fn content_block(block: &PromptBlock) -> ContentBlock {
    match block {
        PromptBlock::Text { text } => ContentBlock::Text(TextContent::new(text.clone())),
        // A resource link is rendered as a text mention so that agents without
        // resource-link support still see which file the user meant.
        PromptBlock::ResourceLink { uri, name } => {
            ContentBlock::Text(TextContent::new(format!("@{name} ({uri})")))
        }
    }
}

fn map_option(option: &AcpPermissionOption) -> PermissionOption {
    PermissionOption {
        option_id: option.option_id.to_string(),
        name: option.name.clone(),
        kind: match option.kind {
            AcpPermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
            AcpPermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
            AcpPermissionOptionKind::RejectOnce => PermissionOptionKind::RejectOnce,
            _ => PermissionOptionKind::RejectAlways,
        },
    }
}

/// Serialise any ACP value, falling back to `null` rather than panicking.
///
/// ACP types are plain serde structs, so failure is unreachable in practice —
/// but an agent's output must never be able to abort the gateway.
fn value_of<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, SessionId as AcpSessionId, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn permission(options: Vec<AcpPermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            AcpSessionId::from("acp-1"),
            ToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new().title("Run cargo test".to_owned()),
            ),
            options,
        )
    }

    fn option(id: &'static str, kind: AcpPermissionOptionKind) -> AcpPermissionOption {
        AcpPermissionOption::new(id, id.to_owned(), kind)
    }

    #[test]
    fn agent_chunks_keep_the_agents_own_content_structure() {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("hello"),
        )));
        let draft = event_for_update(&update);
        assert_eq!(draft.event_type, EventType::AgentMessageChunk);
        assert_eq!(draft.payload["content"]["text"], "hello");
    }

    #[test]
    fn unmodelled_updates_are_preserved_instead_of_dropped() {
        let update = SessionUpdate::CurrentModeUpdate(
            agent_client_protocol::schema::v1::CurrentModeUpdate::new("architect"),
        );
        let draft = event_for_update(&update);
        assert_eq!(draft.event_type, EventType::CurrentModeUpdate);
        assert!(!draft.payload.is_null());
    }

    #[test]
    fn a_permission_request_keeps_the_raw_tool_call_for_the_ui() {
        let request = permission(vec![option("yes", AcpPermissionOptionKind::AllowOnce)]);
        let (domain, draft) = permission_request(PermissionId::new("perm_1"), &request, Utc::now());
        assert_eq!(domain.title.as_deref(), Some("Run cargo test"));
        assert_eq!(domain.options.len(), 1);
        assert_eq!(draft.event_type, EventType::PermissionRequest);
        assert_eq!(draft.payload["tool_call"]["toolCallId"], "tool-1");
    }

    #[test]
    fn a_boolean_approval_picks_the_agents_allow_once_option() {
        let request = permission(vec![
            option("always", AcpPermissionOptionKind::AllowAlways),
            option("once", AcpPermissionOptionKind::AllowOnce),
            option("no", AcpPermissionOptionKind::RejectOnce),
        ]);
        let (domain, _) = permission_request(PermissionId::new("perm_1"), &request, Utc::now());

        let outcome = outcome_for(&domain, &PermissionDecision::approved(true));
        match outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.to_string(), "once");
            }
            RequestPermissionOutcome::Cancelled => panic!("expected a selection"),
            _ => panic!("unexpected outcome"),
        }
    }

    #[test]
    fn an_unknown_option_id_is_downgraded_to_a_rejection() {
        let request = permission(vec![
            option("yes", AcpPermissionOptionKind::AllowOnce),
            option("no", AcpPermissionOptionKind::RejectOnce),
        ]);
        let (domain, _) = permission_request(PermissionId::new("perm_1"), &request, Utc::now());

        let outcome = outcome_for(
            &domain,
            &PermissionDecision::Selected {
                option_id: "made-up".to_owned(),
            },
        );
        match outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.to_string(), "no");
            }
            other => panic!("expected the reject option, got {other:?}"),
        }
    }

    #[test]
    fn a_decision_with_no_matching_option_cancels_rather_than_allows() {
        let request = permission(vec![option("yes", AcpPermissionOptionKind::AllowOnce)]);
        let (domain, _) = permission_request(PermissionId::new("perm_1"), &request, Utc::now());
        assert!(matches!(
            outcome_for(&domain, &PermissionDecision::approved(false)),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn prompt_blocks_become_acp_content() {
        let request = prompt_request(
            &AcpSessionId::from("acp-1"),
            &[
                PromptBlock::text("look at"),
                PromptBlock::ResourceLink {
                    uri: "file:///a/b.rs".to_owned(),
                    name: "b.rs".to_owned(),
                },
            ],
        );
        assert_eq!(request.prompt.len(), 2);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["prompt"][0]["text"], "look at");
        assert_eq!(json["prompt"][1]["text"], "@b.rs (file:///a/b.rs)");
    }
}
