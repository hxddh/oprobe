//! The app's own controls on `nsh_base` (which has behaviour and no look), at the platform's
//! density (design app-native "Look"): macOS's compact 13 pt controls, Windows' and GNOME's
//! taller ones, one cell on a terminal. Sections are spaced, not boxed; headings are sentence
//! case. The views compose only these.

use nsh_base::{button, input, switch, theme, toggle, toggle_group};
use nsh_core::element::os;
use nsh_core::platform::Os;
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

/// Whether Windows draws Mica behind the window (Windows 11, build 22000 and later): the OS
/// version nsh reports is 10.0 on Windows 10 and 11 alike, so the build is read once from
/// `ver`. Never off Windows.
pub fn mica() -> bool {
    #[cfg(windows)]
    {
        static MICA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *MICA.get_or_init(|| {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console flashes up.
            let out = std::process::Command::new("cmd")
                .args(["/C", "ver"])
                .creation_flags(0x0800_0000)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            // "Microsoft Windows [Version 10.0.22631.4460]"
            out.split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .find(|v| v.starts_with("10.0."))
                .and_then(|v| v.split('.').nth(2))
                .and_then(|b| b.parse::<u32>().ok())
                .is_some_and(|b| b >= 22000)
        })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// A control's least height: the platform's, grown with the user's text size (Windows'
/// "Make text bigger", macOS's and GNOME's text scaling). Controls take it as a minimum and
/// grow with their text, so nothing clips.
pub fn control_h() -> f32 {
    let m = metrics().control_h;
    if in_terminal() {
        m
    } else {
        m * font_scale().max(1.0)
    }
}

fn radius() -> AbsoluteLength {
    px(theme().radius.0).into()
}

/// The platform's sizes: a control's height, the body text and the secondary text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub control_h: f32,
    pub body: f32,
    pub small: f32,
}

/// The sizes of this platform (macOS's 13 pt, Windows' Segoe 14 px, GNOME's 11 pt Cantarell,
/// a terminal's cells).
pub fn metrics() -> Metrics {
    if in_terminal() {
        return Metrics {
            control_h: 16.0,
            body: 13.0,
            small: 12.0,
        };
    }
    match os() {
        Os::MacOs => Metrics {
            control_h: 24.0,
            body: 13.0,
            small: 11.0,
        },
        Os::Windows => Metrics {
            control_h: 30.0,
            body: 14.0,
            small: 12.0,
        },
        _ => Metrics {
            control_h: 32.0,
            body: 14.0,
            small: 12.0,
        },
    }
}

// Typography: the system font at the platform's sizes, tabular numerals.

pub fn body(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_size(px(metrics().body))
}

pub fn strong(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(metrics().body))
        .font_weight(FontWeight::SEMIBOLD)
}

/// The title of what the window shows (a run's id).
pub fn title(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(metrics().body + 2.0))
        .font_weight(FontWeight::SEMIBOLD)
}

/// A section's heading: body size, semibold, sentence case.
pub fn heading(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(metrics().body))
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn small(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(metrics().small))
        .text_color(move || theme().muted_foreground)
}

/// A label beside a value or above a group (secondary text, medium weight).
pub fn caption(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(px(metrics().small))
        .font_weight(FontWeight::MEDIUM)
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
        .min_h(px(control_h()))
        .py(px(if in_terminal() { 0.0 } else { 2.0 }))
        .px(px(if in_terminal() { 8.0 } else { 12.0 }))
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
                .text_size(px(metrics().body))
                .number_of_lines(1)
                .text_color(fg),
        )
}

/// A borderless icon button of the toolbar, its label the tooltip and the accessible name.
pub fn tool_btn(
    icon: crate::ui::icons::Icon,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let label = label.into_reactive();
    let (a11y, tip) = (label.clone(), label);
    let size = if in_terminal() {
        16.0
    } else {
        28.0 * font_scale().clamp(1.0, 1.5)
    };
    button()
        .on_press(on_press)
        .accessibility_label(move || Some(a11y.get()))
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .text_color(move || theme().muted_foreground)
        .hover(|s| s.bg(move || theme().secondary))
        .active(|s| s.opacity(0.7))
        .focus_visible(ring)
        .disabled_style(|s| s.opacity(0.35))
        .child(
            view()
                .size_full()
                .items_center()
                .justify_center()
                .tooltip(text(tip).text_size(px(12.0)))
                .child(crate::ui::icons::icon(icon, 16.0, None)),
        )
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
        .min_h(px(control_h()))
        .px(px(8.0))
        .text_size(px(metrics().body))
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
                .min_h(px((control_h() - 4.0).max(16.0)))
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
                .child(
                    text(label)
                        .text_size(px(metrics().body - 1.0))
                        .number_of_lines(1),
                )
        }))
}

/// A grouped surface (a form's fields, a set of numbers): a quiet fill, no border.
pub fn panel() -> View {
    view()
        .rounded(move || px(theme().number("radius-card").unwrap_or(8.0)))
        .bg(move || theme().muted)
        .min_w(px(0.0))
}

/// A section: its heading over its content, set apart by space (and a hairline above), not
/// by a box.
pub fn section(heading_text: impl IntoReactive<SharedString>) -> View {
    let h = heading_text.into_reactive();
    let shown = h.clone();
    view()
        .gap_2()
        .pt(px(if in_terminal() { 0.0 } else { 16.0 }))
        .border_t_1()
        .border_color(move || theme().border)
        .min_w(px(0.0))
        .child(dynamic(move || {
            let t = shown.get();
            untracked(|| (!t.is_empty()).then(|| heading(t.clone())))
        }))
}

/// The first section of a view: no hairline above it.
pub fn section_first(heading_text: impl IntoReactive<SharedString>) -> View {
    section(heading_text).border_t(px(0.0)).pt(px(0.0))
}

/// A verdict or severity: a filled dot and its word, in the tone's colour.
pub fn badge(
    label: impl IntoReactive<SharedString>,
    tone: impl Fn() -> Tone + Copy + 'static,
) -> View {
    view()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .self_start()
        .flex_shrink_0()
        .child(dot(tone))
        .child(
            text(label)
                .text_size(px(metrics().small))
                .font_weight(FontWeight::MEDIUM)
                .number_of_lines(1)
                .text_color(move || tone().color()),
        )
}

/// A small filled dot in a tone's colour (a `●` in a terminal).
pub fn dot(tone: impl Fn() -> Tone + Copy + 'static) -> View {
    if in_terminal() {
        view().child(
            text("●")
                .text_size(px(11.0))
                .text_color(move || tone().color()),
        )
    } else {
        view()
            .w(px(8.0))
            .h(px(8.0))
            .rounded_full()
            .flex_shrink_0()
            .bg(move || tone().color())
    }
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
                .text_size(px(metrics().body + 6.0))
                .font_weight(FontWeight::SEMIBOLD)
                .tabular_nums()
                .number_of_lines(1)
                .text_color(move || match tone() {
                    Tone::Neutral => theme().foreground,
                    t => t.color(),
                }),
        )
}

/// The row of tiles: one row when it fits, more when it does not, on a grouped surface.
pub fn tile_row() -> View {
    panel()
        .flex_row()
        .flex_wrap()
        .py(px(if in_terminal() { 0.0 } else { 6.0 }))
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
        .gap_2()
        .p_6()
        .child(
            text(heading)
                .text_size(px(metrics().body + 4.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || theme().muted_foreground),
        )
        .child(small(detail).text_center().max_w(px(420.0)))
}

/// A two-column property row.
pub fn prop(name: impl IntoReactive<SharedString>, value: impl IntoReactive<SharedString>) -> View {
    view()
        .flex_row()
        .gap_2()
        .min_w(px(0.0))
        .child(
            view()
                .w(px(180.0))
                .flex_shrink_0()
                .child(small(name).number_of_lines(1)),
        )
        .child(
            text(value)
                .text_size(px(metrics().small + 1.0))
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

/// A column of a [`table`]: its heading, width and whether its cells are numbers (right-aligned,
/// tabular).
#[derive(Clone, Copy, Debug)]
pub struct Col {
    pub heading: &'static str,
    pub width: f32,
    pub numeric: bool,
}

/// A compact table: a heading row, then one hairline-separated row per item. The first column
/// takes the room left; the others have their widths. It scrolls sideways when the window is
/// narrower than its columns.
pub fn table(cols: &[Col], rows: Vec<Vec<String>>) -> impl IntoElement {
    let cols: Vec<Col> = cols.to_vec();
    let total: f32 = cols.iter().skip(1).map(|c| c.width).sum::<f32>() + 160.0;
    let cell = |c: Col, first: bool, t: String, head: bool| {
        let t = if head {
            caption(t)
        } else if c.numeric {
            text(t).text_size(px(metrics().small + 1.0)).tabular_nums()
        } else {
            text(t).text_size(px(metrics().small + 1.0))
        };
        let v = view().min_w(px(0.0)).child(t.number_of_lines(1));
        let v = if first {
            v.flex_1().min_w(px(120.0))
        } else {
            v.w(px(c.width)).flex_shrink_0()
        };
        if c.numeric { v.items_end() } else { v }
    };
    let row = |cells: Vec<String>, head: bool, cols: &[Col]| {
        view()
            .flex_row()
            .items_center()
            .gap_3()
            .py(px(if in_terminal() { 0.0 } else { 5.0 }))
            .border_t(px(if head { 0.0 } else { 1.0 }))
            .border_color(move || theme().border)
            .children(
                cols.iter()
                    .zip(cells)
                    .enumerate()
                    .map(|(i, (c, t))| cell(*c, i == 0, t, head)),
            )
    };
    let heads: Vec<String> = cols
        .iter()
        .map(|c| crate::ui::i18n::tr(c.heading).to_owned())
        .collect();
    scroll().horizontal().min_w(px(0.0)).child(
        view()
            .min_w(px(total))
            .child(row(heads, true, &cols))
            .children(rows.into_iter().map(|r| row(r, false, &cols))),
    )
}

/// A disclosure: a heading that shows or hides its content (hidden at first).
pub fn disclosure(
    heading_text: impl IntoReactive<SharedString>,
    id: &'static str,
    content: impl Fn() -> View + 'static,
) -> View {
    let open = signal(false);
    let h = heading_text.into_reactive();
    let a11y = h.clone();
    section("")
        .child(
            button()
                .on_press(move || open.update(|o| *o = !*o))
                .accessibility_label(move || Some(a11y.get()))
                .test_id(id)
                .flex_row()
                .items_center()
                .gap_2()
                .self_start()
                .rounded(px(4.0))
                .focus_visible(ring)
                .child(
                    text(move || SharedString::from(if open.get() { "▾" } else { "▸" }))
                        .text_size(px(metrics().body))
                        .text_color(move || theme().muted_foreground),
                )
                .child(heading(h)),
        )
        .child(dynamic(move || {
            let o = open.get();
            untracked(|| o.then(|| content().gap_3()))
        }))
}
