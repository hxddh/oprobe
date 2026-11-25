//! The sidebar (design app-native "Structure"): the runs, newest first, grouped under their
//! operator, on the window's sidebar material where the platform has one. The runs are one
//! list for the keyboard and for screen readers (a listbox of options, one stop of Tab): ↑/↓
//! move the selection, Shift extends it, Home/End go to the ends, typing a run's id, operator
//! or workload finds it, Delete (⌘⌫ on macOS) asks to delete, Return opens. A press selects a
//! run; with the platform's modifier it adds or removes one, with Shift it extends the
//! selection; a secondary press opens the run's context menu and marks the row it acts on.
//! Its width follows a drag of its edge and is remembered. A running job is the first row,
//! with its progress.

use nsh_base::{HandleEdge, resize_handle, theme};
use nsh_core::element::desktop::main_window;
use nsh_core::element::os;
use nsh_core::keymap::When;
use nsh_core::platform::Os;
use nsh_core::prelude::*;

use super::shell::{bar_h, fill};
use crate::ui::data::RunRow;
use crate::ui::fmt;
use crate::ui::i18n::{tr, tr_dyn};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Kind, Tone, btn, caption, dot, small, tool_btn};
use crate::ui::menus;
use crate::ui::store::{Dialog, Move, SIDEBAR_MAX, SIDEBAR_MIN, Select, Store};

/// Whether the sidebar is drawn on the window's material (macOS's vibrancy, Windows 11's
/// Mica); elsewhere it paints the theme's sidebar colour.
pub fn on_material() -> bool {
    !kit::in_terminal()
        && match os() {
            Os::MacOs => true,
            Os::Windows => kit::mica(),
            _ => false,
        }
}

pub fn sidebar(s: Store) -> View {
    let term = kit::in_terminal();
    let material = on_material();
    let list_ref = element_ref();
    let focused = signal(false);
    keys(s, focused);
    view()
        .w(move || {
            px(if term {
                (s.sidebar_w.get() / 8.0).round().clamp(22.0, 48.0) * 8.0
            } else {
                s.sidebar_w.get()
            })
        })
        .flex_shrink_0()
        .min_h(px(0.0))
        .border_r_1()
        .border_color(move || theme().border)
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
                    .child(
                        view()
                            .role(AccessibilityRole::Listbox)
                            .label(tr("side.runs"))
                            .focusable(true)
                            .element_ref(&list_ref)
                            .test_id("sidebar-list")
                            .on_focus(move || focused.set(true))
                            .on_blur(move || focused.set(false))
                            .child(list(s, list_ref, focused)),
                    ),
            ),
        )
        .child(edge(s))
}

/// The keys of the list while it has the keyboard focus (never while a field edits).
fn keys(s: Store, focused: Signal<bool>) {
    effect(move || {
        if !focused.get() {
            return;
        }
        let bind = |k: &str, f: Box<dyn Fn()>| {
            on_key_with(
                k,
                KeyOptions {
                    when: When::NotEditing,
                    repeat: true,
                    for_focus: true,
                    ..Default::default()
                },
                move |_| f(),
            );
        };
        bind("up", Box::new(move || s.move_cursor(Move::Prev, false)));
        bind("down", Box::new(move || s.move_cursor(Move::Next, false)));
        bind(
            "shift+up",
            Box::new(move || s.move_cursor(Move::Prev, true)),
        );
        bind(
            "shift+down",
            Box::new(move || s.move_cursor(Move::Next, true)),
        );
        bind("home", Box::new(move || s.move_cursor(Move::First, false)));
        bind("end", Box::new(move || s.move_cursor(Move::Last, false)));
        bind(
            "shift+home",
            Box::new(move || s.move_cursor(Move::First, true)),
        );
        bind(
            "shift+end",
            Box::new(move || s.move_cursor(Move::Last, true)),
        );
        for k in ["enter", "space"] {
            bind(
                k,
                Box::new(move || s.show_selection(s.selection.get_untracked())),
            );
        }
        bind(
            if menus::mac() {
                "mod+backspace"
            } else {
                "delete"
            },
            Box::new(move || s.ask_delete(s.selection.get_untracked())),
        );
        for c in ('a'..='z').chain('0'..='9') {
            bind(&c.to_string(), Box::new(move || s.type_select(c)));
        }
    });
}

/// The sidebar's trailing edge: dragged, it resizes the sidebar (the width is saved when the
/// drag ends).
fn edge(s: Store) -> impl IntoElement {
    let from = stored(0.0_f32);
    resize_handle()
        .edge(HandleEdge::Trailing)
        .accessibility_label(tr("side.resize"))
        .on_drag_start(move || {
            from.replace(s.sidebar_w.get_untracked());
        })
        .on_drag_move(move |o: &Points| {
            let w = (from.with(|f| *f) + o.0).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
            s.sidebar_w.set(w);
        })
        .on_drag_end(move |o: &Points| {
            let w = (from.with(|f| *f) + o.0).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
            s.sidebar_w.set(w);
            s.persist();
        })
        .child(view().w(px(1.0)).h_full())
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
        let operator = s.running_label.get();
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
fn list(s: Store, list_ref: ElementRef, focused: Signal<bool>) -> impl IntoElement {
    dynamic(move || {
        let runs = s.runs.get();
        untracked(|| {
            if runs.is_empty() {
                return empty_state(s).into_element();
            }
            let mut groups: Vec<(String, Vec<RunRow>)> = Vec::new();
            for r in crate::ui::data::sidebar_order(&runs) {
                match groups.iter_mut().find(|g| g.0 == r.operator) {
                    Some(g) => g.1.push(r),
                    None => groups.push((r.operator.clone(), vec![r])),
                }
            }
            let total = runs.len();
            let mut n = 0;
            view()
                .children(groups.into_iter().map(|(op, rows)| {
                    view().child(group_header(op)).children(
                        rows.into_iter()
                            .map(|r| {
                                n += 1;
                                row(s, r, n, total, list_ref, focused)
                            })
                            .collect::<Vec<_>>(),
                    )
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

/// How a selected row looks on this platform: macOS fills it with the accent while the list
/// has the focus in the key window, and with grey otherwise; Windows and GNOME use a subtle
/// fill and an accent pill at the leading edge; a terminal reverses it.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    Plain,
    Accent,
    Inactive,
    Subtle,
}

/// One run: its verdict, short id and preset, when it ran, its goodput and p99.
fn row(
    s: Store,
    r: RunRow,
    n: usize,
    total: usize,
    list_ref: ElementRef,
    focused: Signal<bool>,
) -> View {
    let id = r.id.clone();
    let (sel_id, ctx_id, menu_id, cur_id) = (id.clone(), id.clone(), id.clone(), id.clone());
    let row_ref = element_ref();
    effect(move || {
        if s.cursor.get().as_deref() == Some(cur_id.as_str()) {
            row_ref.reveal(nsh_core::RevealOptions::default());
        }
    });
    let a11y = format!("{} ({n}/{total})", fill("runs.open", &[&id]));
    let selected = computed(move || s.selection.with(|v| v.contains(&sel_id)));
    let marked = computed(move || {
        s.context_row
            .with(|c| c.as_deref() == Some(ctx_id.as_str()))
    });
    let term = kit::in_terminal();
    let mac = menus::mac();
    let look = move || {
        if !selected.get() {
            Look::Plain
        } else if term {
            Look::Accent
        } else if mac {
            let key = main_window().state().is_none_or(|w| w.focused);
            if key && focused.get() {
                Look::Accent
            } else {
                Look::Inactive
            }
        } else {
            Look::Subtle
        }
    };
    let tone = Tone::of_verdict(r.verdict);
    let short = r.id.rsplit('-').next().unwrap_or(&r.id).to_owned();
    view()
        .role(AccessibilityRole::Option)
        .label(a11y)
        .accessibility_state(move || AccessibilityState {
            selected: selected.get(),
            ..Default::default()
        })
        .test_id(&format!("run-{}", r.id))
        .element_ref(&row_ref)
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(if term { 0.0 } else { 5.0 }))
        .rounded(px(6.0))
        .bg(move || match look() {
            Look::Accent => theme().primary,
            Look::Inactive => kit::token("selection-inactive")(),
            Look::Subtle => kit::token("selection")(),
            Look::Plain => transparent_black().into(),
        })
        .text_color(move || match look() {
            Look::Accent => theme().primary_foreground,
            _ => theme().foreground,
        })
        .outline(px(if term { 0.0 } else { 2.0 }))
        .outline_color(move || {
            if marked.get() {
                theme().primary
            } else {
                transparent_black().into()
            }
        })
        .hover(move |st| {
            st.bg(move || match look() {
                Look::Accent => theme().primary,
                Look::Inactive => kit::token("selection-inactive")(),
                Look::Subtle => kit::token("selection")(),
                Look::Plain => theme().secondary,
            })
        })
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
            list_ref.focus();
            s.select(&id, how);
        })
        .on_context_menu(move |_: &ContextMenuEvent| menus::run_context(s, &menu_id))
        // Windows' and GNOME's selection: an accent pill at the leading edge.
        .child((!term && !mac).then(|| {
            view()
                .w(px(3.0))
                .h(px(16.0))
                .ml(px(-5.0))
                .rounded_full()
                .bg(move || {
                    if look() == Look::Subtle {
                        theme().primary
                    } else {
                        transparent_black().into()
                    }
                })
        }))
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
            tr("runs.profile_lab"),
            || Kind::Primary,
            move || s.dialog.set(Some(Dialog::NewRun)),
        ))
}
