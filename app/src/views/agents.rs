use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
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
                        .child(div().text_size(px(16.)).child("Agents"))
                        .child(
                            Button::new("refresh-agents")
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
                .children(state.agents.iter().map(|agent| {
                    let selected = state
                        .selected_agent_id
                        .as_ref()
                        .is_some_and(|id| id == &agent.id);
                    let launch_id = SharedString::from(format!("launch-{}", agent.id));
                    v_flex()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .border_1()
                        .border_color(rgb(0x27303a))
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(div().child(agent.name.clone()))
                                .child(
                                    Button::new(launch_id)
                                        .with_size(Size::Small)
                                        .primary()
                                        .label("Launch")
                                        .on_click(cx.listener({
                                            let agent_id = agent.id.clone();
                                            move |state, _, window, cx| {
                                                state.launch_session(agent_id.clone(), window, cx)
                                            }
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x9ca3af))
                                .child(agent.id.clone()),
                        )
                        .when(selected, |this| this.bg(rgb(0x172554)))
                        .into_any_element()
                })),
        )
        .into_any_element()
}
