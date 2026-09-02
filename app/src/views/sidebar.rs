use gpui::{App, AnyElement, Context, IntoElement, ParentElement, Styled, Window, div, px, rgb, prelude::FluentBuilder as _};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Selectable, Sizable, Size};

use crate::state::{AppState, AppTab};
use crate::views::{h_flex, v_flex};

pub fn render(state: &mut AppState, _window: &mut Window, cx: &mut Context<AppState>) -> AnyElement {
    let sessions_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Sessions, cx));
    let machines_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Machines, cx));
    let agents_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Agents, cx));
    let devices_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Devices, cx));
    let zed_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Integrations, cx));
    let logs_click = cx.listener(|state: &mut AppState, _, _window: &mut Window, cx| state.set_tab(AppTab::Logs, cx));
    let base_url = state.services.client.base_url().to_owned();
    let daemon_status = state.daemon_status_label();
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
                        .child(h_flex().items_center().gap_2().child(Icon::new(IconName::SquareTerminal).with_size(Size::Small)).child("Agent Gateway"))
                        .child(div().text_size(px(12.)).text_color(rgb(0x9ca3af)).child(base_url)),
                ),
        )
        .child(
            v_flex()
                .px_3()
                .py_3()
                .gap_2()
                .children([
                    nav_button("Sessions", IconName::Inbox, state.tab == AppTab::Sessions, sessions_click),
                    nav_button("Machines", IconName::Building2, state.tab == AppTab::Machines, machines_click),
                    nav_button("Agents", IconName::Bot, state.tab == AppTab::Agents, agents_click),
                    nav_button("Devices", IconName::CircleUser, state.tab == AppTab::Devices, devices_click),
                    nav_button("Zed", IconName::Settings2, state.tab == AppTab::Integrations, zed_click),
                    nav_button("Logs", IconName::SquareTerminal, state.tab == AppTab::Logs, logs_click),
                ]),
        )
        .child(
            div()
                .mt_auto()
                .px_4()
                .py_4()
                .border_t_1()
                .border_color(rgb(0x27303a))
                .child(
                    v_flex()
                        .gap_2()
                        .child(status_row("Daemon", daemon_status))
                        .child(status_row("Gateway", connection_status))
                        .child(status_row("Zed", zed_status))
                        .child(
                            h_flex()
                                .gap_2()
                                .children(daemon_controls(state, cx)),
                        ),
                ),
        )
        .into_any_element()
}

fn daemon_controls(state: &mut AppState, cx: &mut Context<AppState>) -> Vec<AnyElement> {
    match state.daemon_status {
        crate::state::DaemonStatus::Running { .. } => vec![
            Button::new("daemon-stop")
                .with_size(Size::Small)
                .danger()
                .icon(Icon::new(IconName::CircleX).with_size(Size::Small))
                .label("Stop")
                .on_click(cx.listener(|state, _, _, cx| {
                    state.stop_daemon();
                    cx.notify();
                }))
                .into_any_element(),
        ],
        _ => vec![
            Button::new("daemon-start")
                .with_size(Size::Small)
                .primary()
                .icon(Icon::new(IconName::Plus).with_size(Size::Small))
                .label("Start")
                .on_click(cx.listener(|state, _, _, cx| {
                    state.start_daemon();
                    cx.notify();
                }))
                .into_any_element(),
        ],
    }
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
        .child(div().text_size(px(12.)).text_color(rgb(0x9ca3af)).child(label))
        .child(div().text_size(px(12.)).child(value))
        .into_any_element()
}
