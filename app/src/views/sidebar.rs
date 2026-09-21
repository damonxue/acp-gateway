use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Selectable, Sizable, Size};

use crate::state::{AppState, AppTab, ConnectionStatus, DaemonStatus};
use crate::views::{h_flex, v_flex};

pub fn render(
    state: &mut AppState,
    _window: &mut Window,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let sessions_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Sessions, cx)
    });
    let machines_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Machines, cx)
    });
    let agents_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Agents, cx)
    });
    let devices_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Devices, cx)
    });
    let zed_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Integrations, cx)
    });
    let logs_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| {
        state.set_tab(AppTab::Logs, cx)
    });
    let base_url = state.services.client.base_url().to_owned();
    let connection_status = state.connection_label();
    let zed_status = state.zed_status_label();

    v_flex()
        .w(px(260.))
        .h_full()
        .flex_shrink_0()
        .border_r_1()
        .border_color(rgb(0x27303a))
        .bg(rgb(0x111827))
        .text_color(rgb(0xe5e7eb))
        .child(
            div()
                .px_4()
                .py_4()
                .border_b_1()
                .border_color(rgb(0x27303a))
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(Icon::new(IconName::SquareTerminal).with_size(Size::Small))
                                .child("Agent Gateway"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x9ca3af))
                                .child(base_url),
                        ),
                ),
        )
        .child(v_flex().px_3().py_3().gap_2().children([
            nav_button(
                "Sessions",
                IconName::Inbox,
                state.tab == AppTab::Sessions,
                sessions_click,
            ),
            nav_button(
                "Machines",
                IconName::Building2,
                state.tab == AppTab::Machines,
                machines_click,
            ),
            nav_button(
                "Agents",
                IconName::Bot,
                state.tab == AppTab::Agents,
                agents_click,
            ),
            nav_button(
                "Devices",
                IconName::CircleUser,
                state.tab == AppTab::Devices,
                devices_click,
            ),
            nav_button(
                "Zed",
                IconName::Settings2,
                state.tab == AppTab::Integrations,
                zed_click,
            ),
            nav_button(
                "Logs",
                IconName::SquareTerminal,
                state.tab == AppTab::Logs,
                logs_click,
            ),
        ]))
        .child(
            div()
                .mt_auto()
                .px_4()
                .py_4()
                .border_t_1()
                .border_color(rgb(0x27303a))
                .child(gateway_control(state, &connection_status, cx))
                .child(session_context(state))
                .child(channel_summary(state))
                .child(status_row("Zed", zed_status)),
        )
        .into_any_element()
}

fn gateway_control(
    state: &mut AppState,
    connection: &str,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let running = matches!(state.daemon_status, DaemonStatus::Running { .. })
        || matches!(state.connection, ConnectionStatus::Online);
    let label = if running {
        "Stop gateway"
    } else {
        "▶ Start gateway"
    };
    let button = if running {
        Button::new("gateway-stop")
            .with_size(Size::Small)
            .danger()
            .icon(Icon::new(IconName::CircleX).with_size(Size::Small))
            .label(label)
            .on_click(cx.listener(|state, _, _, cx| {
                state.stop_daemon();
                cx.notify();
            }))
            .into_any_element()
    } else {
        Button::new("gateway-start")
            .with_size(Size::Small)
            .primary()
            .icon(Icon::new(IconName::Plus).with_size(Size::Small))
            .label(label)
            .on_click(cx.listener(|state, _, _, cx| {
                state.start_daemon();
                cx.notify();
            }))
            .into_any_element()
    };
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(status_row("Gateway", connection.to_owned()))
                .child(button),
        )
        .into_any_element()
}

fn session_context(state: &AppState) -> AnyElement {
    let Some(id) = state.selected_session_id.as_ref() else {
        return div()
            .text_size(px(11.))
            .text_color(rgb(0x64748b))
            .child("No active session")
            .into_any_element();
    };
    let session = state
        .sessions
        .iter()
        .find(|session| session.id.to_string() == *id);
    let Some(session) = session else {
        return div()
            .text_size(px(11.))
            .text_color(rgb(0x64748b))
            .child("Session loading…")
            .into_any_element();
    };
    let title = session.title.as_deref().unwrap_or("Untitled session");
    v_flex()
        .gap_1()
        .px_2()
        .py_2()
        .rounded_sm()
        .bg(rgb(0x172033))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(0x93c5fd))
                .child("ACTIVE SESSION"),
        )
        .child(div().text_size(px(12.)).child(title.to_owned()))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(0x94a3b8))
                .child(session.workspace.display().to_string()),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(0x64748b))
                .child(format!(
                    "{} · {}",
                    session.agent_name,
                    session.status.as_str()
                )),
        )
        .into_any_element()
}

fn channel_summary(state: &AppState) -> AnyElement {
    let ahp = state.ahp_status.status.as_ref();
    let embedded_wechat = state.health.as_ref().and_then(|health| {
        health
            .components
            .iter()
            .find(|component| component.name == "wechat")
    });
    let ahp_text = ahp
        .map(|status| {
            let detail = status
                .session_id
                .as_deref()
                .or(status.channel_id.as_deref())
                .unwrap_or("");
            if detail.is_empty() {
                format!("微信 · {}", status.state)
            } else {
                format!("微信 · {} · {}", status.state, detail)
            }
        })
        .or_else(|| embedded_wechat.map(|component| format!("微信 · {}", component.state)))
        .unwrap_or_else(|| "微信 · 未配置".to_owned());
    let component_rows = state
        .health
        .as_ref()
        .map(|health| {
            health
                .components
                .iter()
                .filter(|component| matches!(component.name.as_str(), "lark" | "telegram"))
                .map(|component| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x94a3b8))
                        .child(format!("{} · {}", component.name, component.state))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(0x94a3b8))
                .child("CHANNELS"),
        )
        .child(div().text_size(px(11.)).child(ahp_text))
        .children(component_rows)
        .into_any_element()
}

fn nav_button(
    label: &'static str,
    icon: IconName,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    Button::new(label)
        .with_size(Size::Small)
        .ghost()
        .selected(selected)
        .icon(Icon::new(icon).with_size(Size::Small))
        .label(label)
        .on_click(on_click)
        .into_any_element()
}

fn status_row(label: &'static str, value: String) -> AnyElement {
    h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(0x9ca3af))
                .child(label),
        )
        .child(div().text_size(px(12.)).child(value))
        .into_any_element()
}
