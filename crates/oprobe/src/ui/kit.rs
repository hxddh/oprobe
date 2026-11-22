//! The app's own controls on `nsh_base` (which has behaviour and no look): compact,
//! native-density, in the theme's tokens. The views compose only these.

use nsh_base::{button, input, switch, theme, toggle, toggle_group};
use nsh_core::prelude::*;

use crate::analysis::diagnosis::Severity;
use crate::model::VerdictLevel;

/// A theme token of `theme.json`, falling back to a visibly wrong colour if missing.
pub fn token(name: &'static str) -> impl Fn() -> AdaptiveColor + Copy + 'static {
    move || theme().color(name).unwrap_or(theme().destructive)
}

/// The resolved colour of a token now (canvas paint).
pub fn paint(name: &'static str) -> Hsla {
    color(token(name)())
}

/// The token of a stage: one colour per stage everywhere (bars, waterfalls, legends).
pub fn stage_token(stage: &str) -> &'static str {
    match stage {
        "queue" => "stage-queue",
        "cache" => "stage-cache",
        "connect" => "stage-connect",
        "upload" | "send" => "stage-upload",
        "server" | "ttfb" => "stage-server",
        "download" | "body" => "stage-download",
        "stall" => "stage-stall",
        "retry" => "stage-retry",
        _ => "stage-client",
    }
}

/// How a value reads: neutral, good, a warning, bad, informational, undecided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Ok,
    Warn,
    Crit,
    Info,
    Inconclusive,
}

impl Tone {
    pub fn color(self) -> AdaptiveColor {
        match self {
            Tone::Neutral => theme().muted_foreground,
            Tone::Ok => token("ok")(),
            Tone::Warn => token("warn")(),
            Tone::Crit => token("crit")(),
            Tone::Info => token("info")(),
            Tone::Inconclusive => token("inconclusive")(),
        }
    }

    pub fn of_verdict(v: VerdictLevel) -> Tone {
        match v {
            VerdictLevel::Ok => Tone::Ok,
            VerdictLevel::Review => Tone::Info,
            VerdictLevel::Attention => Tone::Warn,
            VerdictLevel::Critical => Tone::Crit,
        }
    }

    pub fn of_severity(s: Severity) -> Tone {
        match s {
            Severity::Critical => Tone::Crit,
            Severity::Warning => Tone::Warn,
            _ => Tone::Info,
        }
    }
}

/// Whether the app draws on a terminal grid (character cells).
pub fn in_terminal() -> bool {
    nsh_base::terminal::is_terminal()
}

fn radius() -> AbsoluteLength {
    px(theme().radius.0).into()
}

// Typography: the system font at native desktop sizes (13 pt body), tabular numerals.

pub fn body(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_size(px(13.0))
}

pub fn strong(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn title(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(17.0))
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn small(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(11.0))
        .text_color(move || theme().muted_foreground)
}

/// A section heading.
pub fn caption(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(move || theme().muted_foreground)
}

pub fn mono(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(12.0))
        .font_family(move || {
            theme()
                .font_mono
                .clone()
                .unwrap_or_else(|| "monospace".into())
        })
        .tabular_nums()
}

/// The focus ring: an outline in a window; a reversed band in a terminal (outlines draw
/// nothing there), so the keyboard focus is always visible.
pub fn ring(s: nsh_core::element::StyleRefinement) -> nsh_core::element::StyleRefinement {
    if in_terminal() {
        s.bg(move || theme().primary)
            .text_color(move || theme().primary_foreground)
    } else {
        s.outline(px(2.0)).outline_color(move || theme().ring)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The default action (filled accent).
    Primary,
    /// A bordered push button.
    Plain,
    /// A destructive action.
    Danger,
    /// Borderless (toolbars, inline links).
    Ghost,
}

/// The app's push button; its accessible label is its label. `kind` may follow state (Run ⇄
/// Stop stays one element, so the keyboard focus stays on it).
pub fn btn(
    label: impl IntoReactive<SharedString>,
    kind: impl Fn() -> Kind + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let label = label.into_reactive();
    let a11y = label.clone();
    let bg = move || match kind() {
        Kind::Primary => theme().primary,
        Kind::Danger => theme().destructive,
        Kind::Plain => theme().background,
        Kind::Ghost => transparent_black().into(),
    };
    let fg = move || match kind() {
        Kind::Primary | Kind::Danger => theme().primary_foreground,
        Kind::Plain | Kind::Ghost => theme().foreground,
    };
    button()
        .on_press(on_press)
        .accessibility_label(move || Some(a11y.get()))
        .h(px(26.0))
        .px(px(10.0))
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_center()
        .rounded(radius)
        .bg(bg)
        .border_1()
        .border_color(move || match kind() {
            Kind::Plain => theme().input,
            Kind::Ghost => transparent_black().into(),
            _ => bg(),
        })
        .hover(|s| s.opacity(0.88))
        .active(|s| s.opacity(0.7))
        .focus_visible(ring)
        .disabled_style(|s| s.opacity(0.4))
        .child(
            text(label)
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .number_of_lines(1)
                .text_color(fg),
        )
}

/// A pressable list row (history, findings, navigation): keyboard focusable.
pub fn row_button(
    label: impl IntoReactive<SharedString>,
    selected: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let label = label.into_reactive();
    button()
        .selected(selected)
        .on_press(on_press)
        .accessibility_label(move || Some(label.get()))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(radius)
        .hover(|s| s.bg(move || theme().secondary))
        .bg(move || {
            if selected() {
                theme().accent
            } else {
                transparent_black().into()
            }
        })
        .focus_visible(ring)
}

/// A text field bound to a signal; `id` is its test id, `label` its accessible name.
pub fn field(
    id: &'static str,
    label: impl IntoReactive<SharedString>,
    placeholder: impl Into<String>,
    value: impl Fn() -> String + 'static,
    on_change: impl Fn(String) + 'static,
) -> Input {
    let label = label.into_reactive();
    input()
        .test_id(id)
        .label(move || label.get())
        .value(value)
        .on_change(move |t: &str| on_change(t.to_owned()))
        .placeholder(placeholder.into())
        .placeholder_color(|| color(theme().muted_foreground))
        .caret_color(|| color(theme().primary))
        .h(px(26.0))
        .px(px(8.0))
        .text_size(px(13.0))
        .rounded(radius)
        .border_1()
        .border_color(move || theme().input)
        .bg(move || theme().background)
        .focus(|s| s.border_color(move || theme().ring))
}

/// An on/off switch; `label` is its accessible name.
pub fn switch_on(
    label: impl IntoReactive<SharedString>,
    on: impl Fn() -> bool + 'static,
    set: impl Fn(bool) + 'static,
) -> impl IntoElement {
    let label = label.into_reactive();
    switch()
        .checked(on)
        .on_change(move |next: &bool| set(*next))
        .accessibility_label(move || Some(label.get()))
        .flex_row()
        .items_center()
        .track_style(|s| {
            s.w(px(30.0))
                .h(px(18.0))
                .p(px(2.0))
                .rounded_full()
                .bg(move || theme().input)
                .checked(|s| s.bg(move || theme().primary))
        })
        .thumb_style(|s| {
            s.size(px(14.0))
                .rounded_full()
                .bg(move || theme().background)
                .shadow_sm()
                .checked(|s| s.translate_x(px(12.0)))
        })
}

/// A segmented control: `options` are `(value, label)`.
pub fn segmented<T: Copy + PartialEq + 'static>(
    options: Vec<(T, SharedString)>,
    value: impl Fn() -> T + Copy + 'static,
    set: impl Fn(T) + Copy + 'static,
) -> impl IntoElement {
    toggle_group()
        .flex_row()
        .flex_wrap()
        .p(px(2.0))
        .gap(px(2.0))
        .rounded(radius)
        .bg(move || theme().muted)
        .children(options.into_iter().map(move |(key, label)| {
            toggle()
                .pressed(move || value() == key)
                .accessibility_label(label.clone())
                .on_change(move |_: &bool| set(key))
                .px(px(10.0))
                .h(px(22.0))
                .justify_center()
                .rounded(radius)
                .bg(move || {
                    if value() == key {
                        theme().background
                    } else {
                        transparent_black().into()
                    }
                })
                .focus_visible(ring)
                .child(text(label).text_size(px(12.0)).number_of_lines(1))
        }))
}

/// A bordered surface.
pub fn panel() -> View {
    view()
        .rounded(move || px(theme().number("radius-card").unwrap_or(8.0)))
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .min_w(px(0.0))
}

/// A section: a heading over its content, in a panel.
pub fn section(heading: impl IntoReactive<SharedString>) -> View {
    panel().p_3().gap_2().child(caption(heading))
}

/// A coloured pill.
pub fn badge(
    label: impl IntoReactive<SharedString>,
    tone: impl Fn() -> Tone + Copy + 'static,
) -> View {
    view()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(radius)
        .border_1()
        .border_color(move || tone().color())
        .self_start()
        .flex_shrink_0()
        .child(
            text(label)
                .text_size(px(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .number_of_lines(1)
                .text_color(move || tone().color()),
        )
}

/// A swatch of a token's colour: a square in a window, a coloured `■` in a terminal.
pub fn swatch(tok: &'static str) -> View {
    if in_terminal() {
        view().child(text("■").text_size(px(11.0)).text_color(token(tok)))
    } else {
        view().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(token(tok))
    }
}

/// A key number: fixed height, never wraps, so values arriving never move the layout.
pub fn tile(
    label: impl IntoReactive<SharedString>,
    value: impl Fn() -> SharedString + 'static,
    tone: impl Fn() -> Tone + Copy + 'static,
) -> View {
    let term = in_terminal();
    view()
        .flex_1()
        .min_w(px(if term { 104.0 } else { 96.0 }))
        .h(px(if term { 48.0 } else { 56.0 }))
        .justify_center()
        .gap(px(2.0))
        .px(px(10.0))
        .child(caption(label).number_of_lines(1))
        .child(
            text(value)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .tabular_nums()
                .number_of_lines(1)
                .text_color(move || match tone() {
                    Tone::Neutral => theme().foreground,
                    t => t.color(),
                }),
        )
}

/// The row of tiles: one row when it fits, more when it does not.
pub fn tile_row() -> View {
    view()
        .flex_row()
        .flex_wrap()
        .rounded(px(6.0))
        .border_1()
        .border_color(move || theme().border)
        .overflow_hidden()
}

/// A centred message for an empty pane.
pub fn empty(
    heading: impl IntoReactive<SharedString>,
    detail: impl IntoReactive<SharedString>,
) -> View {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_1()
        .p_6()
        .child(strong(heading))
        .child(small(detail).text_center().max_w(px(460.0)))
}

/// A two-column property row.
pub fn prop(name: impl IntoReactive<SharedString>, value: impl IntoReactive<SharedString>) -> View {
    view()
        .flex_row()
        .gap_2()
        .min_w(px(0.0))
        .child(
            view()
                .w(px(200.0))
                .flex_shrink_0()
                .child(small(name).number_of_lines(1)),
        )
        .child(
            text(value)
                .text_size(px(12.0))
                .tabular_nums()
                .number_of_lines(1)
                .flex_1()
                .min_w(px(0.0))
                .selectable(true),
        )
}

/// A horizontal bar of `ratio` (0–1) in a token's colour, on the muted track.
pub fn meter(ratio: f64, tok: &'static str) -> View {
    let r = ratio.clamp(0.0, 1.0) as f32;
    view()
        .flex_1()
        .h(px(8.0))
        .min_w(px(40.0))
        .rounded(px(2.0))
        .bg(move || theme().muted)
        .overflow_hidden()
        .child(view().h_full().w(relative(r)).bg(token(tok)))
}
