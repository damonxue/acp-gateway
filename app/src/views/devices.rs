use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable, Icon, IconName, Sizable, Size};
use qrcode::QrCode;

use crate::state::AppState;
use crate::views::{h_flex, v_flex};

pub fn render(state: &mut AppState, window: &mut Window, cx: &mut Context<AppState>) -> AnyElement {
    let pair = cx.listener(|state, _, window, cx| state.begin_pairing(window, cx));
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
                        .child(div().text_size(px(16.)).child("Devices"))
                        .child(
                            Button::new("pair-device")
                                .with_size(Size::Small)
                                .primary()
                                .label("Pair")
                                .on_click(pair),
                        ),
                ),
        )
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(channel_panel(state, window, cx))
                .child(device_list(state, window, cx)),
        )
        .into_any_element()
}

fn channel_panel(state: &AppState, window: &mut Window, cx: &mut Context<AppState>) -> AnyElement {
    let ahp = state.ahp_status.status.as_ref();
    let wechat_health = state.health.as_ref().and_then(|health| {
        health
            .components
            .iter()
            .find(|component| component.name == "wechat")
    });
    let qr = ahp
        .and_then(|status| status.qr_code.as_deref())
        .and_then(|payload| QrCode::new(payload).ok());
    let bind_id = state.selected_session_id.clone();
    let bind_button = bind_id.map(|id| {
        Button::new("bind-wechat")
            .with_size(Size::Small)
            .primary()
            .label("Bind WeChat to selected session")
            .on_click(
                cx.listener(move |state, _, window, cx| state.bind_ahp(id.clone(), window, cx)),
            )
            .into_any_element()
    });
    v_flex()
        .w(px(250.))
        .gap_3()
        .px_4()
        .py_4()
        .border_r_1()
        .border_color(rgb(0x27303a))
        .child(div().text_size(px(14.)).child("Channels"))
        .child(channel_row("WeChat", ahp.map(|s| s.state.as_str()).or_else(|| wechat_health.map(|component| component.state.as_str())).unwrap_or("not configured")))
        .child(ahp.and_then(|s| s.session_id.as_ref()).map(|id| div().text_size(px(11.)).text_color(rgb(0x94a3b8)).child(format!("bound session: {id}"))).unwrap_or_else(|| div().text_size(px(11.)).text_color(rgb(0x64748b)).child("No WeChat session binding")))
        .when_some(qr, |this, qr| this.child(render_qr(qr)))
        .when_some(bind_button, |this, button| this.child(button))
        .child(Button::new("unbind-wechat").with_size(Size::Small).ghost().label("Unbind WeChat").on_click(cx.listener(|state, _, window, cx| state.unbind_ahp(window, cx))))
        .child(div().text_size(px(11.)).text_color(rgb(0x64748b)).child("Lark and Telegram use webhook/token configuration; their live state appears here when enabled."))
        .into_any_element()
}

fn channel_row(name: &'static str, state: &str) -> AnyElement {
    h_flex()
        .items_center()
        .justify_between()
        .child(div().text_size(px(12.)).child(name))
        .child(
            div()
                .text_size(px(11.))
                .text_color(if matches!(state, "ready" | "bound" | "up") {
                    rgb(0x4ade80)
                } else {
                    rgb(0xfbbf24)
                })
                .child(state.to_owned()),
        )
        .into_any_element()
}

fn pairing_panel(state: &AppState) -> AnyElement {
    let content = state
        .pairing_offer
        .as_ref()
        .map(|offer| {
            let qr = QrCode::new(offer.to_qr_payload().unwrap_or_default()).ok();
            v_flex()
                .gap_3()
                .px_4()
                .py_4()
                .border_r_1()
                .border_color(rgb(0x27303a))
                .child(div().text_size(px(14.)).child("Pairing"))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(format!("Code: {}", offer.pairing_code)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(format!("Expires: {}", offer.expires_at)),
                )
                .when_some(qr, |this, qr| this.child(render_qr(qr)))
                .child(
                    div()
                        .text_size(px(12.))
                        .child(offer.machine_public_key.clone()),
                )
        })
        .unwrap_or_else(|| {
            v_flex()
                .gap_3()
                .px_4()
                .py_4()
                .border_r_1()
                .border_color(rgb(0x27303a))
                .child(div().text_size(px(14.)).child("Pairing"))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child("Use the Pair button to generate a QR code."),
                )
        });

    content.into_any_element()
}

fn render_qr(code: QrCode) -> AnyElement {
    let width = code.width();
    let colors = code.to_colors();
    v_flex()
        .gap_0()
        .p_2()
        .bg(rgb(0xffffff))
        .children((0..width).map(|y| {
            h_flex().gap_0().children((0..width).map(|x| {
                let dark = colors[y * width + x] == qrcode::types::Color::Dark;
                div()
                    .size(px(4.))
                    .bg(if dark { rgb(0x111827) } else { rgb(0xffffff) })
            }))
        }))
        .into_any_element()
}

fn device_list(state: &AppState, _window: &mut Window, cx: &mut Context<AppState>) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_3()
        .px_4()
        .py_4()
        .children(state.devices.iter().map(|device| {
            let revoke_id = SharedString::from(format!("revoke-{}", device.id));
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
                        .child(div().child(device.name.clone()))
                        .child(div().text_size(px(12.)).child(if device.revoked {
                            "revoked"
                        } else {
                            "active"
                        })),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(device.platform.clone()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(0x9ca3af))
                        .child(device.id.to_string()),
                )
                .child(
                    Button::new(revoke_id)
                        .with_size(Size::Small)
                        .danger()
                        .label("Revoke")
                        .disabled(device.revoked)
                        .on_click(cx.listener({
                            let device_id = device.id.to_string();
                            move |state, _, window, cx| {
                                state.revoke_device(device_id.clone(), window, cx)
                            }
                        })),
                )
                .into_any_element()
        }))
        .into_any_element()
}
