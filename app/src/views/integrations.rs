use gpui::{
    Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Sizable, Size};

use crate::state::AppState;
use crate::views::{h_flex, v_flex};

pub fn render(
    state: &mut AppState,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> impl IntoElement {
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
                        .child(div().text_size(px(16.)).child("Zed"))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("enable-zed")
                                        .with_size(Size::Small)
                                        .primary()
                                        .label("Enable")
                                        .on_click(cx.listener(|state, _, window, cx| {
                                            state.enable_zed(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("disable-zed")
                                        .with_size(Size::Small)
                                        .danger()
                                        .label("Disable")
                                        .on_click(cx.listener(|state, _, window, cx| {
                                            state.disable_zed(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("restore-zed")
                                        .with_size(Size::Small)
                                        .ghost()
                                        .label("Restore backup")
                                        .on_click(cx.listener(|state, _, window, cx| {
                                            state.restore_zed_backup(window, cx)
                                        })),
                                ),
                        ),
                ),
        )
        .child(
            v_flex()
                .gap_3()
                .px_4()
                .py_4()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(format!(
                            "Settings: {}",
                            state.zed_snapshot.settings_path.display()
                        )),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(format!(
                            "Backup: {}",
                            state.zed_snapshot.backup_path.display()
                        )),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(format!(
                            "Record: {}",
                            state.zed_snapshot.record_path.display()
                        )),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .child(format!("Enabled: {}", state.zed_snapshot.enabled)),
                )
                .children(state.zed_snapshot.agent_servers.iter().map(|entry| {
                    v_flex()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .border_1()
                        .border_color(rgb(0x27303a))
                        .child(div().child(entry.key.clone()))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0x9ca3af))
                                .child(entry.kind.clone()),
                        )
                        .when_some(entry.command.clone(), |this, command| {
                            this.child(div().text_size(px(12.)).child(command))
                        })
                        .when(!entry.args.is_empty(), |this| {
                            this.child(div().text_size(px(12.)).child(entry.args.join(" ")))
                        })
                })),
        )
}
