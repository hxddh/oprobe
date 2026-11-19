use nsh_base::{button, theme};
use nsh_core::prelude::*;

pub fn action_button(
    label: impl IntoReactive<SharedString>,
    primary: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    button()
        .px_3()
        .py_2()
        .rounded(move || theme().radius)
        .bg(move || {
            if primary {
                theme().primary
            } else {
                theme().secondary
            }
        })
        .hover(move |s| s.opacity(0.9))
        .active(move |s| s.opacity(0.75))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(move || theme().ring))
        .on_press(on_press)
        .child(
            text(label).text_sm().text_color(move || {
                if primary {
                    theme().primary_foreground
                } else {
                    theme().secondary_foreground
                }
            }),
        )
}

pub fn chip_toggle(
    label: &'static str,
    selected: impl IntoReactive<bool> + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    button()
        .px_3()
        .py_1()
        .rounded(move || theme().radius)
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .style_when(selected, |s| {
            s.bg(move || theme().accent)
                .border_color(move || theme().primary)
        })
        .hover(|s| s.bg(move || theme().muted))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(move || theme().ring))
        .on_press(on_press)
        .child(text(label).text_sm())
}

pub fn kpi_card(
    title: &'static str,
    headline: Memo<SharedString>,
    sub: Memo<SharedString>,
) -> impl IntoElement {
    view()
        .flex_1()
        .min_w(px(0.0))
        .p_3()
        .gap_1()
        .rounded(move || theme().radius)
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .child(
            text(title)
                .text_sm()
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(headline)
                .font_weight(FontWeight::SEMIBOLD)
                .tabular_nums()
                .number_of_lines(1),
        )
        .child(
            text(sub)
                .text_sm()
                .tabular_nums()
                .text_color(move || theme().muted_foreground)
                .number_of_lines(1),
        )
}

pub fn section_frame(title: &'static str, body: impl IntoElement) -> impl IntoElement {
    view()
        .p_3()
        .gap_2()
        .rounded(move || theme().radius)
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .child(text(title).font_weight(FontWeight::MEDIUM))
        .child(body)
}
