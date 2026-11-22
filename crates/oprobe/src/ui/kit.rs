//! oprobe's own component layer on `nsh_base` (which has behavior and no look): compact,
//! native-density controls in the theme's tokens. Screens compose only these.

use nsh_base::{
    SelectEntry, SelectItem, button, input, select, switch, theme, toggle, toggle_group, tooltip,
};
use nsh_core::prelude::*;

use crate::analysis::diagnosis::Severity;
use crate::model::VerdictLevel;
use crate::ui::model::verdicts::{self, Level};

// ---------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------

/// A custom theme token, falling back to a visibly wrong color if missing.
pub fn token(name: &'static str) -> impl Fn() -> AdaptiveColor + Copy + 'static {
    move || theme().color(name).unwrap_or(theme().destructive)
}

/// The token of a stage. One color per stage everywhere (bars, charts, waterfalls).
pub fn stage_token(stage: &str) -> &'static str {
    match stage {
        "queue" => "stage-queue",
        "cache" => "stage-cache",
        "connect" | "dns" | "tcp" | "tls" | "proxy" => "stage-connect",
        "upload" | "send" => "stage-upload",
        "server" | "ttfb" => "stage-server",
        "download" | "transfer" | "body" => "stage-download",
        "stall" => "stage-stall",
        "retry" => "stage-retry",
        _ => "stage-client",
    }
}

pub fn stage_color(stage: &str) -> impl Fn() -> AdaptiveColor + Copy + 'static {
    token(stage_token(stage))
}

/// The resolved color of a token now (canvas paint).
pub fn paint(name: &'static str) -> Hsla {
    color(token(name)())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Ok,
    Warn,
    Crit,
    Info,
    /// Not enough evidence either way: its own colour, never a pass (U3).
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
    /// The colour of a judgement's level ([`crate::ui::model::verdicts`]: one scale for all).
    pub fn of_level(l: Level) -> Tone {
        match l {
            Level::Neutral => Tone::Neutral,
            Level::Ok => Tone::Ok,
            Level::Info => Tone::Info,
            Level::Inconclusive => Tone::Inconclusive,
            Level::Warn => Tone::Warn,
            Level::Crit => Tone::Crit,
        }
    }
    pub fn of_severity(s: Severity) -> Tone {
        Tone::of_level(verdicts::severity(s).1)
    }
    pub fn of_verdict(v: VerdictLevel) -> Tone {
        Tone::of_level(verdicts::run_level(v).1)
    }
}

fn radius() -> AbsoluteLength {
    px(theme().radius.0).into()
}

/// Whether the app runs in a terminal (character cells).
pub fn in_terminal() -> bool {
    nsh_base::terminal::is_terminal()
}

// ---------------------------------------------------------------------------------------
// Typography: the system font at native desktop sizes (13 pt body), tabular numerals. Sizes
// follow the app's Text Size setting (U15) on top of the system's text size, which the engine
// applies itself.
// ---------------------------------------------------------------------------------------

/// A text size of `pt` points times the app's text scale (reactive: changing the setting
/// resizes the text in place).
pub fn sz(pt: f32) -> impl Fn() -> Length + Copy + 'static {
    let scale = use_context::<crate::ui::store::Store>().map(|s| s.text_scale);
    move || px(pt * scale.map(|s| s.get()).unwrap_or(1.0)).into()
}

pub fn body(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_size(sz(13.0))
}

pub fn strong(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(sz(13.0))
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn title(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(sz(17.0))
        .font_weight(FontWeight::SEMIBOLD)
}

pub fn small(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(sz(11.0))
        .text_color(move || theme().muted_foreground)
}

/// A small caps-like section label.
pub fn caption(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(sz(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(move || theme().muted_foreground)
}

pub fn num(t: impl IntoReactive<SharedString>) -> Text {
    text(t).text_size(sz(13.0)).tabular_nums()
}

pub fn mono(t: impl IntoReactive<SharedString>) -> Text {
    text(t)
        .text_size(sz(12.0))
        .font_family(move || {
            theme()
                .font_mono
                .clone()
                .unwrap_or_else(|| "monospace".into())
        })
        .tabular_nums()
}

// ---------------------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------------------

/// The focus ring: an outline in a window; in a terminal (where outlines draw nothing) a
/// reversed band in the accent, so the keyboard focus is always visible.
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

/// The app's push button. `kind` may follow state (Run ⇄ Stop stays one element, so the
/// keyboard focus stays on it). Its accessible label is its label (a caller may give a
/// longer one with `.accessibility_label(…)`), so no button depends on a derived name.
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
        .gap_1()
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
                .text_size(sz(13.0))
                .font_weight(FontWeight::MEDIUM)
                .number_of_lines(1)
                .text_color(fg),
        )
}

/// A toolbar button: a glyph and a short label, with a tooltip naming its shortcut.
pub fn tool(
    glyph: &'static str,
    label: &'static str,
    hint: impl Into<SharedString>,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    tool_dyn(move || glyph, move || label, hint, on_press)
}

/// A toolbar button whose glyph and label follow state (Run ⇄ Stop).
pub fn tool_dyn(
    glyph: impl Fn() -> &'static str + Copy + 'static,
    label: impl Fn() -> &'static str + Copy + 'static,
    hint: impl Into<SharedString>,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let hint: SharedString = hint.into();
    let term = in_terminal();
    let inner = view()
        .flex_row()
        .gap(px(5.0))
        .items_center()
        .child((!term).then(|| {
            // Windows draws its own icon set (Segoe Fluent) where the glyph has one (U11).
            let native = move || {
                if os() == nsh_core::platform::Os::Windows {
                    native_icon(glyph())
                } else {
                    None
                }
            };
            text(move || match native() {
                Some(c) => SharedString::from(c.to_string()),
                None => SharedString::from(glyph()),
            })
            .text_size(sz(13.0))
            .font_family(move || {
                if native().is_some() {
                    crate::ui::model::icons::WINDOWS_ICON_FONT.into()
                } else {
                    theme()
                        .font_sans
                        .clone()
                        .unwrap_or_else(|| "system-ui".into())
                }
            })
            .text_color(move || theme().muted_foreground)
        }))
        .child(text(label).text_size(sz(12.0)).number_of_lines(1));
    let inner = if hint.is_empty() || in_terminal() {
        inner
    } else {
        inner.tooltip(tip(hint))
    };
    button()
        .on_press(on_press)
        .accessibility_label(move || Some(SharedString::from(label())))
        .h(px(28.0))
        .px(px(8.0))
        .items_center()
        .justify_center()
        .rounded(radius)
        .hover(|s| s.bg(move || theme().secondary))
        .active(|s| s.opacity(0.7))
        .selected_style(|s| s.bg(move || theme().accent))
        .focus_visible(ring)
        .disabled_style(|s| s.opacity(0.4))
        .child(inner)
}

/// The Segoe Fluent code point of a toolbar glyph, when the icon set has it.
fn native_icon(glyph: &str) -> Option<char> {
    use crate::ui::model::icons::{Icon, names};
    let icon = match glyph {
        "▶" => Icon::Run,
        "■" => Icon::Stop,
        "↻" => Icon::Rerun,
        "⇄" => Icon::Compare,
        "♥" => Icon::Health,
        "⚙" => Icon::Tune,
        "⌁" => Icon::Probe,
        "⇪" => Icon::Export,
        "≡" => Icon::Commands,
        "◧" => Icon::Sidebar,
        "◨" => Icon::Inspector,
        _ => return None,
    };
    Some(names(icon).1)
}

/// A tooltip bubble.
pub fn tip(label: impl Into<SharedString>) -> nsh_base::TooltipElement {
    let label: SharedString = label.into();
    tooltip().delay(500.0).child(
        view()
            .px_2()
            .py_1()
            .rounded(radius)
            .bg(move || theme().popover)
            .border_1()
            .border_color(move || theme().border)
            .shadow_md()
            .child(small(label).text_color(move || theme().popover_foreground)),
    )
}

/// A row that acts as a list item (sidebar rows, findings): pressable, keyboard focusable.
pub fn row_button(
    label: impl Into<SharedString>,
    selected: impl Fn() -> bool + Copy + 'static,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    button()
        .selected(selected)
        .on_press(on_press)
        .accessibility_label(label.into())
        .px(px(8.0))
        .py(px(4.0))
        .rounded(radius)
        .hover(|s| s.bg(move || theme().secondary))
        .bg(move || {
            if selected() {
                theme().sidebar_accent
            } else {
                transparent_black().into()
            }
        })
        .focus_visible(ring)
}

// ---------------------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------------------

/// A text field bound to a signal; `label` is its accessibility name and test id.
pub fn field(label: &'static str, placeholder: &'static str, value: Signal<String>) -> Input {
    input()
        .test_id(label)
        .label(label)
        .value(move || value.get())
        .on_change(move |t: &str| value.set(t.to_owned()))
        .placeholder(placeholder)
        .placeholder_color(|| color(theme().muted_foreground))
        .caret_color(|| color(theme().primary))
        .h(px(26.0))
        .px(px(8.0))
        .text_size(sz(13.0))
        .rounded(radius)
        .border_1()
        .border_color(move || theme().input)
        .bg(move || theme().background)
        .focus(|s| s.border_color(move || theme().ring))
}

/// A form row: the label in a fixed column at the left, the control filling the rest.
pub fn form_row(label: &'static str, control: impl IntoElement) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .min_w(px(0.0))
        .child(
            view()
                .w(px(110.0))
                .flex_shrink_0()
                .items_end()
                .child(body(label).text_color(move || theme().muted_foreground)),
        )
        .child(view().flex_1().min_w(px(0.0)).child(control))
}

/// A select over `(value, title)` pairs.
pub fn picker(
    label: &'static str,
    items: Vec<(SharedString, SharedString)>,
    value: Signal<Option<SharedString>>,
) -> impl IntoElement {
    let titles = items.clone();
    select()
        .items(
            items
                .into_iter()
                .map(|(v, t)| SelectItem::new(v, t))
                .collect::<Vec<_>>(),
        )
        .value(move || value.get())
        .on_change(move |next: &Option<SharedString>| value.set(next.clone()))
        .accessibility_label(label)
        .test_id(label)
        .offset(px(4.0))
        .popup_style(|s| {
            s.p_1()
                .rounded(radius)
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().popover)
                .shadow_lg()
        })
        .item_class("rounded-md focus:bg-accent selected:font-semibold")
        .render_item(|item: &SelectEntry| view().px_2().py_1().child(body(item.title.clone())))
        .trigger(
            view()
                .h(px(26.0))
                .px(px(8.0))
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .rounded(radius)
                .border_1()
                .border_color(move || theme().input)
                .bg(move || theme().background)
                .min_w(px(0.0))
                .child(
                    body(move || {
                        let v = value.get();
                        titles
                            .iter()
                            .find(|(k, _)| Some(k) == v.as_ref())
                            .map(|(_, t)| t.clone())
                            .unwrap_or_else(|| "Choose…".into())
                    })
                    .number_of_lines(1)
                    .flex_1()
                    .min_w(px(0.0)),
                )
                .child(small("⌄").flex_shrink_0()),
        )
}

/// An on/off switch with its label.
pub fn switch_row(label: &'static str, on: Signal<bool>) -> impl IntoElement {
    switch()
        .checked(on)
        .on_change(move |next: &bool| on.set(*next))
        .accessibility_label(label)
        .flex_row()
        .items_center()
        .gap_2()
        .track_style(|s| {
            s.w(px(30.0))
                .h(px(18.0))
                .p(px(2.0))
                .rounded_full()
                .bg(move || theme().input)
                .transition_colors()
                .checked(|s| s.bg(move || theme().primary))
        })
        .thumb_style(|s| {
            s.size(px(14.0))
                .rounded_full()
                .bg(move || theme().background)
                .shadow_sm()
                .transition_transform()
                .checked(|s| s.translate_x(px(12.0)))
        })
        .child(body(label))
}

/// A segmented control over `options`.
pub fn segmented(
    label: &'static str,
    options: &'static [(&'static str, &'static str)],
    value: Signal<&'static str>,
) -> impl IntoElement {
    let _ = label;
    toggle_group()
        .flex_row()
        .p(px(2.0))
        .gap(px(2.0))
        .rounded(radius)
        .bg(move || theme().muted)
        .children(options.iter().map(move |(key, title)| {
            let key = *key;
            toggle()
                .pressed(move || value.get() == key)
                .accessibility_label(*title)
                .on_change(move |_: &bool| value.set(key))
                .px(px(10.0))
                .h(px(22.0))
                .justify_center()
                .rounded(radius)
                .bg(move || {
                    if value.get() == key {
                        theme().background
                    } else {
                        transparent_black().into()
                    }
                })
                .focus_visible(ring)
                .child(text(*title).text_size(sz(12.0)))
        }))
}

// ---------------------------------------------------------------------------------------
// Containers and data display
// ---------------------------------------------------------------------------------------

/// A bordered surface.
pub fn panel() -> View {
    view()
        .rounded(move || px(theme().number("radius-card").unwrap_or(8.0)))
        .border_1()
        .border_color(move || theme().border)
        .bg(move || theme().card)
        .min_w(px(0.0))
}

/// A colored pill.
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
        .child(
            text(label)
                .text_size(sz(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || tone().color()),
        )
}

/// A dot of a stage's (or a series') color.
pub fn swatch(c: impl Fn() -> AdaptiveColor + 'static) -> View {
    view().w(px(8.0)).h(px(8.0)).rounded(px(2.0)).bg(c)
}

/// A key number. Fixed height and no wrapping: values arriving never move the layout.
/// Tiles share a row and flow onto a second one when it is too narrow ([`kpi_row`]).
pub fn kpi(
    label: &'static str,
    value: impl Fn() -> SharedString + 'static,
    tone: impl Fn() -> Tone + Copy + 'static,
) -> View {
    kpi_detail(label, value, SharedString::default, tone)
}

/// The row of key numbers: one row when it fits, two (or more) when it does not.
pub fn kpi_row() -> View {
    view()
        .flex_row()
        .flex_wrap()
        .rounded(px(6.0))
        .border_1()
        .border_color(move || theme().border)
        .overflow_hidden()
}

/// A key number with a small line under it (what it is made of, an assumption).
pub fn kpi_detail(
    label: &'static str,
    value: impl Fn() -> SharedString + 'static,
    detail: impl Fn() -> SharedString + 'static,
    tone: impl Fn() -> Tone + Copy + 'static,
) -> View {
    let term = in_terminal();
    view()
        .flex_1()
        .min_w(px(if term { 104.0 } else { 96.0 }))
        .h(px(if term { 48.0 } else { 58.0 }))
        .justify_center()
        .gap(px(2.0))
        .px(px(10.0))
        .child(caption(label).number_of_lines(1))
        .child(
            text(value)
                .text_size(sz(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .tabular_nums()
                .number_of_lines(1)
                .text_color(move || match tone() {
                    Tone::Neutral => theme().foreground,
                    t => t.color(),
                }),
        )
        .child(small(detail).number_of_lines(1).tabular_nums())
}

/// A centered message for an empty pane.
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
        .child(small(detail).text_center().max_w(px(420.0)))
}

/// A key and what it does, for the footer and the help overlay.
/// Never wraps: a footer too narrow for all of them clips the last ones.
pub fn key_hint(keys: &'static str, what: &'static str) -> View {
    view()
        .flex_row()
        .gap_1()
        .flex_shrink_0()
        .child(
            text(keys)
                .text_size(sz(11.0))
                .font_weight(FontWeight::BOLD)
                .number_of_lines(1)
                .flex_shrink_0(),
        )
        .child(small(what).number_of_lines(1).flex_shrink_0())
}

/// A two-column property list row (inspector).
pub fn prop_row(
    name: impl IntoReactive<SharedString>,
    value: impl IntoReactive<SharedString>,
) -> View {
    view()
        .flex_row()
        .gap_2()
        .min_w(px(0.0))
        .child(
            view()
                .w(px(96.0))
                .flex_shrink_0()
                .child(small(name).number_of_lines(1)),
        )
        .child(
            text(value)
                .text_size(sz(12.0))
                .tabular_nums()
                .number_of_lines(1)
                .flex_1()
                .min_w(px(0.0))
                .selectable(true),
        )
}
