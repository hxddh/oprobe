//! oprobe's component library on top of `nsh_base` (which draws nothing by itself).
//! Every screen composes only these.

use nsh_base::{SelectEntry, SelectItem, button, input, select, tab, tabs, theme};
use nsh_core::prelude::*;

// ---------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------

/// A custom theme token, falling back to a visibly wrong color if missing.
pub fn token(name: &'static str) -> impl Fn() -> AdaptiveColor + Copy + 'static {
    move || theme().color(name).unwrap_or(theme().destructive)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Ok,
    Warn,
    Crit,
    Info,
}

impl Tone {
    pub fn color(self) -> AdaptiveColor {
        match self {
            Tone::Neutral => theme().muted_foreground,
            Tone::Ok => token("ok")(),
            Tone::Warn => token("warn")(),
            Tone::Crit => token("crit")(),
            Tone::Info => token("info")(),
        }
    }
}

/// The color of a stage in "where the time goes" and request waterfalls.
pub fn stage_color(stage: &'static str) -> impl Fn() -> AdaptiveColor + Copy + 'static {
    token(match stage {
        "queue" => "stage-queue",
        "connect" => "stage-connect",
        "upload" | "send" => "stage-upload",
        "server" | "ttfb" => "stage-server",
        "download" | "transfer" => "stage-download",
        _ => "stage-client",
    })
}

fn radius() -> AbsoluteLength {
    px(theme().radius.0).into()
}

// ---------------------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------------------

pub fn title(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_xl().font_weight(FontWeight::SEMIBOLD)
}

pub fn heading(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_sm().font_weight(FontWeight::SEMIBOLD)
}

pub fn muted(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_xs()
        .text_color(move || theme().muted_foreground)
}

pub fn mono(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_xs()
        .font_family(move || {
            theme()
                .font_mono
                .clone()
                .unwrap_or_else(|| "monospace".into())
        })
        .tabular_nums()
}

// ---------------------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------------------

/// A bordered surface with a heading.
pub fn card(heading_text: &'static str) -> View {
    view()
        .gap_3()
        .p_4()
        .rounded(move || px(theme().number("radius-card").unwrap_or(10.0)))
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .min_w(px(0.0))
        .child(heading(heading_text))
}

pub fn row() -> View {
    view().flex_row().items_center().gap_3().min_w(px(0.0))
}

// ---------------------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
}

/// The app's button.
pub fn btn(
    label: impl IntoReactive<SharedString>,
    kind: Kind,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    btn_with(label, kind, false, on_press)
}

/// The app's button, optionally disabled.
pub fn btn_with(
    label: impl IntoReactive<SharedString>,
    kind: Kind,
    disabled: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    styled_button(label, move || kind, disabled, on_press)
}

/// The app's button with a kind that follows state (Run ⇄ Stop keeps one element, and so
/// keyboard focus).
pub fn btn_reactive(
    label: impl IntoReactive<SharedString>,
    kind: impl Fn() -> Kind + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    styled_button(label, kind, false, on_press)
}

fn styled_button(
    label: impl IntoReactive<SharedString>,
    kind: impl Fn() -> Kind + Copy + 'static,
    disabled: bool,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let bg = move || match kind() {
        Kind::Primary => theme().primary,
        Kind::Secondary => theme().secondary,
        Kind::Danger => theme().destructive,
    };
    let fg = move || match kind() {
        Kind::Primary | Kind::Danger => theme().primary_foreground,
        Kind::Secondary => theme().secondary_foreground,
    };
    button()
        .on_press(on_press)
        .disabled(disabled)
        .h_9()
        .px_4()
        .items_center()
        .justify_center()
        .rounded(radius)
        .bg(bg)
        .border_1()
        .border_color(move || theme().border)
        .hover(|s| s.opacity(0.9))
        .active(|s| s.opacity(0.75))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(move || theme().ring))
        .disabled_style(|s| s.opacity(0.45))
        .child(
            text(label)
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg),
        )
}

/// A labelled text field bound to a signal. Screen readers announce `label` (the visible label
/// is a separate text). Placeholders are examples ("e.g. …"), never values: in a terminal they
/// would otherwise read as filled in.
pub fn field(label: &'static str, placeholder: &'static str, value: Signal<String>) -> View {
    view().gap_1().min_w(px(0.0)).child(muted(label)).child(
        input()
            .test_id(label)
            .label(label)
            .value(move || value.get())
            .on_change(move |t: &str| value.set(t.to_owned()))
            .placeholder(placeholder)
            .placeholder_color(|| color(theme().muted_foreground))
            .caret_color(|| color(theme().primary))
            .h_9()
            .px_3()
            .text_sm()
            .rounded(radius)
            .border_1()
            .border_color(move || theme().input)
            .bg(move || theme().background)
            .focus(|s| s.border_color(move || theme().ring)),
    )
}

/// An on/off pill.
pub fn toggle(label: &'static str, on: Signal<bool>) -> impl IntoElement {
    button()
        .selected(move || on.get())
        .on_press(move || on.update(|v| *v = !*v))
        .accessibility_label(label)
        .h_8()
        .px_3()
        .items_center()
        .justify_center()
        .rounded(radius)
        .border_1()
        .border_color(move || theme().input)
        .bg(move || {
            if on.get() {
                theme().accent
            } else {
                theme().background
            }
        })
        .focus_visible(|s| s.outline(px(2.0)).outline_color(move || theme().ring))
        .child(text(move || format!("{} {label}", if on.get() { "●" } else { "○" })).text_xs())
}

/// A labelled select over `(value, title)` pairs.
pub fn picker(
    label: &'static str,
    items: &[(&'static str, &'static str)],
    value: Signal<Option<SharedString>>,
) -> View {
    let items_vec: Vec<SelectItem> = items.iter().map(|(v, t)| SelectItem::new(*v, *t)).collect();
    let titles: Vec<(&'static str, &'static str)> = items.to_vec();
    view().gap_1().child(muted(label)).child(
        select()
            .items(items_vec)
            .value(move || value.get())
            .on_change(move |next: &Option<SharedString>| value.set(next.clone()))
            .accessibility_label(label)
            .offset(px(4.0))
            .popup_style(|s| {
                s.p_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(move || theme().border)
                    .bg(move || theme().popover)
                    .shadow_md()
            })
            .item_class("rounded-md focus:bg-accent selected:font-medium")
            .render_item(|item: &SelectEntry| {
                view()
                    .px_2()
                    .py_1p5()
                    .child(text(item.title.clone()).text_sm())
            })
            .trigger(
                view()
                    .h_9()
                    .px_3()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .rounded(radius)
                    .border_1()
                    .border_color(move || theme().input)
                    .bg(move || theme().background)
                    .child(
                        text(move || {
                            let v = value.get();
                            titles
                                .iter()
                                .find(|(k, _)| Some(SharedString::from(*k)) == v)
                                .map(|(_, t)| SharedString::from(*t))
                                .unwrap_or_else(|| "Choose…".into())
                        })
                        .text_sm(),
                    )
                    .child(muted("▾")),
            ),
    )
}

/// A tab row bound to a signal.
pub fn tab_row(
    value: Signal<SharedString>,
    items: &[(&'static str, &'static str)],
) -> impl IntoElement {
    let item = move |key: &'static str, label: &'static str| {
        let selected = move || value.get() == key;
        tab()
            .selected(selected)
            .on_press(move |_: &()| value.set(key.into()))
            .px_3()
            .py_1p5()
            .child(text(label).text_sm().text_color(move || {
                if selected() {
                    theme().foreground
                } else {
                    theme().muted_foreground
                }
            }))
    };
    tabs()
        .flex_row()
        .self_start()
        .gap_1()
        .p_1()
        .rounded(radius)
        .bg(move || theme().muted)
        .indicator_style(|s| s.top_1().bottom_1().rounded_md().bg(move || theme().card))
        .children(items.iter().map(move |(k, l)| item(k, l)))
}

// ---------------------------------------------------------------------------------------
// Data display
// ---------------------------------------------------------------------------------------

/// A KPI tile.
pub fn stat(
    label: &'static str,
    value: impl IntoReactive<SharedString>,
    tone: impl Fn() -> Tone + 'static,
) -> View {
    view()
        .flex_1()
        .min_w(px(110.0))
        .gap_1()
        .p_3()
        .rounded(radius)
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .child(muted(label))
        .child(
            text(value)
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .tabular_nums()
                .text_color(move || match tone() {
                    Tone::Neutral => theme().foreground,
                    t => t.color(),
                }),
        )
}

/// A horizontal meter: `fraction` of the track filled in `fill`.
pub fn meter(fraction: f32, fill: impl Fn() -> AdaptiveColor + 'static) -> View {
    view()
        .flex_1()
        .h_2()
        .min_w(px(40.0))
        .rounded_full()
        .bg(move || theme().muted)
        .child(
            view()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(fill),
        )
}

/// A severity badge.
pub fn badge(label: impl IntoReactive<SharedString>, tone: Tone) -> View {
    view()
        .px_2()
        .py_0p5()
        .rounded(radius)
        .border_1()
        .border_color(move || tone.color())
        .child(
            text(label)
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || tone.color()),
        )
}

/// A table header row with fixed column widths (the first column flexes).
pub fn table_head(cols: &[(&'static str, f32)]) -> View {
    let mut r = view()
        .flex_row()
        .gap_2()
        .py_1p5()
        .border_b_1()
        .border_color(move || theme().border);
    for (i, (label, w)) in cols.iter().enumerate() {
        let cell = muted(*label);
        r = if i == 0 {
            r.child(view().flex_1().min_w(px(*w)).child(cell))
        } else {
            r.child(view().w(px(*w)).items_end().child(cell))
        };
    }
    r
}

/// A table row matching [`table_head`].
pub fn table_row(cols: &[(&'static str, f32)], cells: Vec<AnyElement>) -> View {
    let mut r = view().flex_row().gap_2().py_1().items_center();
    for (i, (cell, (_, w))) in cells.into_iter().zip(cols.iter()).enumerate() {
        r = if i == 0 {
            r.child(view().flex_1().min_w(px(*w)).child(cell))
        } else {
            r.child(view().w(px(*w)).items_end().child(cell))
        };
    }
    r
}

/// A text cell.
pub fn cell(t: impl Into<SharedString>) -> AnyElement {
    text(t.into())
        .text_sm()
        .tabular_nums()
        .number_of_lines(1)
        .into_element()
}

/// A colored text cell.
pub fn cell_tone(t: impl Into<SharedString>, tone: Tone) -> AnyElement {
    text(t.into())
        .text_sm()
        .tabular_nums()
        .number_of_lines(1)
        .text_color(move || match tone {
            Tone::Neutral => theme().foreground,
            t => t.color(),
        })
        .into_element()
}
