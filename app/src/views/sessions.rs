use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::scroll::ScrollableElement;
use gpui_component::{Icon, IconName, Selectable, Sizable, Size};

use gateway_core::event::AgentEvent;
use gateway_core::permission::PermissionRequest;
use gateway_core::session::{AgentSession, SessionStatus};
use gateway_core::transcript::{Transcript, TranscriptItem, tool_summary};

use crate::state::{AppState, AppTab};
use crate::views::{h_flex, v_flex};

pub fn render(state: &mut AppState, window: &mut Window, cx: &mut Context<AppState>) -> AnyElement {
    let left = render_session_list(state, window, cx);
    let right = render_session_detail(state, window, cx);

    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(rgb(0x27303a))
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(16.)).child("Sessions"))
                        .child(h_flex().gap_2().child(refresh_button(cx))),
                ),
        )
        .child(h_flex().flex_1().min_h_0().child(left).child(right))
        .into_any_element()
}

fn refresh_button(cx: &mut Context<AppState>) -> AnyElement {
    Button::new("refresh-sessions")
        .with_size(Size::Small)
        .ghost()
        .icon(Icon::new(IconName::Redo2).with_size(Size::Small))
        .on_click(cx.listener(|state, _, window, cx| state.refresh_overview(window, cx)))
        .into_any_element()
}

fn render_session_list(
    state: &mut AppState,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let create = cx.listener(|state, _, window, cx| state.create_session_from_inputs(window, cx));
    let selected_agent_id = state.selected_agent_id.clone();
    let selected_session_id = state.selected_session_id.clone();
    let agents: Vec<_> = state.agents.clone();
    let sessions: Vec<_> = state.sessions.clone();
    let agent_buttons: Vec<AnyElement> = agents
        .into_iter()
        .map(|agent| {
            let selected = selected_agent_id.as_ref().is_some_and(|id| id == &agent.id);
            let agent_id = agent.id.clone();
            Button::new(SharedString::from(format!("agent-{}", agent_id)))
                .with_size(Size::Small)
                .ghost()
                .selected(selected)
                .label(agent.name.clone())
                .on_click(cx.listener(move |state, _, _window, cx| {
                    state.select_agent(agent_id.clone(), cx);
                }))
                .into_any_element()
        })
        .collect();
    let session_rows: Vec<AnyElement> = sessions
        .into_iter()
        .map(|session| {
            let selected = selected_session_id
                .as_ref()
                .is_some_and(|id| id == &session.id.to_string());
            session_row(session, selected, cx)
        })
        .collect();

    v_flex()
        .w(px(360.))
        .min_h_0()
        .border_r_1()
        .border_color(rgb(0x27303a))
        .child(
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(rgb(0x27303a))
                .child(
                    v_flex()
                        .gap_3()
                        .child(div().text_size(px(14.)).child("New session"))
                        .child(
                            Input::new(&state.workspace_input)
                                .appearance(true)
                                .bordered(true)
                                .cleanable(false),
                        )
                        .child(
                            Input::new(&state.cwd_input)
                                .appearance(true)
                                .bordered(true)
                                .cleanable(false),
                        )
                        .child(h_flex().gap_2().flex_wrap().children(agent_buttons))
                        .child(
                            Button::new("create-session")
                                .with_size(Size::Small)
                                .primary()
                                .label("Create")
                                .on_click(create),
                        ),
                ),
        )
        .child(
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .children(session_rows),
        )
        .into_any_element()
}

fn session_row(session: AgentSession, selected: bool, cx: &mut Context<AppState>) -> AnyElement {
    let session_id = session.id.to_string();
    Button::new(SharedString::from(format!("session-{}", session_id)))
        .ghost()
        .selected(selected)
        .label(format!(
            "{} · {}",
            session.title.as_deref().unwrap_or(&session.agent_name),
            session.status.as_str()
        ))
        .on_click(cx.listener({
            let id = session.id.to_string();
            move |state, _, window, cx| state.select_session(id.clone(), window, cx)
        }))
        .into_any_element()
}

fn render_session_detail(
    state: &mut AppState,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let Some(session) = state.selected_session_view() else {
        return v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(0x9ca3af))
                    .child("Pick a session to inspect its transcript."),
            )
            .into_any_element();
    };

    let prompt_submit = cx.listener(|state, _, window, cx| state.submit_prompt(window, cx));
    let cancel = cx.listener(|state, _, window, cx| state.cancel_selected_session(window, cx));

    let transcript = state.selected_transcript.clone();
    let pending = state.pending_permissions();

    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(rgb(0x27303a))
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .child(session.title.unwrap_or(session.agent_name)),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(0x9ca3af))
                                        .child(session.workspace),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(status_chip(session.status.as_str()))
                                .when(session.status.is_drivable(), |this| {
                                    this.child(
                                        Button::new("cancel")
                                            .with_size(Size::Small)
                                            .danger()
                                            .label("Cancel")
                                            .on_click(cancel),
                                    )
                                }),
                        ),
                ),
        )
        .child(
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(render_transcript(transcript, pending, cx)),
        )
        .child(
            div()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(rgb(0x27303a))
                .child(
                    h_flex()
                        .gap_2()
                        .items_end()
                        .child(
                            Input::new(&state.prompt_input)
                                .appearance(true)
                                .bordered(true)
                                .h(px(96.)),
                        )
                        .child(
                            Button::new("send-prompt")
                                .with_size(Size::Small)
                                .primary()
                                .label("Send")
                                .on_click(prompt_submit),
                        ),
                ),
        )
        .into_any_element()
}

fn render_transcript(
    transcript: Transcript,
    pending: Vec<PermissionRequest>,
    cx: &mut Context<AppState>,
) -> AnyElement {
    v_flex()
        .flex_1()
        .min_h_0()
        .overflow_y_scrollbar()
        .px_4()
        .py_4()
        .gap_3()
        .children(transcript.items.into_iter().map(render_item))
        .children(
            pending
                .into_iter()
                .map(|request| render_pending_request(request, cx)),
        )
        .into_any_element()
}

fn render_item(item: TranscriptItem) -> AnyElement {
    match item {
        TranscriptItem::User { text, .. } => {
            bubble("User", text, rgb(0x1d4ed8).into(), rgb(0xe0f2fe).into())
        }
        TranscriptItem::Agent { text, .. } => {
            bubble("Agent", text, rgb(0x1f2937).into(), rgb(0xf3f4f6).into())
        }
        TranscriptItem::Thought { text, .. } => {
            bubble("Thought", text, rgb(0x312e81).into(), rgb(0xe0e7ff).into())
        }
        TranscriptItem::Tool {
            title,
            status,
            output,
            ..
        } => v_flex()
            .gap_2()
            .px_3()
            .py_3()
            .border_1()
            .border_color(rgb(0x374151))
            .child(h_flex().justify_between().child(title).child(status))
            .when(!output.is_empty(), |this| this.child(div().child(output)))
            .into_any_element(),
        TranscriptItem::Terminal { stream, text, .. } => v_flex()
            .gap_1()
            .px_3()
            .py_3()
            .border_1()
            .border_color(rgb(0x374151))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0x9ca3af))
                    .child(stream),
            )
            .child(div().child(text))
            .into_any_element(),
        TranscriptItem::Permission {
            request, answer, ..
        } => v_flex()
            .gap_2()
            .px_3()
            .py_3()
            .border_1()
            .border_color(rgb(0x92400e))
            .bg(rgb(0x451a03))
            .child(div().child(format!(
                "Permission: {}",
                request.title.unwrap_or_else(|| "tool call".to_owned())
            )))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0xfbbf24))
                    .child(answer.unwrap_or_else(|| "pending".to_owned())),
            )
            .into_any_element(),
        TranscriptItem::Notice { text, .. } => {
            bubble("Notice", text, rgb(0x374151).into(), rgb(0xf3f4f6).into())
        }
    }
}

fn render_pending_request(request: PermissionRequest, cx: &mut Context<AppState>) -> AnyElement {
    let command = describe_tool_call(&request);
    let request_id = request.id.to_string();
    v_flex()
        .gap_2()
        .px_3()
        .py_3()
        .border_1()
        .border_color(rgb(0xb45309))
        .bg(rgb(0x451a03))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(0xf59e0b))
                .child("Permission required"),
        )
        .child(div().child(request.title.unwrap_or_else(|| "tool call".to_owned())))
        .when_some(command, |this, command| {
            this.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(0xfcd34d))
                    .child(command),
            )
        })
        .child(
            h_flex()
                .gap_2()
                .children(request.options.iter().map(|option| {
                    let approved = option.kind.is_allow();
                    let option_id = option.option_id.clone();
                    Button::new(SharedString::from(format!("perm-{}", &option_id)))
                        .with_size(Size::Small)
                        .when(!approved, |this| this.danger())
                        .label(option.name.clone())
                        .on_click(cx.listener({
                            let request_id = request_id.clone();
                            move |state, _, window, cx| {
                                state.answer_permission(
                                    request_id.clone(),
                                    Some(option_id.clone()),
                                    approved,
                                    window,
                                    cx,
                                );
                            }
                        }))
                }))
                .when(request.options.is_empty(), |this| {
                    this.children([
                        Button::new("deny")
                            .with_size(Size::Small)
                            .danger()
                            .label("Deny")
                            .on_click(cx.listener({
                                let request_id = request_id.clone();
                                move |state, _, window, cx| {
                                    state.answer_permission(
                                        request_id.clone(),
                                        None,
                                        false,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                        Button::new("allow")
                            .with_size(Size::Small)
                            .primary()
                            .label("Allow")
                            .on_click(cx.listener({
                                let request_id = request_id.clone();
                                move |state, _, window, cx| {
                                    state.answer_permission(
                                        request_id.clone(),
                                        None,
                                        true,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ])
                }),
        )
        .into_any_element()
}

fn bubble(kind: &'static str, text: String, bg: gpui::Hsla, fg: gpui::Hsla) -> AnyElement {
    v_flex()
        .gap_2()
        .px_3()
        .py_3()
        .rounded_sm()
        .bg(bg)
        .text_color(fg)
        .child(div().text_size(px(12.)).child(kind))
        .child(div().child(text))
        .into_any_element()
}

fn status_chip(text: &str) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(rgb(0x374151))
        .text_color(rgb(0xf3f4f6))
        .text_size(px(12.))
        .child(text.to_owned())
        .into_any_element()
}

fn describe_tool_call(request: &PermissionRequest) -> Option<String> {
    let body = request.tool_call.as_object()?;
    if let Some(title) = body.get("title").and_then(|value| value.as_str()) {
        return Some(title.to_owned());
    }
    if let Some(raw) = body
        .get("rawInput")
        .or_else(|| body.get("raw_input"))
        .and_then(|value| value.as_object())
    {
        if let Some(command) = raw.get("command").and_then(|value| value.as_str()) {
            return Some(command.to_owned());
        }
    }
    request.title.clone()
}
