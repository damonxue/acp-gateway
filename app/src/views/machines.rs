use gpui::{
    AnyElement, Context, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Sizable, Size};

use crate::state::AppState;
use crate::views::{h_flex, v_flex};

pub fn render(
    state: &mut AppState,
    _window: &mut Window,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let health = state.health.clone();
    let refresh = cx.listener(|state, _, window, cx| state.refresh_overview(window, cx));
    v_flex()
        .flex_1()
        .min_w_0()
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
                        .child(div().text_size(px(16.)).child("Machines"))
                        .child(
                            Button::new("refresh-machines")
                                .with_size(Size::Small)
                                .ghost()
                                .icon(Icon::new(IconName::Redo2).with_size(Size::Small))
                                .on_click(refresh),
                        ),
                ),
        )
        .child(
            v_flex()
                .gap_3()
                .px_4()
                .py_4()
                .children(health.into_iter().map(|health| {
                    v_flex()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .border_1()
                        .border_color(rgb(0x27303a))
                        .child(div().child(health.machine.name))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x9ca3af))
                                .child(health.machine.hostname),
                        )
                        .child(div().text_size(px(12.)).child(format!(
                                "Endpoint: {}",
                                health
                                    .machine
                                    .public_endpoint
                                    .clone()
                                    .unwrap_or_else(|| "local only".to_owned())
                            )))
                        .child(div().text_size(px(12.)).child(format!(
                            "Sessions: active {} / total {}",
                            health.sessions.active, health.sessions.created_total
                        )))
                        .child(v_flex().gap_1().children(health.components.into_iter().map(
                            |component| {
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(0x9ca3af))
                                            .child(component.name),
                                    )
                                    .child(div().text_size(px(12.)).child(component.state))
                                    .into_any_element()
                            },
                        )))
                        .into_any_element()
                })),
        )
        .into_any_element()
}
