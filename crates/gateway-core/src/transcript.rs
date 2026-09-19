//! Folding event logs into a readable transcript.
//!
//! 将事件日志折叠成可读的 transcript。
//!
//! The fold is deliberately pure: the same events always produce the same
//! transcript, which is what makes reconnect/replay stable for both the web
//! client and the macOS app.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::event::AgentEvent;
use crate::permission::PermissionRequest;

/// A compact, UI-friendly transcript.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Folded items in display order.
    pub items: Vec<TranscriptItem>,
    /// Permission requests still waiting for an answer.
    pub pending: Vec<PermissionRequest>,
    /// Highest sequence number observed while folding.
    pub last_seq: u64,
}

/// One rendered transcript item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        id: String,
        seq: u64,
        text: String,
    },
    Agent {
        id: String,
        seq: u64,
        text: String,
    },
    Thought {
        id: String,
        seq: u64,
        text: String,
    },
    Tool {
        id: String,
        seq: u64,
        tool_call_id: String,
        title: String,
        status: String,
        output: String,
    },
    Terminal {
        id: String,
        seq: u64,
        stream: String,
        text: String,
    },
    Permission {
        id: String,
        seq: u64,
        request: PermissionRequest,
        answer: Option<String>,
    },
    Notice {
        id: String,
        seq: u64,
        text: String,
    },
}

/// Fold a session event stream into the UI transcript shape.
#[must_use]
pub fn fold_transcript(events: &[AgentEvent]) -> Transcript {
    let mut items = Vec::new();
    let mut pending = HashMap::<String, PermissionRequest>::new();
    let mut tool_index = HashMap::<String, usize>::new();
    let mut permission_index = HashMap::<String, usize>::new();
    let mut last_seq = 0;

    for event in events {
        last_seq = last_seq.max(event.seq);
        let payload = event.payload.as_object().cloned().unwrap_or_default();

        match event.event_type.as_str() {
            "user_message" | "user_message_chunk" => {
                let text = chunk_text(&payload);
                if !text.is_empty() {
                    items.push(TranscriptItem::User {
                        id: event.id.to_string(),
                        seq: event.seq,
                        text,
                    });
                }
            }
            "agent_message" | "agent_message_chunk" => {
                append_streamed(&mut items, "agent", event, chunk_text(&payload));
            }
            "agent_thought_chunk" => {
                append_streamed(&mut items, "thought", event, chunk_text(&payload));
            }
            "tool_call" | "tool_call_update" => {
                let tool_call_id = payload
                    .get("toolCallId")
                    .or_else(|| payload.get("tool_call_id"))
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| event.id.to_string());
                let title = payload
                    .get("title")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
                let status = payload
                    .get("status")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
                let output = tool_output(&payload);

                if let Some(existing) = tool_index.get(&tool_call_id).copied() {
                    if let Some(TranscriptItem::Tool {
                        title: existing_title,
                        status: existing_status,
                        output: existing_output,
                        ..
                    }) = items.get_mut(existing)
                    {
                        if let Some(title) = title {
                            *existing_title = title;
                        }
                        if let Some(status) = status {
                            *existing_status = status;
                        }
                        if !output.is_empty() {
                            if existing_output.is_empty() {
                                *existing_output = output;
                            } else {
                                existing_output.push('\n');
                                existing_output.push_str(&output);
                            }
                        }
                    }
                } else {
                    tool_index.insert(tool_call_id.clone(), items.len());
                    items.push(TranscriptItem::Tool {
                        id: event.id.to_string(),
                        seq: event.seq,
                        tool_call_id,
                        title: title.unwrap_or_else(|| event.id.to_string()),
                        status: status.unwrap_or_else(|| "pending".to_owned()),
                        output,
                    });
                }
            }
            "terminal_output" => {
                items.push(TranscriptItem::Terminal {
                    id: event.id.to_string(),
                    seq: event.seq,
                    stream: payload
                        .get("stream")
                        .and_then(|value| value.as_str())
                        .unwrap_or("stdout")
                        .to_owned(),
                    text: payload
                        .get("content")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            "permission_request" => {
                if let Ok(request) = serde_json::from_value::<PermissionRequest>(
                    serde_json::Value::Object(payload.clone()),
                ) {
                    pending.insert(request.id.to_string(), request.clone());
                    permission_index.insert(request.id.to_string(), items.len());
                    items.push(TranscriptItem::Permission {
                        id: event.id.to_string(),
                        seq: event.seq,
                        request,
                        answer: None,
                    });
                }
            }
            "permission_response" => {
                let request_id = payload
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                pending.remove(&request_id);
                if let Some(index) = permission_index.get(&request_id).copied() {
                    if let Some(TranscriptItem::Permission { answer, .. }) = items.get_mut(index) {
                        *answer = Some(describe_outcome(payload.get("outcome")));
                    }
                }
            }
            "session_failed" => {
                let reason = payload
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown error");
                let fatal = payload
                    .get("fatal")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                items.push(TranscriptItem::Notice {
                    id: event.id.to_string(),
                    seq: event.seq,
                    text: if fatal {
                        format!("Session failed: {reason}")
                    } else {
                        format!("Turn failed: {reason}")
                    },
                });
            }
            "error" => {
                items.push(TranscriptItem::Notice {
                    id: event.id.to_string(),
                    seq: event.seq,
                    text: payload
                        .get("message")
                        .and_then(|value| value.as_str())
                        .unwrap_or("error")
                        .to_owned(),
                });
            }
            _ => {}
        }
    }

    Transcript {
        items,
        pending: pending.into_values().collect(),
        last_seq,
    }
}

/// A one-line tool summary for compact lists.
#[must_use]
pub fn tool_summary(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::Tool { title, status, .. } => format!("{title} · {status}"),
        _ => String::new(),
    }
}

fn append_streamed(items: &mut Vec<TranscriptItem>, kind: &str, event: &AgentEvent, text: String) {
    if text.is_empty() {
        return;
    }

    let event_id = event.id.to_string();
    let should_merge = match (kind, items.last_mut()) {
        (
            "agent",
            Some(TranscriptItem::Agent {
                id,
                text: existing_text,
                ..
            }),
        ) if id == &event_id => {
            existing_text.push_str(&text);
            true
        }
        (
            "thought",
            Some(TranscriptItem::Thought {
                id,
                text: existing_text,
                ..
            }),
        ) if id == &event_id => {
            existing_text.push_str(&text);
            true
        }
        _ => false,
    };

    if !should_merge {
        let target = match kind {
            "agent" => TranscriptItem::Agent {
                id: event_id,
                seq: event.seq,
                text,
            },
            "thought" => TranscriptItem::Thought {
                id: event_id,
                seq: event.seq,
                text,
            },
            _ => return,
        };
        items.push(target);
    }
}

fn chunk_text(payload: &serde_json::Map<String, serde_json::Value>) -> String {
    payload.get("content").map(content_text).unwrap_or_default()
}

fn content_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(|value| value.as_str()) {
                return text.to_owned();
            }
            if let Some(content) = object.get("content") {
                if let Some(array) = content.as_array() {
                    return array.iter().map(content_text).collect();
                }
                return content_text(content);
            }
            object
                .get("type")
                .and_then(|value| value.as_str())
                .map(|kind| format!("[{kind}]"))
                .unwrap_or_default()
        }
        serde_json::Value::Array(items) => items.iter().map(content_text).collect(),
        _ => String::new(),
    }
}

fn tool_output(payload: &serde_json::Map<String, serde_json::Value>) -> String {
    let Some(content) = payload.get("content").and_then(|value| value.as_array()) else {
        return String::new();
    };

    content
        .iter()
        .map(|entry| match entry {
            serde_json::Value::Object(object) => {
                match object.get("type").and_then(|value| value.as_str()) {
                    Some("diff") => "[diff]".to_owned(),
                    Some("terminal") => "[terminal]".to_owned(),
                    _ => object
                        .get("content")
                        .map(content_text)
                        .unwrap_or_else(|| content_text(entry)),
                }
            }
            _ => content_text(entry),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_outcome(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else {
        return "answered".to_owned();
    };

    if let Some(outcome) = value.as_object().and_then(|object| object.get("outcome")) {
        if outcome == "cancelled" {
            return "cancelled".to_owned();
        }
        if let Some(option_id) = outcome
            .as_object()
            .and_then(|object| object.get("optionId").or_else(|| object.get("option_id")))
            .and_then(|value| value.as_str())
        {
            return option_id.to_owned();
        }
    }

    value
        .as_object()
        .and_then(|object| object.get("optionId").or_else(|| object.get("option_id")))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "answered".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, EventType};
    use crate::ids::{EventId, PermissionId, SessionId};
    use crate::permission::{PermissionOption, PermissionOptionKind};
    use chrono::{TimeZone, Utc};

    fn event(seq: u64, event_type: &str, payload: serde_json::Value) -> AgentEvent {
        AgentEvent {
            id: EventId::new(format!("evt_{seq}")),
            session_id: SessionId::new("sess_1"),
            seq,
            timestamp: Utc.timestamp_opt(seq as i64, 0).unwrap(),
            event_type: event_type.to_owned(),
            payload,
        }
    }

    fn text_chunk(text: &str) -> serde_json::Value {
        serde_json::json!({ "content": [{ "type": "text", "text": text }] })
    }

    #[test]
    fn merges_consecutive_agent_chunks() {
        let transcript = fold_transcript(&[
            event(1, EventType::UserMessage.as_str(), text_chunk("why?")),
            event(
                2,
                EventType::AgentMessageChunk.as_str(),
                text_chunk("Looking at "),
            ),
            event(
                3,
                EventType::AgentMessageChunk.as_str(),
                text_chunk("your project."),
            ),
        ]);
        assert_eq!(transcript.items.len(), 2);
        match &transcript.items[1] {
            TranscriptItem::Agent { text, .. } => {
                assert_eq!(text, "Looking at your project.");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn does_not_merge_chunks_from_different_events() {
        let transcript = fold_transcript(&[
            event(
                1,
                EventType::AgentMessageChunk.as_str(),
                text_chunk("first"),
            ),
            event(
                2,
                EventType::AgentMessageChunk.as_str(),
                text_chunk("second"),
            ),
        ]);
        assert_eq!(transcript.items.len(), 2);
        assert!(matches!(
            transcript.items.as_slice(),
            [
                TranscriptItem::Agent { text, .. },
                TranscriptItem::Agent { text: second, .. }
            ] if text == "first" && second == "second"
        ));
    }

    #[test]
    fn tracks_pending_permissions() {
        let request = PermissionRequest {
            id: PermissionId::new("perm_1"),
            title: Some("rm -rf target".into()),
            tool_kind: None,
            tool_call: serde_json::json!({ "title": "rm -rf target" }),
            options: vec![PermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            requested_at: Utc.timestamp_opt(1, 0).unwrap(),
        };
        let transcript = fold_transcript(&[event(
            1,
            EventType::PermissionRequest.as_str(),
            serde_json::to_value(&request).unwrap(),
        )]);
        assert_eq!(transcript.pending.len(), 1);
        assert!(matches!(
            transcript.items.first(),
            Some(TranscriptItem::Permission { answer, .. }) if answer.is_none()
        ));
    }

    #[test]
    fn resolves_permission_responses() {
        let request = PermissionRequest {
            id: PermissionId::new("perm_1"),
            title: Some("rm -rf target".into()),
            tool_kind: None,
            tool_call: serde_json::json!({ "title": "rm -rf target" }),
            options: vec![PermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            requested_at: Utc.timestamp_opt(1, 0).unwrap(),
        };
        let transcript = fold_transcript(&[
            event(
                1,
                EventType::PermissionRequest.as_str(),
                serde_json::to_value(&request).unwrap(),
            ),
            event(
                2,
                EventType::PermissionResponse.as_str(),
                serde_json::json!({ "id": "perm_1", "outcome": { "optionId": "allow" } }),
            ),
        ]);
        assert!(transcript.pending.is_empty());
        assert!(matches!(
            transcript.items.first(),
            Some(TranscriptItem::Permission { answer, .. }) if answer.as_deref() == Some("allow")
        ));
    }
}
