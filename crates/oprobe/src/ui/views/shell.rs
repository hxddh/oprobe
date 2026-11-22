//! The window: toolbar, three panes (sidebar · run inspector · inspector panel), status bar,
//! the overlays (New Run sheet, command palette, help, settings), the application menu and the
//! keyboard. One list of commands feeds the menu, the palette and the help.

use std::rc::Rc;

use nsh_base::{
    CommandItem, SheetEvent, alert_dialog, command, dialog, h_resizable, resizable_panel, sheet,
    tab, tabs, theme, toolbar, toolbar_group, toolbar_separator,
};
use nsh_core::element::desktop::{MenuItem, popup_menu};
use nsh_core::prelude::*;

use crate::store::RunSummary;
use crate::ui::kit::{
    self, Kind, Tone, badge, body, btn, caption, field, form_row, key_hint, picker, row_button,
    small, strong, switch_row, tool,
};
use crate::ui::model::commands::{self, COMMANDS, CmdCtx};
use crate::ui::model::i18n::{cmd_label, tr};
use crate::ui::model::{Tab, fmt, runs};
use crate::ui::store::{Inspect, JobKind, Mode, Overlay, Phase, Sel, Stage, Store, store};
use crate::ui::views::{
    compare, health, inspector, overview, requests, tabs as tabviews, timeline, tools, welcome,
};

// ---------------------------------------------------------------------------------------
// Commands: one list for the menu, the palette and the help (`model::commands`)
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct Cmd {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub keys: Option<&'static str>,
    /// Whether it can run now: the menu and the palette show the same state.
    pub enabled: bool,
    /// Why it cannot run now (U6).
    pub reason: Option<&'static str>,
    pub run: Rc<dyn Fn()>,
}

/// Start a run, or stop the one running: one control.
pub fn run_or_stop(s: Store) {
    if s.busy() {
        s.stop();
    } else {
        s.open_new_run();
    }
}

/// What the commands' enabled states depend on, now (tracked: menus follow it).
pub fn cmd_ctx(s: Store) -> CmdCtx {
    CmdCtx {
        busy: s.busy(),
        has_run: s.current.with(|c| c.is_some()) && s.sel.get() != Sel::Live,
        has_verify: s.current.with(|c| {
            c.as_ref().is_some_and(|c| {
                c.run
                    .report
                    .findings
                    .iter()
                    .any(|f| f.verification.is_some())
            })
        }),
        runs: s.runs.with(|r| r.len()),
        targets: s.targets.with(|t| t.len()),
    }
}

/// The action of command `id`.
fn action(s: Store, id: &'static str) -> Rc<dyn Fn()> {
    let f: Rc<dyn Fn()> = match id {
        "new" | "run" => Rc::new(move || s.open_new_run()),
        "open-run" => Rc::new(move || s.open(Overlay::PickRun)),
        "open" => Rc::new(move || crate::ui::files::open(s)),
        "add-target" => Rc::new(move || s.open_add_target(None)),
        "switch-target" => Rc::new(move || s.open(Overlay::PickTarget)),
        "export" => Rc::new(move || crate::ui::files::export_run(s, "oprobe")),
        "export-json" => Rc::new(move || crate::ui::files::export_run(s, "json")),
        "export-md" => Rc::new(move || crate::ui::files::export_run(s, "md")),
        "export-csv" => Rc::new(move || requests::export_csv(s)),
        "settings" => Rc::new(move || open_settings(s)),
        "stop" => Rc::new(move || s.stop()),
        "rerun" => Rc::new(move || s.rerun()),
        "edit-rerun" => Rc::new(move || s.edit_rerun()),
        "verify" => Rc::new(move || s.verify_top()),
        "fix" => Rc::new(move || s.fix()),
        "demo" => Rc::new(move || s.demo()),
        "compare" => Rc::new(move || s.compare_default()),
        "tune" => Rc::new(move || s.mode.set(Mode::Tune)),
        "probe" => Rc::new(move || s.mode.set(Mode::Probe)),
        "slowest" => Rc::new(move || s.explain_slowest()),
        "health" => Rc::new(move || s.show_health(None)),
        "waterfall" => Rc::new(move || s.waterfall_slowest()),
        "copy-link" => Rc::new(move || s.copy_link()),
        "sidebar" => Rc::new(move || s.toggle_sidebar()),
        "inspector" => Rc::new(move || s.toggle_inspector()),
        "log" => Rc::new(move || s.set_log_latency(!s.log_latency.get_untracked())),
        "palette" => Rc::new(move || s.open(Overlay::Palette)),
        "help" => Rc::new(move || s.open(Overlay::Help)),
        "docs" => Rc::new(|| {
            let _ = open_url("https://github.com/hxddh/oprobe/tree/main/docs");
        }),
        tab if tab.starts_with("tab-") => {
            let i: usize = tab[4..].parse().unwrap_or(0);
            let t = Tab::ALL[i.min(Tab::ALL.len() - 1)];
            Rc::new(move || s.set_tab(t))
        }
        _ => Rc::new(|| {}),
    };
    // Under the root's owner: a task the command starts (a save dialog, a load) is not ended
    // when the palette or menu it was chosen from closes.
    let root = s.root;
    Rc::new(move || root.run(|| f()))
}

/// Every command with its state now. Reading it tracks what the states depend on.
pub fn commands(s: Store) -> Vec<Cmd> {
    let x = cmd_ctx(s);
    let mac = os() == nsh_core::platform::Os::MacOs;
    COMMANDS
        .iter()
        .map(|c| Cmd {
            id: c.id,
            label: cmd_label(c.id, c.label),
            group: if c.id == "settings" && mac {
                "App"
            } else {
                c.group
            },
            keys: c.keys,
            enabled: commands::enabled(c.id, &x),
            reason: commands::disabled_reason(c.id, &x),
            run: action(s, c.id),
        })
        .collect()
}

/// Run command `id` if it is enabled now.
pub fn run_command(s: Store, id: &str) {
    if let Some(c) = untracked(|| commands(s)).into_iter().find(|c| c.id == id)
        && c.enabled
    {
        (c.run)();
    }
}

fn menu_items(s: Store, group: &'static str) -> Vec<MenuItem> {
    commands(s)
        .into_iter()
        .filter(|c| c.group == group)
        .map(|c| {
            let run = c.run.clone();
            let mut m = MenuItem::new(c.label)
                .disabled(!c.enabled)
                .on_select(move || run());
            if let Some(k) = c.keys {
                m = m.shortcut(k);
            }
            if c.id == "log" {
                m = m.checked(s.log_latency.get());
            }
            m
        })
        .collect()
}

/// The application menu (macOS menu bar; the window's menu bar on Windows and Linux; the top
/// row in a terminal). It follows the app's state.
fn install_menu(s: Store) {
    use nsh_core::desktop::MenuRole;
    effect(move || {
        let mut file = menu_items(s, "File");
        file.push(MenuItem::separator());
        file.push(MenuItem::role(MenuRole::Close));
        if os() != nsh_core::platform::Os::MacOs {
            file.push(MenuItem::role(MenuRole::Quit));
        }
        let mut items = Vec::new();
        if os() == nsh_core::platform::Os::MacOs {
            items.push(MenuItem::submenu(
                "oprobe",
                [
                    MenuItem::role(MenuRole::About),
                    MenuItem::separator(),
                    MenuItem::new("Settings…")
                        .shortcut("mod+,")
                        .on_select(move || open_settings(s)),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Hide),
                    MenuItem::role(MenuRole::HideOthers),
                    MenuItem::role(MenuRole::ShowAll),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Quit),
                ],
            ));
        }
        items.push(MenuItem::submenu("File", file));
        items.push(MenuItem::submenu(
            "Edit",
            [
                MenuItem::role(MenuRole::Undo),
                MenuItem::role(MenuRole::Redo),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Cut),
                MenuItem::role(MenuRole::Copy),
                MenuItem::role(MenuRole::Paste),
                MenuItem::role(MenuRole::SelectAll),
            ],
        ));
        items.push(MenuItem::submenu("Run", menu_items(s, "Run")));
        let mut viewm = menu_items(s, "View");
        viewm.push(MenuItem::separator());
        viewm.push(MenuItem::role(MenuRole::ToggleFullScreen));
        items.push(MenuItem::submenu("View", viewm));
        items.push(MenuItem::submenu(
            "Window",
            [
                MenuItem::role(MenuRole::Minimize),
                MenuItem::role(MenuRole::Zoom),
            ],
        ));
        items.push(MenuItem::submenu("Help", menu_items(s, "Help")));
        nsh_core::element::desktop::set_application_menu(items);
    });
}

/// A single-letter shortcut: only while no text field has the keyboard focus, so typing
/// `api:GetObject` in a filter never switches to Compare on `c` (UI-15).
pub fn letter_key(key: &str, f: impl Fn() + 'static) {
    on_key_with(
        key,
        KeyOptions {
            when: nsh_core::keymap::When::NotEditing,
            ..Default::default()
        },
        move |_| f(),
    );
}

/// Move the keyboard selection of the shown list by `d` rows (`i64::MIN` / `i64::MAX`: the
/// first / last): the Requests table's focused row, the Timeline's inspected operation (in the
/// order the Timeline shows), else the selected run of the history (U7).
pub fn step(s: Store, d: i64) {
    let to = |cur: i64, n: i64| -> i64 {
        match d {
            i64::MIN => 0,
            i64::MAX => n - 1,
            _ => (cur + d).clamp(0, n - 1),
        }
    };
    if s.mode.get_untracked() == Mode::Run && s.sel.get_untracked() != Sel::Live {
        match s.tab.get_untracked() {
            Tab::Requests => {
                let n = s.req_rows.with_untracked(|r| r.0.len()) as i64;
                if n > 0 {
                    let cur = s.req_focus.get_untracked().map(|c| c as i64).unwrap_or(-1);
                    s.req_focus.set(Some(to(cur, n) as u32));
                }
                return;
            }
            Tab::Timeline => {
                let order = s.tl_order.get_untracked();
                let n = order.0.len() as i64;
                if n > 0 {
                    let cur = match s.inspect.get_untracked() {
                        Inspect::Op(i) => order.0.iter().position(|o| *o == i).map(|p| p as i64),
                        _ => None,
                    }
                    .unwrap_or(-1);
                    s.inspect.set(Inspect::Op(order.0[to(cur, n) as usize]));
                }
                return;
            }
            _ => {}
        }
    }
    let runs = s.runs.get_untracked();
    if runs.is_empty() {
        return;
    }
    let cur = match s.sel.get_untracked() {
        Sel::Run(id) => runs
            .iter()
            .position(|r| r.id == id)
            .map(|p| p as i64)
            .unwrap_or(-1),
        _ => -1,
    };
    if let Some(r) = runs.get(to(cur, runs.len() as i64) as usize) {
        s.select_run(&r.id.clone());
    }
}

/// Rows a page key moves by.
const PAGE_ROWS: i64 = 20;

/// Sort the Requests table from the keyboard (U7): `>` sorts by the next column (descending
/// first, the slowest on top), `<` reverses the order.
pub fn sort_key(s: Store, next: bool) {
    use crate::ui::model::requests::Col;
    s.req_sort.update(|so| {
        *so = match (*so, next) {
            (None, true) => Some((Col::Total, true)),
            (Some((c, _)), true) => {
                let i = Col::ALL.iter().position(|x| *x == c).unwrap_or(0);
                Some((Col::ALL[(i + 1) % Col::ALL.len()], true))
            }
            (Some((c, d)), false) => Some((c, !d)),
            (None, false) => Some((Col::Total, false)),
        }
    });
}

/// Keys of both hosts that act on what is shown (outside text fields): page and jump keys,
/// sort keys, the heatmap brush.
fn install_navigation_keys(s: Store) {
    let repeat = KeyOptions {
        when: nsh_core::keymap::When::NotEditing,
        repeat: true,
        ..Default::default()
    };
    on_key_with("pagedown", repeat, move |_| step(s, PAGE_ROWS));
    on_key_with("pageup", repeat, move |_| step(s, -PAGE_ROWS));
    on_key_with("home", repeat, move |_| step(s, i64::MIN));
    on_key_with("end", repeat, move |_| step(s, i64::MAX));
    letter_key("shift+.", move || sort_key(s, true));
    letter_key("shift+,", move || sort_key(s, false));
    for (k, c) in [("[", '['), ("]", ']'), ("shift+[", '{'), ("shift+]", '}')] {
        letter_key(k, move || {
            if matches!(s.mode.get_untracked(), Mode::Run)
                && s.current.with_untracked(|c| c.is_some())
            {
                s.nudge_brush(c);
            }
        });
    }
}

/// Keys of the terminal (keyboard-first): r run again, s stop, n new, 1–6 tabs, / filter,
/// j/k move, g/G first/last, enter inspect, c compare, v verify, o open run, h health,
/// w waterfall, : commands, ? help, q quit (asks while a run is in progress). Letters fire only
/// outside text fields.
fn install_terminal_keys(s: Store, filter: ElementRef) {
    letter_key("r", move || run_command(s, "rerun"));
    letter_key("s", move || s.stop());
    letter_key("n", move || run_command(s, "new"));
    letter_key("v", move || run_command(s, "verify"));
    letter_key("o", move || run_command(s, "open-run"));
    letter_key("t", move || run_command(s, "add-target"));
    letter_key("h", move || run_command(s, "health"));
    letter_key("w", move || run_command(s, "waterfall"));
    letter_key("shift+;", move || s.open(Overlay::Palette));
    for (i, t) in Tab::ALL.into_iter().enumerate() {
        letter_key(["1", "2", "3", "4", "5", "6"][i], move || s.set_tab(t));
    }
    letter_key("/", move || {
        s.set_tab(Tab::Requests);
        // The Requests tab mounts in the next frame: focus its filter once it is there.
        spawn(async move {
            next_frame().await;
            next_frame().await;
            filter.focus();
        });
    });
    letter_key("j", move || step(s, 1));
    letter_key("k", move || step(s, -1));
    letter_key("g", move || step(s, i64::MIN));
    letter_key("shift+g", move || step(s, i64::MAX));
    // Enter inspects the focused request; it passes through, so a focused button still takes
    // it (U5).
    on_key_with(
        "enter",
        KeyOptions {
            when: nsh_core::keymap::When::NotEditing,
            pass_through: true,
            ..Default::default()
        },
        move |_| {
            if s.overlay.get_untracked().is_none()
                && s.tab.get_untracked() == Tab::Requests
                && s.req_focus.get_untracked().is_some()
                && (!s.prefs.with_untracked(|p| p.inspector_visible) || side_panel_hidden())
            {
                s.open(Overlay::Inspector);
            }
        },
    );
    letter_key("c", move || run_command(s, "compare"));
    letter_key("shift+/", move || s.open(Overlay::Help));
    letter_key("q", move || confirm_quit(s));
}

/// Esc closes the overlay and gives the keyboard focus back to the main area (U5: not to the
/// filter field), else leaves the filter field (so the letters act again).
fn install_escape(s: Store, filter: ElementRef) {
    let always = KeyOptions {
        when: nsh_core::keymap::When::Always,
        ..Default::default()
    };
    on_key_with("escape", always, move |_| {
        if s.overlay.get_untracked().is_some() {
            s.overlay.set(None);
            focus_main(s);
        } else {
            filter.blur();
        }
    });
}

/// Give the keyboard focus to the main area (after an overlay closes).
pub fn focus_main(s: Store) {
    spawn(async move {
        next_frame().await;
        s.main_ref.focus();
    });
}

/// Quit, asking first while a job runs: it is stopped and its test data cleaned up.
pub fn confirm_quit(s: Store) {
    if !s.busy() {
        s.request_quit();
        return;
    }
    spawn(async move {
        let answer = alert_dialog()
            .title("Stop the run and quit?")
            .description("The run is stopped and its test data removed before oprobe quits.")
            .cancel("cancel", "Keep Running")
            .destructive("quit", "Stop & Quit")
            .show()
            .await;
        if matches!(answer, Ok(r) if r.action.as_deref() == Some("quit")) {
            s.request_quit();
        }
    });
}

/// The terminal at 80×24 (and narrow windows): one pane at a time.
fn narrow() -> bool {
    window().width.0 < 900.0
}

/// Below this width (120 columns in a terminal) the inspector panel opens over the content
/// (Enter) instead of taking a column: the run's tabs keep their room.
fn side_panel_hidden() -> bool {
    window().width.0 < 1200.0
}

// ---------------------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------------------

fn toolbar_view(s: Store) -> impl IntoElement {
    let busy = move || s.busy();
    let inset = move || nsh_core::element::desktop::title_bar().left.0;
    let term = kit::in_terminal();
    view().window_drag(true).child(
        toolbar()
            .accessibility_label("Toolbar")
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(if kit::in_terminal() { 16.0 } else { 44.0 }))
            .pl(move || px(inset().max(8.0)))
            .pr_2()
            .bg(move || theme().color("toolbar").unwrap_or(theme().background))
            .border_b_1()
            .border_color(move || theme().border)
            .child(
                toolbar_group()
                    .accessibility_label("Run")
                    .flex_row()
                    .gap_1()
                    // One control for Run and Stop: the focus stays on it.
                    .child(
                        kit::tool_dyn(
                            move || if busy() { "■" } else { "▶" },
                            move || if busy() { "Stop" } else { "Run" },
                            format!("Run {} · Stop {}", key_label("mod+n"), key_label("mod+.")),
                            move || run_or_stop(s),
                        )
                        .test_id("run-stop"),
                    )
                    .child(
                        tool(
                            "↻",
                            "Re-run",
                            format!("Run the shown run again ({})", key_label("shift+mod+r")),
                            move || run_command(s, "rerun"),
                        )
                        .disabled(move || busy() || s.current.with(|c| c.is_none())),
                    )
                    .child(
                        tool(
                            "♥",
                            "Health",
                            format!("The target's health ({})", key_label("mod+0")),
                            move || run_command(s, "health"),
                        )
                        .selected(move || s.mode.get() == Mode::Health),
                    )
                    .child(tool(
                        "⇄",
                        "Compare",
                        format!("Compare two runs ({})", key_label("shift+mod+c")),
                        move || s.compare_default(),
                    )),
            )
            // In a terminal the menu bar (F10) carries the tools: the row keeps Run/Stop.
            .child((!term).then(|| {
                view()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        toolbar_separator()
                            .w(px(1.0))
                            .h(px(18.0))
                            .mx_1()
                            .bg(move || theme().border),
                    )
                    .child(
                        toolbar_group()
                            .accessibility_label("Tools")
                            .flex_row()
                            .gap_1()
                            .child(
                                tool("⚙", "Tune", "Find better client settings", move || {
                                    s.mode.set(Mode::Tune)
                                })
                                .selected(move || s.mode.get() == Mode::Tune),
                            )
                            .child(
                                tool(
                                    "⌁",
                                    "Probe",
                                    "Handshakes and TTFB of an endpoint",
                                    move || s.mode.set(Mode::Probe),
                                )
                                .selected(move || s.mode.get() == Mode::Probe),
                            )
                            .child(tool(
                                "⇪",
                                "Export",
                                format!("Export the run ({})", key_label("mod+s")),
                                move || {
                                    let _ = popup_menu(vec![
                                        MenuItem::new("Run File (.oprobe)…").on_select(move || {
                                            crate::ui::files::export_run(s, "oprobe")
                                        }),
                                        MenuItem::new("Report (JSON)…").on_select(move || {
                                            crate::ui::files::export_run(s, "json")
                                        }),
                                        MenuItem::new("Report (Markdown)…").on_select(move || {
                                            crate::ui::files::export_run(s, "md")
                                        }),
                                        MenuItem::new("Requests (CSV)…")
                                            .on_select(move || requests::export_csv(s)),
                                    ]);
                                },
                            )),
                    )
            }))
            .child(
                view().flex_1().min_w(px(0.0)).items_center().child(
                    body(move || status_line(s))
                        .number_of_lines(1)
                        .tabular_nums()
                        .text_color(move || theme().muted_foreground),
                ),
            )
            .child((!term).then(|| {
                view()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(tool(
                        "≡",
                        "Commands",
                        format!("Command palette ({})", key_label("mod+k")),
                        move || s.open(Overlay::Palette),
                    ))
                    .child(
                        tool("◧", "Sidebar", "Show or hide the sidebar", move || {
                            s.toggle_sidebar()
                        })
                        .selected(move || s.prefs.with(|p| p.sidebar_visible)),
                    )
                    .child(
                        tool(
                            "◨",
                            "Inspector",
                            "Show or hide the inspector",
                            move || s.toggle_inspector(),
                        )
                        .selected(move || s.prefs.with(|p| p.inspector_visible)),
                    )
            })),
    )
}

fn status_line(s: Store) -> String {
    s.live.with(|l| match l {
        Some(l) if l.phase == Phase::Running => match &l.snapshot {
            Some(snap) if l.total_s > 0.0 && l.stage == Stage::Measuring => format!(
                "{} · {} / {} · {} ops/s",
                l.title,
                fmt::clock_s(snap.elapsed_s),
                fmt::clock_s(l.total_s),
                fmt::compact(snap.summary.ops_per_s)
            ),
            _ if l.kind == JobKind::Run => format!("{} · {}…", l.title, l.stage.label()),
            _ => format!(
                "{} · {}",
                l.title,
                l.log.last().cloned().unwrap_or_else(|| "starting…".into())
            ),
        },
        Some(l) if l.phase == Phase::Stopping => "Stopping — cleaning up test data…".into(),
        _ => match s.mode.get() {
            Mode::Compare => "Compare".into(),
            Mode::Health => s
                .health_target
                .get()
                .map(|t| crate::ui::model::i18n::trf("app.health_of", &[("target", &t)]))
                .unwrap_or_else(|| tr("app.health").into()),
            Mode::Tune => "Tune".into(),
            Mode::Probe => "Probe".into(),
            _ => s
                .current
                .with(|c| {
                    c.as_ref()
                        .map(|c| format!("{} · run {}", c.run.report.target.display, c.id()))
                })
                .unwrap_or_else(|| "oprobe".into()),
        },
    })
}

// ---------------------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------------------

fn verdict_tone(r: &RunSummary) -> Tone {
    Tone::of_verdict(r.verdict_level)
}

fn run_row(s: Store, r: RunSummary) -> impl IntoElement {
    let id = r.id.clone();
    let id2 = id.clone();
    let id3 = id.clone();
    let selected = {
        let id = id.clone();
        computed(move || s.sel.get() == Sel::Run(id.clone()) && matches!(s.mode.get(), Mode::Run))
    };
    let selected = move || selected.get();
    let marked = {
        let id = id.clone();
        move || s.marked.with(|m| m.contains(&id))
    };
    let today = crate::util::now_rfc3339();
    // Descriptive rows (U10): date and time (or the label), then preset, verdict and numbers;
    // the id only in the accessible name and the menu.
    let (label, detail) = crate::ui::model::health::row(&r, fmt::day_of(&today));
    let tone = verdict_tone(&r);
    let partial = runs::partial_badge(&r);
    let a11y = format!(
        "Run {} · {} · {}{}",
        r.id,
        label,
        detail,
        partial
            .as_ref()
            .map(|p| format!(" · {p}"))
            .unwrap_or_default()
    );
    row_button(a11y, selected, move || s.select_run(&id))
        .flex_row()
        .items_center()
        .gap_2()
        .on_context_menu(move |_: &ContextMenuEvent| run_menu(s, id2.clone()))
        .child(
            view()
                .w(px(7.0))
                .h(px(7.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(move || tone.color()),
        )
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .min_w(px(0.0))
                        .child(body(label).number_of_lines(1).flex_1().min_w(px(0.0)))
                        // A budget stop is not a failure: a partial run, with a badge.
                        .child(partial.map(|p| badge(p, || Tone::Warn).flex_shrink_0())),
                )
                .child(small(detail).number_of_lines(1).tabular_nums()),
        )
        .child(
            nsh_base::button()
                .accessibility_label(format!("Mark {} for comparison", id3))
                .on_press(move || s.toggle_mark(&id3))
                .w(px(18.0))
                .h(px(18.0))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .hover(|st| st.bg(move || theme().secondary))
                .focus_visible(kit::ring)
                .child(small(move || if marked() { "◆" } else { "◇" })),
        )
}

fn run_menu(s: Store, id: String) {
    let (a, b, c, d, e, f) = (
        id.clone(),
        id.clone(),
        id.clone(),
        id.clone(),
        id.clone(),
        id.clone(),
    );
    let path = s.run_store().run_path(&id);
    let _ = popup_menu(vec![
        MenuItem::new("Open").on_select(move || s.select_run(&a)),
        MenuItem::new("Mark for Comparison").on_select(move || s.toggle_mark(&b)),
        MenuItem::new("Compare with Shown Run").on_select(move || {
            if let Sel::Run(cur) = s.sel.get_untracked() {
                s.compare(&c, &cur);
            }
        }),
        MenuItem::new("Run Again")
            .disabled(s.busy())
            .on_select(move || {
                s.select_run(&d);
                set_timeout(std::time::Duration::from_millis(300), move || s.rerun());
            }),
        MenuItem::new("Edit and Run Again…")
            .disabled(s.busy())
            .on_select(move || {
                s.select_run(&f);
                set_timeout(std::time::Duration::from_millis(300), move || {
                    s.edit_rerun()
                });
            }),
        MenuItem::separator(),
        MenuItem::new("Reveal Run File").on_select(move || {
            let _ = reveal_path(path.clone());
        }),
        MenuItem::new("Delete Run")
            .destructive(true)
            .on_select(move || s.delete_run(&e)),
    ]);
}

/// Runs grouped by target, newest first.
fn grouped(runs: &[RunSummary]) -> Vec<(String, Vec<RunSummary>)> {
    let mut out: Vec<(String, Vec<RunSummary>)> = Vec::new();
    for r in runs {
        let key = r
            .target_name
            .clone()
            .unwrap_or_else(|| r.target_display.clone());
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(r.clone()),
            None => out.push((key, vec![r.clone()])),
        }
    }
    out
}

fn sidebar(s: Store) -> impl IntoElement {
    // What the list shows: the search's matches, `run_limit` of them (`RUNS (n)` is true).
    let listed = computed(move || {
        let q = s.run_query.get();
        let limit = s.run_limit.get();
        s.runs.with(|r| {
            let (shown, matched) = runs::visible(r, &q, limit);
            (shown, matched, r.len())
        })
    });
    view()
        .flex_1()
        .min_w(px(0.0))
        .bg(move || theme().sidebar)
        .macos_backdrop(Some({
            let mut b = MacOsBackdrop::default();
            b.material = MacOsBackdropMaterial::Sidebar;
            b
        }))
        .child(
            scroll().flex_1().child(
                view()
                    .p_2()
                    .gap_3()
                    // The live run, pinned.
                    .child(dynamic(move || {
                        let live = s.live.with(|l| {
                            l.as_ref()
                                .filter(|l| {
                                    l.kind == JobKind::Run
                                        && matches!(l.phase, Phase::Running | Phase::Stopping)
                                })
                                .map(|l| l.title.clone())
                        });
                        untracked(|| match live {
                            Some(title) => view()
                                .gap_1()
                                .child(caption("LIVE").px_2())
                                .child(
                                    row_button(
                                        "Live run",
                                        move || s.sel.get() == Sel::Live,
                                        move || s.select_live(),
                                    )
                                    .flex_row()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        view()
                                            .w(px(7.0))
                                            .h(px(7.0))
                                            .rounded_full()
                                            .bg(move || Tone::Info.color()),
                                    )
                                    .child(
                                        view()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .child(body(title).number_of_lines(1))
                                            .child(
                                                small(move || live_line(s))
                                                    .number_of_lines(1)
                                                    .tabular_nums(),
                                            ),
                                    ),
                                )
                                .into_element(),
                            None => view().into_element(),
                        })
                    }))
                    .child(
                        view()
                            .gap_1()
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .px_2()
                                    .child(
                                        caption(move || {
                                            listed.with(|(shown, matched, all)| {
                                                runs::header(shown.len(), *matched, *all)
                                            })
                                        })
                                        .number_of_lines(1)
                                        .flex_1(),
                                    )
                                    .child(dynamic(move || {
                                        let n = s.marked.with(|m| m.len());
                                        untracked(|| {
                                            if n == 2 {
                                                btn(
                                                    "Compare",
                                                    || Kind::Ghost,
                                                    move || s.compare_default(),
                                                )
                                                .accessibility_label("Compare the marked runs")
                                                .into_element()
                                            } else {
                                                view().into_element()
                                            }
                                        })
                                    })),
                            )
                            .child(dynamic(move || {
                                let many = s.runs.with(|r| r.len() > 8);
                                untracked(|| {
                                    if many {
                                        field("Search runs", "Search runs", s.run_query)
                                            .into_element()
                                    } else {
                                        view().into_element()
                                    }
                                })
                            }))
                            .child(dynamic(move || {
                                let (shown, matched, all) = listed.get();
                                untracked(|| {
                                    let more = matched.saturating_sub(shown.len());
                                    view()
                                        .gap_1()
                                        .child((all == 0).then(|| small("No runs yet.").px_2()))
                                        .child(
                                            (all > 0 && matched == 0).then(|| {
                                                small("No run matches the search.").px_2()
                                            }),
                                        )
                                        .children(grouped(&shown).into_iter().map(
                                            move |(target, rs)| {
                                                let key = target.clone();
                                                let selected = {
                                                    let key = target.clone();
                                                    computed(move || {
                                                        s.mode.get() == Mode::Health
                                                            && s.health_target.get().as_deref()
                                                                == Some(key.as_str())
                                                    })
                                                };
                                                view()
                                                    .gap_0p5()
                                                    .child(
                                                        row_button(
                                                            format!("Health of {target}"),
                                                            move || selected.get(),
                                                            move || {
                                                                s.show_health(Some(key.clone()))
                                                            },
                                                        )
                                                        .child(
                                                            small(format!("♥ {target}"))
                                                                .number_of_lines(1),
                                                        ),
                                                    )
                                                    .children(
                                                        rs.into_iter().map(move |r| run_row(s, r)),
                                                    )
                                            },
                                        ))
                                        .child((more > 0).then(|| {
                                            btn(
                                                format!("Show {} more", more.min(runs::PAGE)),
                                                || Kind::Ghost,
                                                move || {
                                                    s.run_limit.update(|n| *n += runs::PAGE);
                                                },
                                            )
                                            .accessibility_label(format!(
                                                "Show more runs ({more} not listed)"
                                            ))
                                            .self_start()
                                        }))
                                        .into_element()
                                })
                            })),
                    )
                    .child(dynamic(move || {
                        let targets = s.targets.get();
                        untracked(|| {
                            view()
                                .gap_1()
                                .child(
                                    view()
                                        .flex_row()
                                        .items_center()
                                        .px_2()
                                        .child(
                                            caption(format!("TARGETS ({})", targets.len()))
                                                .flex_1(),
                                        )
                                        .child(
                                            btn(
                                                "Add…",
                                                || Kind::Ghost,
                                                move || s.open_add_target(None),
                                            )
                                            .accessibility_label("Add Target"),
                                        ),
                                )
                                .child(targets.is_empty().then(|| {
                                    view()
                                        .gap_1()
                                        .px_2()
                                        .child(small(
                                            "No targets yet: save a bucket with its region, \
                                             endpoint and credentials to run it by name.",
                                        ))
                                        .child(
                                            btn(
                                                "Add Target…",
                                                || Kind::Plain,
                                                move || s.open_add_target(None),
                                            )
                                            .self_start(),
                                        )
                                }))
                                .children(targets.into_iter().map(move |(name, url)| {
                                    let n2 = name.clone();
                                    row_button(
                                        format!("Target {name}: new run"),
                                        || false,
                                        move || s.switch_target(&n2),
                                    )
                                    .child(body(name).number_of_lines(1))
                                    .child(small(url).number_of_lines(1))
                                }))
                                .into_element()
                        })
                    }))
                    .child(dynamic(move || {
                        let bl = s.baselines.get();
                        untracked(|| {
                            if bl.is_empty() {
                                return view().into_element();
                            }
                            view()
                                .gap_1()
                                .child(caption("BASELINES").px_2())
                                .children(bl.into_iter().map(move |(name, runs)| {
                                    let last = runs.last().cloned();
                                    row_button(
                                        format!("Baseline {name}"),
                                        || false,
                                        move || {
                                            if let (Some(b), Sel::Run(cur)) =
                                                (last.clone(), s.sel.get_untracked())
                                            {
                                                s.compare(&b, &cur);
                                            }
                                        },
                                    )
                                    .child(body(name).number_of_lines(1))
                                    .child(
                                        small(format!(
                                            "{} runs · compare with the shown run",
                                            runs.len()
                                        ))
                                        .number_of_lines(1),
                                    )
                                }))
                                .into_element()
                        })
                    })),
            ),
        )
}

/// The live run's line in the sidebar: its phase, and the measured time once measuring.
fn live_line(s: Store) -> String {
    s.live.with(|l| {
        let Some(l) = l else {
            return String::new();
        };
        if l.phase == Phase::Stopping {
            return "stopping · cleaning up…".into();
        }
        match l.stage {
            Stage::Measuring => format!(
                "measuring · {} of {}",
                fmt::clock_s(l.snapshot.as_ref().map(|s| s.elapsed_s).unwrap_or(0.0)),
                fmt::clock_s(l.total_s)
            ),
            st => st.label().to_owned(),
        }
    })
}

// ---------------------------------------------------------------------------------------
// The run inspector (center)
// ---------------------------------------------------------------------------------------

fn tab_bar(s: Store) -> impl IntoElement {
    let item = move |t: Tab| {
        let selected = move || s.tab.get() == t;
        tab()
            .selected(selected)
            .on_press(move |_: &()| s.set_tab(t))
            .px_3()
            .h(px(26.0))
            .justify_center()
            .rounded(px(5.0))
            .focus_visible(kit::ring)
            .child(
                text(t.title())
                    .number_of_lines(1)
                    .text_size(kit::sz(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(move || {
                        if selected() {
                            theme().foreground
                        } else {
                            theme().muted_foreground
                        }
                    }),
            )
    };
    tabs()
        .flex_row()
        .flex_shrink_0()
        .self_start()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(7.0))
        .bg(move || theme().muted)
        .indicator_style(|st| {
            st.top(px(2.0))
                .bottom(px(2.0))
                .rounded(px(5.0))
                .bg(move || theme().background)
                .shadow_sm()
        })
        .children(Tab::ALL.into_iter().map(item))
}

fn run_header(s: Store) -> impl IntoElement {
    // The run's info yields to the tabs: it takes what the tab bar leaves and is cut with an
    // ellipsis (or left out on narrow windows), never covering a tab.
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .px_3()
        .py(px(6.0))
        .min_w(px(0.0))
        .border_b_1()
        .border_color(move || theme().border)
        .child(tab_bar(s))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .items_end()
                .child(dynamic(move || {
                    let info = s.current.with(|c| {
                        c.as_ref().map(|c| {
                            let r = &c.run.report;
                            format!(
                                "{} · {} · {} · {}{}",
                                r.target.name.clone().unwrap_or(r.target.display.clone()),
                                r.workload.preset,
                                fmt::dur_us(r.run.window_s * 1e6),
                                c.id(),
                                if r.run.sampled { " · sampled" } else { "" }
                            )
                        })
                    });
                    let live = s.sel.get() == Sel::Live;
                    if narrow() {
                        return view().into_element();
                    }
                    untracked(|| {
                        small(if live {
                            "live".to_owned()
                        } else {
                            info.unwrap_or_default()
                        })
                        .number_of_lines(1)
                        .into_element()
                    })
                })),
        )
}

fn run_inspector(s: Store) -> impl IntoElement {
    view()
        .flex_1()
        .min_w(px(0.0))
        .child(run_header(s))
        .child(dynamic(move || {
            let t = s.tab.get();
            let live = s.sel.get() == Sel::Live;
            untracked(|| {
                if live && t != Tab::Overview {
                    return kit::empty(
                        format!("{} shows when the run has finished", t.label()),
                        "The overview follows the run live.",
                    )
                    .into_element();
                }
                match t {
                    Tab::Overview => overview::overview().into_element(),
                    Tab::Requests => requests::requests().into_element(),
                    Tab::Timeline => timeline::timeline().into_element(),
                    Tab::Connections => tabviews::connections().into_element(),
                    Tab::Findings => tabviews::findings().into_element(),
                    Tab::Cost => tabviews::cost().into_element(),
                }
            })
        }))
}

fn main_area(s: Store) -> impl IntoElement {
    view()
        .flex_1()
        .min_w(px(0.0))
        .bg(move || theme().background)
        // Where the keyboard focus goes back to when an overlay closes (U5).
        .focusable(true)
        .label("Main area")
        .element_ref(&s.main_ref)
        .child(dynamic(move || {
            let m = s.mode.get();
            let loading = s.loading.get() && s.current.with(|c| c.is_none());
            untracked(|| match m {
                Mode::Welcome => welcome::welcome().into_element(),
                Mode::Health => health::health_view().into_element(),
                Mode::Run if loading => kit::empty("Opening the run…", "").into_element(),
                Mode::Run => run_inspector(s).into_element(),
                Mode::Compare => compare::compare_view().into_element(),
                Mode::Tune => tools::tune_page().into_element(),
                Mode::Probe => tools::probe_page().into_element(),
            })
        }))
}

// ---------------------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------------------

fn new_run_sheet(s: Store) -> impl IntoElement {
    let f = s.form;
    let presets: Vec<(SharedString, SharedString)> = crate::config::Preset::ALL
        .iter()
        .map(|p| (p.slug().into(), p.slug().into()))
        .collect();
    // Enter in any field starts the run.
    let submit = move |_: &str| s.submit_form();
    sheet()
        .side(Placement::Right)
        .open(move || Some(s.overlay.get() == Some(Overlay::NewRun)))
        .on_open_change(move |e: &SheetEvent| {
            if let SheetEvent::OpenChange(false, _) = e {
                s.overlay.set(None);
                focus_main(s);
            }
        })
        .accessibility_label("New Run")
        .backdrop_style(|st| st.bg(move || theme().foreground.opacity(0.12)))
        .child(
            view()
                .w(px(440.0))
                .max_w(relative(1.0))
                .h_full()
                .p_4()
                .gap_3()
                .bg(move || theme().popover)
                .border_l_1()
                .border_color(move || theme().border)
                .shadow_xl()
                .child(kit::title(move || {
                    if f.spec.with(|x| x.is_some()) {
                        "Run Again"
                    } else {
                        "New Run"
                    }
                }))
                .child(dynamic(move || {
                    let run = f.spec_run.get();
                    untracked(|| match run {
                        Some(id) => small(format!(
                            "Repeats run {id}: the fields below change its spec; its dataset, \
                             mix, warmup, seed and every other setting are kept."
                        ))
                        .into_element(),
                        None => view().into_element(),
                    })
                }))
                // The first field has the focus when the sheet opens.
                .child(form_row(
                    "Target",
                    field(
                        "Target",
                        "s3://bucket/prefix · a target name · lab://demo",
                        f.target,
                    )
                    .auto_focus(true)
                    .on_submit(submit),
                ))
                .child(dynamic(move || {
                    let recent = s.prefs.with(|p| p.recent_targets.clone());
                    let targets = s
                        .targets
                        .with(|t| t.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>());
                    untracked(|| {
                        let mut chips: Vec<String> = targets;
                        for r in recent {
                            if !chips.contains(&r) {
                                chips.push(r);
                            }
                        }
                        if !chips.iter().any(|c| c.starts_with("lab://")) {
                            chips.push("lab://demo".into());
                        }
                        form_row(
                            "",
                            view()
                                .flex_row()
                                .flex_wrap()
                                .gap_1()
                                .children(chips.into_iter().take(8).map(move |c| {
                                    let c2 = c.clone();
                                    btn(c, || Kind::Ghost, move || f.target.set(c2.clone()))
                                        .h(px(22.0))
                                }))
                                .child(
                                    btn(
                                        "Add target…",
                                        || Kind::Ghost,
                                        move || s.open_add_target(None),
                                    )
                                    .h(px(22.0)),
                                ),
                        )
                        .into_element()
                    })
                }))
                .child(form_row("Workload", picker("Preset", presets, f.preset)))
                .child(form_row(
                    "Duration",
                    field("Duration", "e.g. 10s", f.duration).on_submit(submit),
                ))
                .child(form_row(
                    "Concurrency",
                    field("Concurrency", "e.g. 16", f.concurrency).on_submit(submit),
                ))
                .child(form_row("", switch_row("Advanced settings", f.advanced)))
                .child(dynamic(move || {
                    let adv = f.advanced.get();
                    untracked(|| {
                        if !adv {
                            return view().into_element();
                        }
                        view()
                            .gap_3()
                            .child(form_row(
                                "I/O per op",
                                field("I/O per op", "e.g. 4", f.io_concurrency).on_submit(submit),
                            ))
                            .child(form_row(
                                "Chunk",
                                field("Chunk", "e.g. 8MiB", f.chunk).on_submit(submit),
                            ))
                            .child(form_row(
                                "Endpoint",
                                field("Endpoint", "https://… (S3-compatible)", f.endpoint)
                                    .on_submit(submit),
                            ))
                            .child(form_row(
                                "Region",
                                field("Region", "e.g. eu-west-1", f.region).on_submit(submit),
                            ))
                            .child(form_row(
                                "Stored data",
                                field(
                                    "Stored GiB",
                                    "GiB for the cost projection (default: assumed 1 TiB)",
                                    f.stored_gib,
                                )
                                .on_submit(submit),
                            ))
                            .child(form_row(
                                "Label",
                                field("Label", "a name for this run", f.label).on_submit(submit),
                            ))
                            .child(form_row(
                                "",
                                switch_row("Anonymous (public bucket)", f.anonymous),
                            ))
                            .into_element()
                    })
                }))
                .child(dynamic(move || {
                    let e = f.error.get();
                    untracked(|| match e {
                        Some(e) => small(e)
                            .text_color(move || Tone::Crit.color())
                            .selectable(true)
                            .role(AccessibilityRole::Alert)
                            .into_element(),
                        None => view().into_element(),
                    })
                }))
                .child(view().flex_1())
                .child(small(
                    "oprobe writes test objects only under oprobe-run-<id>/ and deletes them \
                     when the run ends. Enter starts the run, Esc cancels.",
                ))
                .child(
                    view()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(btn("Cancel", || Kind::Plain, move || s.overlay.set(None)))
                        .child(
                            btn("Start Run", || Kind::Primary, move || s.submit_form())
                                .test_id("start-run"),
                        ),
                ),
        )
}

/// Add Target: URL (with its bucket), region, endpoint and the credentials source (found on
/// this machine, names only); a read-only connection test; saved to the config file.
fn add_target_sheet(s: Store) -> impl IntoElement {
    let a = s.add;
    let submit = move |_: &str| s.save_target();
    sheet()
        .side(Placement::Right)
        .open(move || Some(s.overlay.get() == Some(Overlay::AddTarget)))
        .on_open_change(move |e: &SheetEvent| {
            if let SheetEvent::OpenChange(false, _) = e {
                s.overlay.set(None);
            }
        })
        .accessibility_label("Add Target")
        .backdrop_style(|st| st.bg(move || theme().foreground.opacity(0.12)))
        .child(
            view()
                .w(px(460.0))
                .max_w(relative(1.0))
                .h_full()
                .p_4()
                .gap_3()
                .bg(move || theme().popover)
                .border_l_1()
                .border_color(move || theme().border)
                .shadow_xl()
                .child(kit::title("Add Target"))
                .child(small(
                    "A named target keeps its URL, region, endpoint and credentials source in \
                     the config file; runs then use its name.",
                ))
                .child(form_row(
                    "URL",
                    field("Target URL", "s3://bucket/prefix", a.url)
                        .auto_focus(true)
                        .on_submit(submit),
                ))
                .child(form_row(
                    "Name",
                    field("Target name", "default: the bucket", a.name).on_submit(submit),
                ))
                .child(form_row(
                    "Region",
                    field("Target region", "e.g. eu-west-1 (S3)", a.region).on_submit(submit),
                ))
                .child(form_row(
                    "Endpoint",
                    field(
                        "Target endpoint",
                        "https://… (S3-compatible, optional)",
                        a.endpoint,
                    )
                    .on_submit(submit),
                ))
                .child(form_row("Credentials", {
                    let found = crate::ui::model::cloud::detect();
                    let items: Vec<(SharedString, SharedString)> =
                        crate::ui::model::target::cred_choices(&found)
                            .into_iter()
                            .map(|c| (c.value.into(), c.title.into()))
                            .collect();
                    picker("Credentials source", items, a.credentials)
                }))
                .child(small(
                    "Sources found on this machine are listed by name; no secret is read or \
                     stored.",
                ))
                .child(dynamic(move || {
                    let e = a.error.get();
                    let t = a.test.get();
                    untracked(|| {
                        view()
                            .gap_1()
                            .child(e.map(|e| {
                                small(e)
                                    .text_color(move || Tone::Crit.color())
                                    .selectable(true)
                                    .role(AccessibilityRole::Alert)
                            }))
                            .child(t.map(|(tone, msg)| {
                                small(msg)
                                    .text_color(move || tone.color())
                                    .selectable(true)
                                    .role(AccessibilityRole::Alert)
                            }))
                            .into_element()
                    })
                }))
                .child(view().flex_1())
                .child(
                    view()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(btn("Cancel", || Kind::Plain, move || s.overlay.set(None)))
                        .child(
                            btn(
                                move || {
                                    if a.testing.get() {
                                        "Testing…"
                                    } else {
                                        "Test Connection"
                                    }
                                },
                                || Kind::Plain,
                                move || s.test_target(),
                            )
                            .disabled(move || a.testing.get()),
                        )
                        .child(btn(
                            "Save Target",
                            || Kind::Primary,
                            move || s.save_target(),
                        )),
                ),
        )
}

fn centered(content: impl IntoElement) -> View {
    view()
        .flex_1()
        .items_center()
        .pt(px(80.0))
        .bg(move || theme().foreground.opacity(0.12))
        .child(content)
}

/// The palette's frame (the command list, or a list of runs or targets to pick from).
fn palette_frame(s: Store, list: nsh_base::CommandElement) -> impl IntoElement {
    dialog()
        .on_cancel(move || s.overlay.set(None))
        .flex_1()
        .child(centered(
            view()
                .w(px(560.0))
                .max_w(relative(0.95))
                .max_h(px(420.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(move || theme().border)
                .bg(move || theme().popover)
                .shadow_xl()
                .overflow_hidden()
                .child(
                    list.auto_focus(true)
                        .empty(small("Nothing matches").p_3())
                        .search_bar_style(|st| {
                            st.px_3()
                                .h(px(40.0))
                                .justify_center()
                                .flex_row()
                                .items_center()
                                .border_b_1()
                                .border_color(move || theme().border)
                        })
                        .item_class(
                            "px-3 py-1.5 rounded-md text-sm focus:bg-accent disabled:opacity-40",
                        )
                        .group_class("px-3 pt-2 pb-1 text-xs text-muted-foreground")
                        .list_style(|st| st.p_1())
                        .on_cancel(move || s.overlay.set(None))
                        .flex_1(),
                ),
        ))
}

/// The command palette: every command with the menu's enabled states, matched fuzzily (U6),
/// best match first; a disabled command says why, every row shows its shortcut as this
/// platform writes it.
fn palette(s: Store) -> impl IntoElement {
    when(
        move || s.overlay.get() == Some(Overlay::Palette),
        move || {
            let cmds: Rc<Vec<Cmd>> = Rc::new(untracked(|| commands(s)));
            let query = signal(SharedString::default());
            let reasons: Rc<std::collections::HashMap<&'static str, &'static str>> = Rc::new(
                cmds.iter()
                    .filter_map(|c| c.reason.map(|r| (c.id, r)))
                    .collect(),
            );
            let list = cmds.clone();
            let items = computed(move || {
                let q = query.get();
                let specs: Vec<(&str, Vec<&str>)> = list
                    .iter()
                    .map(|c| {
                        let mut k = commands::spec(c.id)
                            .map(|sp| commands::keywords(&sp))
                            .unwrap_or_default();
                        // The English title too, for a translated palette.
                        if let Some(sp) = commands::spec(c.id) {
                            k.push(sp.label);
                        }
                        (c.label, k)
                    })
                    .collect();
                commands::ranked(&q, &specs)
                    .into_iter()
                    .map(|i| {
                        let c = &list[i];
                        let mut it = CommandItem::new(c.id, c.label).disabled(!c.enabled);
                        if q.trim().is_empty() {
                            it = it.group(c.group);
                        }
                        if let Some(k) = c.keys {
                            it = it.shortcut(k);
                        }
                        it
                    })
                    .collect::<Vec<_>>()
            });
            let render = move |e: &nsh_base::CommandEntry| {
                let why = reasons.get(e.id.as_ref()).copied();
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .child(body(e.label.clone()).number_of_lines(1).flex_shrink_0())
                    .child(
                        small(why.map(|w| format!("— {w}")).unwrap_or_default())
                            .number_of_lines(1)
                            .flex_1()
                            .min_w(px(0.0)),
                    )
                    .child(
                        small(e.shortcut_label.clone().unwrap_or_default())
                            .number_of_lines(1)
                            .flex_shrink_0(),
                    )
            };
            palette_frame(
                s,
                command()
                    .items(move || items.get())
                    .query(move || Some(query.get()))
                    .filterable(false)
                    .on_query_change(move |q: &SharedString| query.set(q.clone()))
                    .render_item(render)
                    .placeholder(tr("app.palette.placeholder"))
                    .accessibility_label("Commands")
                    .on_select(move |id: &SharedString| {
                        s.overlay.set(None);
                        run_command(s, id.as_ref());
                    }),
            )
        },
    )
}

/// Open Run…: the history, searchable by id, label, target and verdict.
fn run_picker(s: Store) -> impl IntoElement {
    when(
        move || s.overlay.get() == Some(Overlay::PickRun),
        move || {
            let items: Vec<CommandItem> = s.runs.with_untracked(|runs| {
                runs.iter()
                    .map(|r| {
                        let title = format!(
                            "{} · {} · {}{}",
                            r.id,
                            r.label.clone().unwrap_or_else(|| r.target_display.clone()),
                            runs::row_numbers(r),
                            runs::partial_badge(r)
                                .map(|p| format!(" · {p}"))
                                .unwrap_or_default()
                        );
                        CommandItem::new(r.id.clone(), title)
                            .group(
                                r.target_name
                                    .clone()
                                    .unwrap_or_else(|| r.target_display.clone()),
                            )
                            .keywords([r.headline.clone(), r.target_display.clone()])
                    })
                    .collect()
            });
            palette_frame(
                s,
                command()
                    .items(items)
                    .placeholder("Open a run: id, label or target")
                    .accessibility_label("Open Run")
                    .on_select(move |id: &SharedString| {
                        s.overlay.set(None);
                        s.select_run(id.as_ref());
                    }),
            )
        },
    )
}

/// Switch Target…: the configured targets; the New Run sheet opens on the one picked.
fn target_picker(s: Store) -> impl IntoElement {
    when(
        move || s.overlay.get() == Some(Overlay::PickTarget),
        move || {
            let mut items: Vec<CommandItem> = s.targets.with_untracked(|t| {
                t.iter()
                    .map(|(n, url)| {
                        CommandItem::new(n.clone(), format!("{n} · {url}")).group("Targets")
                    })
                    .collect()
            });
            items.push(CommandItem::new("+add", "Add Target…").group("New"));
            palette_frame(
                s,
                command()
                    .items(items)
                    .placeholder("Switch to a target")
                    .accessibility_label("Switch Target")
                    .on_select(move |id: &SharedString| {
                        s.overlay.set(None);
                        if id.as_ref() == "+add" {
                            s.open_add_target(None);
                        } else {
                            s.switch_target(id.as_ref());
                        }
                    }),
            )
        },
    )
}

fn help(s: Store) -> impl IntoElement {
    when(
        move || s.overlay.get() == Some(Overlay::Help),
        move || {
            let term = kit::in_terminal();
            let rows: Vec<(&'static str, &'static str)> = if term {
                vec![
                    ("r", "run again (the run's spec)"),
                    ("s", "stop"),
                    ("n", "new run"),
                    ("v", "verify the top finding"),
                    ("o", "open a run"),
                    ("t", "add a target"),
                    ("h", "the target's health"),
                    ("w", "waterfall of the slowest operation"),
                    ("1–6", "tabs"),
                    ("/", "filter requests"),
                    ("j / k", "next / previous"),
                    ("PgUp / PgDn", "a page up / down"),
                    ("g / G", "first / last"),
                    ("> / <", "sort by the next column / reverse"),
                    ("[ ] { }", "move the brush in time / latency"),
                    ("enter", "inspect"),
                    ("c", "compare"),
                    (":", "commands"),
                    ("?", "this help"),
                    ("q", "quit"),
                    ("F10", "menu"),
                    ("tab", "move the focus"),
                    ("esc", "close"),
                ]
            } else {
                commands(s)
                    .into_iter()
                    .filter_map(|c| c.keys.map(|k| (k, c.label)))
                    .collect()
            };
            dialog()
                .on_cancel(move || s.overlay.set(None))
                .flex_1()
                .child(centered(
                    view()
                        .w(px(420.0))
                        .max_w(relative(0.95))
                        .p_4()
                        .gap_1()
                        .rounded(px(10.0))
                        .bg(move || theme().popover)
                        .border_1()
                        .border_color(move || theme().border)
                        .shadow_xl()
                        .child(strong("Keyboard"))
                        .children(rows.into_iter().map(|(k, what)| {
                            view()
                                .flex_row()
                                .gap_3()
                                .child(
                                    text(if term { k.to_owned() } else { key_label(k) })
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::BOLD)
                                        .w(px(90.0)),
                                )
                                .child(small(what))
                        }))
                        .child(
                            btn("Close", || Kind::Plain, move || s.overlay.set(None)).self_end(),
                        ),
                ))
        },
    )
}

fn inspector_overlay(s: Store) -> impl IntoElement {
    sheet()
        .side(Placement::Right)
        .open(move || Some(s.overlay.get() == Some(Overlay::Inspector)))
        .on_open_change(move |e: &SheetEvent| {
            if let SheetEvent::OpenChange(false, _) = e {
                s.overlay.set(None);
            }
        })
        .accessibility_label("Inspector")
        .child(
            view()
                .w(px(380.0))
                .h_full()
                .bg(move || theme().popover)
                .child(inspector::inspector()),
        )
}

/// Settings: defaults for new runs, display, notifications. Saved as they change.
pub fn settings_content(s: Store) -> View {
    let p = s.prefs.get_untracked();
    let preset = signal(Some(SharedString::from(p.preset.clone())));
    let duration = signal(p.duration.clone());
    let conc = signal(p.concurrency.clone());
    let notify = signal(p.notify);
    let log = signal(p.log_latency);
    let appearance = signal(match p.appearance.as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    });
    effect(move || {
        let (pr, d, c, n, l, a) = (
            preset.get(),
            duration.get(),
            conc.get(),
            notify.get(),
            log.get(),
            appearance.get(),
        );
        untracked(|| {
            s.update_prefs(|p| {
                if let Some(pr) = pr {
                    p.preset = pr.to_string();
                }
                p.duration = d.clone();
                p.concurrency = c.clone();
                p.notify = n;
                p.log_latency = l;
                p.appearance = a.to_owned();
            });
            s.log_latency.set(l);
            apply_appearance(a);
            batch(|| {
                if s.busy() {
                    return;
                }
                if let Some(pr) = s.prefs.with_untracked(|p| Some(p.preset.clone())) {
                    s.form.preset.set(Some(pr.into()));
                }
                s.form.duration.set(d);
                s.form.concurrency.set(c);
            });
        });
    });
    let presets: Vec<(SharedString, SharedString)> = crate::config::Preset::ALL
        .iter()
        .map(|p| (p.slug().into(), p.slug().into()))
        .collect();
    view()
        .flex_1()
        .p_4()
        .gap_3()
        .bg(move || theme().background)
        .child(caption("DEFAULTS FOR NEW RUNS"))
        .child(form_row(
            "Workload",
            picker("Default preset", presets, preset),
        ))
        .child(form_row(
            "Duration",
            field("Default duration", "e.g. 10s", duration),
        ))
        .child(form_row(
            "Concurrency",
            field("Default concurrency", "e.g. 16", conc),
        ))
        .child(caption("DISPLAY"))
        .child(form_row(
            "Appearance",
            kit::segmented(
                "Appearance",
                &[("system", "System"), ("light", "Light"), ("dark", "Dark")],
                appearance,
            ),
        ))
        .child(form_row("", switch_row("Logarithmic latency axes", log)))
        .child(caption("NOTIFICATIONS"))
        .child(form_row(
            "",
            switch_row("Notify when a run finishes in the background", notify),
        ))
        .child(small(format!("Data: {}", s.run_store().dir().display())).selectable(true))
}

pub fn apply_appearance(a: &str) {
    set_color_scheme(match a {
        "light" => Appearance::Light,
        "dark" => Appearance::Dark,
        _ => Appearance::System,
    });
}

/// The Settings window (a dialog over the app in a terminal).
pub fn open_settings(s: Store) {
    use nsh_core::desktop::WindowKind;
    if !kit::in_terminal() {
        let w = nsh_core::element::desktop::window()
            .id("settings")
            .kind(WindowKind::Dialog)
            .title("Settings")
            .default_size(size(px(500.0), px(420.0)))
            .restore_bounds(true)
            .content(move || settings_content(s));
        if nsh_core::element::desktop::open_window(w).is_some() {
            return;
        }
    }
    s.open(Overlay::Settings);
}

fn settings_overlay(s: Store) -> impl IntoElement {
    when(
        move || s.overlay.get() == Some(Overlay::Settings),
        move || {
            dialog()
                .on_cancel(move || s.overlay.set(None))
                .flex_1()
                .child(centered(
                    view()
                        .w(px(520.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(move || theme().border)
                        .bg(move || theme().popover)
                        .shadow_xl()
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .px_4()
                                .pt_3()
                                .child(strong("Settings").flex_1())
                                .child(btn("Done", || Kind::Plain, move || s.overlay.set(None))),
                        )
                        .child(settings_content(s)),
                ))
        },
    )
}

fn toasts(s: Store) -> impl IntoElement {
    // A terminal shows them in the status row instead, never over the content (U13).
    if kit::in_terminal() {
        return view();
    }
    view()
        .absolute()
        .right(px(16.0))
        .bottom(px(36.0))
        .gap_2()
        .items_end()
        .pointer_events(PointerEvents::BoxNone)
        .child(each(
            s.toasts,
            |(id, _, _)| *id,
            move |t, _| {
                let (_, msg, tone) = t.get_untracked();
                view()
                    .max_w(px(420.0))
                    .px_3()
                    .py_2()
                    .rounded(px(8.0))
                    .bg(move || theme().popover)
                    .border_1()
                    .border_color(move || match tone {
                        Tone::Neutral => theme().border,
                        t => t.color(),
                    })
                    .shadow_lg()
                    .role(AccessibilityRole::Alert)
                    .child(body(msg).number_of_lines(3))
            },
        ))
}

fn status_bar(s: Store) -> impl IntoElement {
    let term = kit::in_terminal();
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(if term { 16.0 } else { 22.0 }))
        .px_3()
        .border_t_1()
        .border_color(move || theme().border)
        .bg(move || theme().color("toolbar").unwrap_or(theme().background))
        .child(dynamic(move || {
            let live = s.live.with(|l| l.as_ref().map(|l| (l.phase, l.kind)));
            untracked(|| match live {
                Some((Phase::Running, k)) => badge(
                    match k {
                        JobKind::Run => "running",
                        JobKind::Tune => "tuning",
                        JobKind::Probe => "probing",
                    },
                    || Tone::Info,
                )
                .into_element(),
                Some((Phase::Stopping, _)) => badge("stopping", || Tone::Warn).into_element(),
                Some((Phase::Failed, _)) => badge("failed", || Tone::Crit).into_element(),
                _ => view().into_element(),
            })
        }))
        // The newest toast takes the terminal's status row while it lasts (U13).
        .child(term.then(|| {
            dynamic(move || {
                let last = s.toasts.with(|t| t.last().cloned());
                untracked(|| match last {
                    Some((_, msg, tone)) => text(msg)
                        .text_size(kit::sz(11.0))
                        .number_of_lines(1)
                        .flex_1()
                        .min_w(px(0.0))
                        .role(AccessibilityRole::Alert)
                        .text_color(move || match tone {
                            Tone::Neutral => theme().foreground,
                            t => t.color(),
                        })
                        .test_id("status-toast")
                        .into_element(),
                    None => view().into_element(),
                })
            })
        }))
        .child(dynamic(move || {
            let toast = term && s.toasts.with(|t| !t.is_empty());
            untracked(|| {
                if toast {
                    return view().into_element();
                }
                view()
                    .flex_row()
                    .gap_3()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .children(if term {
                        vec![
                            // The essentials first: at 80 columns the last two are left out.
                            key_hint("r", "rerun"),
                            key_hint("s", "stop"),
                            key_hint("n", "new"),
                            key_hint("1-6", "tabs"),
                            key_hint("/", "filter"),
                            key_hint("c", "compare"),
                            key_hint("?", "help"),
                            key_hint("q", "quit"),
                            key_hint("v", "verify"),
                            key_hint("j/k", "move"),
                        ]
                        .into_iter()
                        .take(if narrow() { 8 } else { 10 })
                        .collect()
                    } else {
                        vec![
                            view().child(
                                small(move || {
                                    format!(
                                        "{} runs · {}",
                                        s.runs.with(|r| r.len()),
                                        s.run_store().dir().display()
                                    )
                                })
                                .number_of_lines(1),
                            ),
                        ]
                    })
                    .into_element()
            })
        }))
}

// ---------------------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------------------

pub fn shell() -> impl IntoElement {
    let s = store();
    install_menu(s);
    let filter_ref = element_ref();
    if kit::in_terminal() {
        install_terminal_keys(s, s.filter_ref);
        install_escape(s, s.filter_ref);
    }
    install_navigation_keys(s);
    // Drop `.oprobe` files on the window to open them.
    let body_view = view()
        .flex_1()
        .flex_row()
        .min_h(px(0.0))
        .on_file_drop(move |e: &FileDropEvent| {
            for u in &e.uris {
                if let Some(p) = url::Url::parse(u).ok().and_then(|x| x.to_file_path().ok()) {
                    s.open_path(p);
                }
            }
        })
        .child(dynamic(move || {
            let (sb, ins) = s.prefs.with(|p| (p.sidebar_visible, p.inspector_visible));
            let narrow = narrow();
            let mode = s.mode.get();
            let show_ins = ins && !side_panel_hidden() && mode == Mode::Run;
            let show_sb = sb && !narrow;
            untracked(|| {
                let (sw, iw) = s
                    .prefs
                    .with_untracked(|p| (p.sidebar_width, p.inspector_width));
                h_resizable()
                    .flex_1()
                    .handle_class("w-px bg-border")
                    .child(
                        resizable_panel()
                            .visible(show_sb)
                            .size(px(sw))
                            .size_range(px(180.0)..px(420.0))
                            .on_resize(move |w: &Points| s.update_prefs(|p| p.sidebar_width = w.0))
                            .child(sidebar(s)),
                    )
                    .child(resizable_panel().child(main_area(s)))
                    .child(
                        resizable_panel()
                            .visible(show_ins)
                            .size(px(iw))
                            .size_range(px(240.0)..px(520.0))
                            .on_resize(move |w: &Points| {
                                s.update_prefs(|p| p.inspector_width = w.0)
                            })
                            .child(inspector::inspector()),
                    )
                    .into_element()
            })
        }));
    let _ = filter_ref;
    view()
        .flex_1()
        .child(toolbar_view(s))
        .child(body_view)
        .child(status_bar(s))
        .child(new_run_sheet(s))
        .child(add_target_sheet(s))
        .child(inspector_overlay(s))
        .child(palette(s))
        .child(run_picker(s))
        .child(target_picker(s))
        .child(help(s))
        .child(settings_overlay(s))
        .child(toasts(s))
}

/// Keep `Inspect` in scope for the terminal's Enter.
#[allow(dead_code)]
fn _inspecting(s: Store) -> bool {
    s.inspect.with_untracked(|i| *i != Inspect::None)
}
