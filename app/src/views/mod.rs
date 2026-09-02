//! View modules for the macOS app.
//!
//! macOS App 的视图模块。

pub mod agents;
pub mod devices;
pub mod integrations;
pub mod machines;
pub mod sessions;
pub mod sidebar;

use gpui::{AnyElement, Div, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px, rgb, prelude::FluentBuilder as _};
use gpui_component::StyledExt;

pub(crate) fn h_flex() -> Div {
    div().h_flex()
}

pub(crate) fn v_flex() -> Div {
    div().v_flex()
}

pub(crate) fn section_header(title: impl Into<SharedString>, detail: Option<impl Into<SharedString>>) -> AnyElement {
    let title = title.into();
    let detail = detail.map(Into::into);
    div()
        .w_full()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(0x2b2f36))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(div().text_size(gpui::px(16.)).child(title))
                .when_some(detail, |this, detail| this.child(div().text_size(gpui::px(12.)).text_color(rgb(0x9ca3af)).child(detail))),
        )
        .into_any_element()
}

pub(crate) fn pill(text: impl Into<SharedString>, bg: Hsla, fg: Hsla) -> AnyElement {
    h_flex()
        .items_center()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(bg)
        .text_color(fg)
        .child(text.into())
        .into_any_element()
}

pub(crate) fn mono_value(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(gpui::px(12.))
        .child(text.into())
        .into_any_element()
}
