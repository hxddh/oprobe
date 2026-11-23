//! The sidebar (design app-native "Structure"): the runs, newest first, grouped under their
//! operator, on the window's sidebar material. A press selects a run; with the platform's
//! modifier it adds or removes one, with Shift it extends the selection; a secondary press
//! opens the run's context menu. A running job is the first row, with its progress.

use nsh_base::theme;
use nsh_core::element::os;
use nsh_core::platform::Os;
use nsh_core::prelude::*;

use super::shell::{SIDEBAR_W, bar_h, fill};
use crate::ui::data::RunRow;
use crate::ui::fmt;
use crate::ui::i18n::{tr, tr_dyn};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Kind, Tone, btn, caption, dot, small, tool_btn};
use crate::ui::menus;
use crate::ui::store::{Dialog, Select, Store};

pub fn sidebar(s: Store) -> View {
    let term = kit::in_terminal();
    let material = !term && matches!(os(), Os::MacOs | Os::Windows);
    view()
        .w(px(if term { 30.0 * 8.0 } else { SIDEBAR_W }))
        .flex_shrink_0()
        .min_h(px(0.0))
        .border_r_1()
        .border_color(move || theme().border)
        // On the material where the platform has one, else the theme's sidebar colour.
        .bg(move || {
            if material {
                transparent_black().into()
            } else {
                theme().sidebar
            }
        })
        .test_id("sidebar")
        .child(top(s))
        .child(
            scroll().flex_1().min_h(px(0.0)).child(
                view()
                    .px(px(if term { 0.0 } else { 8.0 }))
                    .pb_3()
                    .child(live(s))
                    .child(list(s)),
            ),
        )
}

/// The sidebar's top: room for the window's controls, the sidebar toggle at its end.
fn top(s: Store) -> View {
    view()
        .flex_row()
        .items_center()
        .h(move || px(bar_h()))
        .flex_shrink_0()
        .pl(move || px(nsh_core::element::desktop::title_bar().left.0.max(8.0)))
        .pr(px(8.0))
        .window_drag(true)
        .child(view().flex_1())
        .child(
            tool_btn(Icon::Sidebar, tr("menu.hide_sidebar"), move || {
                s.sidebar.set(false)
            })
            .test_id("sidebar-hide"),
        )
}

/// The running job (a run being measured), with its progress.
fn live(s: Store) -> impl IntoElement {
    dynamic(move || {
        let st = s.run_state.get().filter(|st| st.outcome.is_none());
        let operator = s.form.operator.get();
        untracked(|| {
            st.map(|st| {
                let frac = if st.total_s > 0.0 {
                    (st.elapsed_s / st.total_s).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let phase = if st.cleaning {
                    "phase.cleanup".to_owned()
                } else {
                    format!(
                        "phase.{}",
                        st.phase.map(|p| p.label()).unwrap_or("preparing dataset")
                    )
                };
                view()
                    .test_id("live-run")
                    .child(group_header(tr("side.running")))
                    .child(
                        view()
                            .gap(px(4.0))
                            .px(px(8.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(move || theme().accent)
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(dot(|| Tone::Ok))
                                    .child(
                                        text(operator.clone())
                                            .text_size(px(kit::metrics().body))
                                            .number_of_lines(1)
                                            .flex_1()
                                            .min_w(px(0.0)),
                                    ),
                            )
                            .child(
                                small(format!(
                                    "{} · {}/s · p99 {}",
                                    tr_dyn(&phase).unwrap_or(tr("phase.measuring")),
                                    fmt::compact(st.ops_per_s),
                                    fmt::dur_ms(st.p99_ms)
                                ))
                                .tabular_nums()
                                .number_of_lines(1),
                            )
                            .child(kit::meter(frac, "series-a").h(px(3.0)))
                            .child((!kit::in_terminal()).then(|| {
                                crate::ui::chart::sparkline(22.0, move || {
                                    s.run_state.with(|st| {
                                        st.as_ref().map(|s| s.spark.clone()).unwrap_or_default()
                                    })
                                })
                            })),
                    )
            })
        })
    })
}

/// The runs grouped under their operator, newest group first.
fn list(s: Store) -> impl IntoElement {
    dynamic(move || {
        let runs = s.runs.get();
        untracked(|| {
            if runs.is_empty() {
                return empty_state(s).into_element();
            }
            let mut groups: Vec<(String, Vec<RunRow>)> = Vec::new();
            for r in runs {
                match groups.iter_mut().find(|g| g.0 == r.operator) {
                    Some(g) => g.1.push(r),
                    None => groups.push((r.operator.clone(), vec![r])),
                }
            }
            view()
                .children(groups.into_iter().map(|(op, rows)| {
                    view()
                        .child(group_header(op))
                        .children(rows.into_iter().map(|r| row(s, r)))
                }))
                .into_element()
        })
    })
}

fn group_header(label: impl Into<SharedString>) -> View {
    view()
        .px(px(8.0))
        .pt(px(if kit::in_terminal() { 16.0 } else { 14.0 }))
        .pb(px(4.0))
        .child(caption(label.into()).number_of_lines(1))
}

/// One run: its verdict, short id and preset, when it ran, its goodput and p99.
fn row(s: Store, r: RunRow) -> View {
    let id = r.id.clone();
    let (sel_id, ctx_id, a11y) = (id.clone(), id.clone(), fill("runs.open", &[&id]));
    let selected = computed(move || s.selection.with(|v| v.contains(&sel_id)));
    let selected = move || selected.get();
    let tone = Tone::of_verdict(r.verdict);
    let short = r.id.rsplit('-').next().unwrap_or(&r.id).to_owned();
    let term = kit::in_terminal();
    view()
        .role(AccessibilityRole::Button)
        .label(a11y)
        .focusable(true)
        .accessibility_state(move || AccessibilityState {
            selected: selected(),
            ..Default::default()
        })
        .test_id(&format!("run-{}", r.id))
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(if term { 0.0 } else { 5.0 }))
        .rounded(px(6.0))
        .bg(move || {
            if selected() {
                theme().primary
            } else {
                transparent_black().into()
            }
        })
        .text_color(move || {
            if selected() {
                theme().primary_foreground
            } else {
                theme().foreground
            }
        })
        .hover(move |st| {
            if selected() {
                st
            } else {
                st.bg(move || theme().secondary)
            }
        })
        .focus_visible(kit::ring)
        .on_press(move |e: &PressEvent| {
            let m = e.modifiers;
            let toggle = if menus::mac() { m.cmd } else { m.ctrl };
            let how = if m.shift {
                Select::Extend
            } else if toggle {
                Select::Toggle
            } else {
                Select::Only
            };
            s.select(&id, how);
        })
        .on_context_menu(move |_: &ContextMenuEvent| menus::run_context(s, &ctx_id))
        .child(dot(move || tone))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    view()
                        .flex_row()
                        .gap(px(6.0))
                        .child(
                            text(short)
                                .text_size(px(kit::metrics().body))
                                .font_weight(FontWeight::MEDIUM)
                                .tabular_nums()
                                .number_of_lines(1),
                        )
                        .child(
                            text(r.preset.clone())
                                .text_size(px(kit::metrics().body))
                                .number_of_lines(1)
                                .opacity(0.7)
                                .flex_1()
                                .min_w(px(0.0)),
                        ),
                )
                .child((!term).then(|| {
                    text(format!(
                        "{} · {}/s · {}{}",
                        fmt::short_time(&r.started_at),
                        fmt::compact(r.ops_per_s),
                        fmt::dur_ms(r.p99_ms),
                        if r.partial { " · *" } else { "" }
                    ))
                    .text_size(px(kit::metrics().small))
                    .tabular_nums()
                    .number_of_lines(1)
                    .opacity(0.7)
                })),
        )
}

/// No runs yet: what to do first.
fn empty_state(s: Store) -> View {
    view()
        .items_center()
        .gap_2()
        .px_3()
        .py(px(32.0))
        .child(
            text(tr("runs.empty"))
                .text_size(px(kit::metrics().body))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || theme().muted_foreground),
        )
        .child(small(tr("runs.empty_hint")).text_center())
        .child(btn(
            tr("menu.new_run"),
            || Kind::Primary,
            move || s.dialog.set(Some(Dialog::NewRun)),
        ))
}
