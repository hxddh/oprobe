//! The window (design app-native "Structure"): the sidebar of runs beside the detail of the
//! selection, under one bar that is both the title bar and the toolbar (the system's title bar
//! is hidden; its controls stay where the platform puts them, and the bar is a drag region).
//! The detail is Profile or Operator for one run, Compare for several. Dialogs (New Run,
//! Settings, About) are modal over it; Delete and Quit ask with the system's alert. The window
//! opens where it was left, and files opened from the file manager or dropped on it open. On a
//! terminal the same structure, one cell high.

use nsh_base::theme;
use nsh_core::desktop::{Backdrop, ProgressState, WindowPlacement, WindowProgress};
use nsh_core::element::desktop::{main_window, on_quit_request, title_bar};
use nsh_core::element::os;
use nsh_core::platform::Os;
use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::ui::i18n::{tr, trf};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Kind, btn, segmented, small, title, tool_btn};
use crate::ui::menus;
use crate::ui::store::{Dialog, PLACEMENT_KEY, Page, Store, store};

/// The toolbar's height where the platform leaves it to the app: macOS's unified toolbar
/// (52 pt), Windows' title bar with a toolbar (48 px), GNOME's header bar (46 px).
fn platform_bar_h() -> f32 {
    match os() {
        Os::MacOs => 52.0,
        Os::Windows => 48.0,
        _ => 46.0,
    }
}

/// Install window- and app-level effects once per session (outside the language `dynamic`).
pub fn install_app_effects(s: Store) {
    if !kit::in_terminal() {
        menus::install(s);
        dock_progress(s);
        guard_quit(s);
        keep_placement();
        s.follow_open_files();
        s.follow_second_instance();
    } else {
        install_terminal_keys(s);
    }
}

pub fn shell() -> impl IntoElement {
    let s = store();
    view()
        .flex_1()
        .min_h(px(0.0))
        .flex_row()
        .on_file_drop(move |e: &FileDropEvent| s.open_dropped(&e.uris))
        .on_file_drag_enter(move || s.drag_over.set(true))
        .on_file_drag_leave(move || s.drag_over.set(false))
        .child(dynamic(move || {
            let on = s.sidebar.get();
            untracked(|| on.then(|| super::sidebar::sidebar(s)))
        }))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .bg(move || theme().background)
                .child(toolbar(s))
                .child(notice(s))
                .child(detail(s)),
        )
        // A file dragged over the window: the window says it takes it.
        .child(dynamic(move || {
            let over = s.drag_over.get();
            untracked(|| {
                over.then(|| {
                    view()
                        .absolute()
                        .inset_0()
                        .border(px(3.0))
                        .border_color(move || theme().primary)
                        .rounded(px(6.0))
                        .test_id("drop-target")
                })
            })
        }))
        .child(super::dialogs::settings_window(s))
        .child(super::dialogs::dialogs(s))
}

/// The height of the bar: the platform's title bar (a hidden one keeps its height), else the
/// platform's toolbar height.
pub fn bar_h() -> f32 {
    if kit::in_terminal() {
        return 16.0;
    }
    let t = title_bar().top.0;
    if t > 0.0 {
        t.max(38.0)
    } else {
        platform_bar_h()
    }
}

/// The toolbar over the detail: the sidebar toggle (when the sidebar is hidden), the title and
/// subtitle; Profile/Operator, New Run, Run/Stop and, off macOS, the primary menu.
fn toolbar(s: Store) -> View {
    let term = kit::in_terminal();
    let menu_ref = element_ref();
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .h(move || px(bar_h()))
        .flex_shrink_0()
        .px(px(if term { 1.0 } else { 12.0 }))
        .border_b_1()
        .border_color(move || theme().border)
        .window_drag(true)
        .child(dynamic(move || {
            // The window's own controls (macOS's traffic lights) sit over the sidebar while it
            // shows, else at this bar's leading end.
            let hidden = !s.sidebar.get();
            let lead = if hidden { title_bar().left.0 } else { 0.0 };
            untracked(|| {
                view()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pl(px(lead))
                    .child(hidden.then(|| {
                        tool_btn(Icon::Sidebar, tr("menu.show_sidebar"), move || {
                            s.sidebar.set(true)
                        })
                        .test_id("sidebar-show")
                    }))
                    .into_element()
            })
        }))
        .child(heading_of(s))
        .child(view().flex_1().min_w(px(8.0)))
        .child(dynamic(move || {
            let one = s.selection.with(|v| v.len() <= 1) && s.current.with(|c| c.is_some());
            untracked(|| {
                one.then(|| {
                    segmented(
                        Page::ALL
                            .iter()
                            .map(|p| (*p, SharedString::from(tr(p.key()))))
                            .collect(),
                        move || s.page.get(),
                        move |p| s.page.set(p),
                    )
                })
            })
        }))
        .child(job_status(s))
        .child(
            tool_btn(Icon::Plus, tr("menu.new_run"), move || {
                s.dialog.set(Some(Dialog::NewRun))
            })
            .test_id("new-run"),
        )
        .child(dynamic(move || {
            let busy = s.running() || s.proving();
            untracked(|| {
                if busy {
                    tool_btn(Icon::Stop, tr("menu.stop"), move || {
                        s.stop_run();
                        s.stop_prove();
                    })
                    .test_id("stop")
                    .into_element()
                } else {
                    tool_btn(Icon::Play, tr("menu.run_again"), move || s.run_again())
                        .disabled(move || s.current.with(|c| c.is_none()))
                        .test_id("run-again")
                        .into_element()
                }
            })
        }))
        .child((term || !menus::mac()).then(|| {
            tool_btn(
                if menus::windows() {
                    Icon::More
                } else {
                    Icon::Menu
                },
                tr("menu.primary"),
                move || {
                    // Under the button, its leading edge (GNOME's and Windows' menu buttons).
                    let at = menu_ref.measure();
                    s.task(async move {
                        let at = at.await.map(|m| {
                            nsh_core::geometry::point(m.page.x, m.page.y + m.frame.size.height)
                        });
                        menus::show_primary(s, at);
                    });
                },
            )
            .element_ref(&menu_ref)
            .test_id("primary-menu")
        }))
        // The window's own controls at the trailing end (Windows, GNOME's layout).
        .child(dynamic(move || {
            let right = title_bar().right.0;
            untracked(|| view().w(px(right)).flex_shrink_0())
        }))
}

/// The title and subtitle: the run (or how many are compared) and its operator. Not
/// selectable: the bar is the window's drag region (a double press zooms).
fn heading_of(s: Store) -> impl IntoElement {
    dynamic(move || {
        let n = s.selection.with(|v| v.len());
        let cur = s.current.get();
        untracked(|| {
            let (t, sub) = if n >= 2 {
                (trf("compare.of", &[&n]), String::new())
            } else if let Some(m) = cur {
                (
                    m.id.clone(),
                    format!(
                        "{} · {} · {}",
                        m.operator_ref,
                        m.preset,
                        crate::ui::fmt::clock_s(m.window_s)
                    ),
                )
            } else {
                ("oprobe".to_owned(), String::new())
            };
            view()
                .min_w(px(0.0))
                .flex_shrink(1.0)
                .child(title(t).number_of_lines(1))
                .child(
                    (!sub.is_empty() && !kit::in_terminal()).then(|| small(sub).number_of_lines(1)),
                )
                .into_element()
        })
    })
}

/// A running job in the toolbar: its phase and time, and a thin progress bar.
fn job_status(s: Store) -> impl IntoElement {
    dynamic(move || {
        let run = s.run_state.get().filter(|st| st.outcome.is_none());
        let prove = s.prove_state.get().filter(|st| st.outcome.is_none());
        untracked(|| {
            let (what, frac) = match (&run, &prove) {
                (Some(st), _) => (
                    if st.cleaning {
                        tr("phase.cleanup").to_owned()
                    } else {
                        format!(
                            "{} {}",
                            tr("app.running"),
                            crate::ui::fmt::clock_s(st.elapsed_s)
                        )
                    },
                    (st.total_s > 0.0).then(|| (st.elapsed_s / st.total_s).clamp(0.0, 1.0)),
                ),
                (None, Some(st)) => (trf("operator.trials", &[&st.trials.len()]), None),
                _ => return None,
            };
            if kit::in_terminal() {
                return Some(
                    view()
                        .flex_row()
                        .items_center()
                        .w(px(96.0))
                        .test_id("job-status")
                        .child(small(what).tabular_nums().number_of_lines(1)),
                );
            }
            Some(
                view()
                    .gap(px(3.0))
                    .w(px(150.0))
                    .test_id("job-status")
                    .child(small(what).tabular_nums().number_of_lines(1))
                    .child(frac.map(|f| kit::meter(f, "series-a").h(px(3.0)))),
            )
        })
    })
}

/// The detail of the selection.
fn detail(s: Store) -> impl IntoElement {
    dynamic(move || {
        let many = s.selection.with(|v| v.len() >= 2);
        let page = s.page.get();
        untracked(|| {
            if many {
                super::compare::compare().into_element()
            } else {
                match page {
                    Page::Profile => super::profile::profile().into_element(),
                    Page::Operator => super::operator::operator().into_element(),
                }
            }
        })
    })
}

/// The notice bar: an alert screen readers read (`Store::say` announces it), with Undo after a
/// Delete.
fn notice(s: Store) -> impl IntoElement {
    dynamic(move || {
        let n = s.notice.get();
        let undo = s.trashed.with(|t| t.is_some());
        untracked(|| {
            n.map(|(msg, tone)| {
                view()
                    .role(AccessibilityRole::Alert)
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(move || theme().border)
                    .bg(move || theme().muted)
                    .test_id("notice")
                    .child(kit::dot(move || tone))
                    .child(
                        text(msg)
                            .text_size(px(kit::metrics().small + 1.0))
                            .flex_1()
                            .min_w(px(0.0))
                            .selectable(true),
                    )
                    .child(undo.then(|| {
                        btn(tr("app.undo"), || Kind::Plain, move || s.undo_delete())
                            .test_id("notice-undo")
                    }))
                    .child(btn(tr("app.dismiss"), || Kind::Ghost, move || s.dismiss()))
            })
        })
    })
}

/// The running job's progress on the Dock icon or the taskbar button.
fn dock_progress(s: Store) {
    effect(move || {
        let run = s.run_state.get().filter(|st| st.outcome.is_none());
        let prove = s.prove_state.get().filter(|st| st.outcome.is_none());
        let progress = match (run, prove) {
            (Some(st), _) if st.total_s > 0.0 => {
                let mut p = WindowProgress::default();
                p.value = Some(Percentage(
                    (st.elapsed_s / st.total_s).clamp(0.0, 1.0) as f32
                ));
                Some(p)
            }
            (Some(_), _) | (None, Some(_)) => {
                let mut p = WindowProgress::default();
                p.state = ProgressState::Indeterminate;
                Some(p)
            }
            _ => None,
        };
        let mut attrs = nsh_core::desktop::WindowAttributes::default();
        attrs.progress = Some(progress);
        main_window().set(attrs);
    });
}

/// Quitting or closing the window while a run or a prove goes on asks first: the app's Quit
/// (⌘Q, Ctrl+Q, Exit) is a quit request, and the main window's close button does not close it
/// while a job runs.
fn guard_quit(s: Store) {
    on_quit_request(move || s.request_quit());
    let busy = computed(move || s.running() || s.proving());
    effect(move || {
        let mut attrs = nsh_core::desktop::WindowAttributes::default();
        attrs.close_on_press = Some(!busy.get());
        main_window().set(attrs);
    });
    main_window().on_close_press(move |_| {
        if busy.get_untracked() {
            s.request_quit();
        }
    });
}

/// The window opens where the user left it: its placement is saved as it changes and given
/// back at the next start (fitted on the displays there are then). macOS and Windows only: on
/// GTK the bounds nsh 0.6 reports exclude the client-side frame that `set_placement` then
/// includes, so a restored window shrinks by its frame at every start (and Wayland places
/// windows itself).
fn keep_placement() {
    if !matches!(os(), Os::MacOs | Os::Windows) {
        return;
    }
    let main = main_window();
    if let Some(p) = Storage::get_item(PLACEMENT_KEY)
        .and_then(|j| serde_json::from_str::<WindowPlacement>(&j).ok())
    {
        main.set_placement(p);
    }
    let last = stored(None::<WindowPlacement>);
    effect(move || {
        let Some(st) = main.state() else {
            return;
        };
        if last.with(|l| *l == Some(st.placement)) {
            return;
        }
        last.replace(Some(st.placement));
        if let Ok(j) = serde_json::to_string(&st.placement) {
            let _ = Storage::set_item(PLACEMENT_KEY, j);
        }
    });
}

/// The window's chrome: the title bar hidden (the toolbar takes its place), the sidebar's
/// material behind the window on macOS (vibrancy) and Windows 11 (Mica).
pub fn setup_window() {
    use nsh_core::desktop::{TitleBarOptions, TitleBarStyle, WindowAttributes};
    if kit::in_terminal() {
        return;
    }
    let mut attrs = WindowAttributes::default();
    attrs.title = Some("oprobe".into());
    attrs.min_size = Some(Some(size(px(820.0), px(520.0))));
    let mut bar = TitleBarOptions::default();
    bar.style = TitleBarStyle::Hidden;
    bar.height = Some(px(platform_bar_h()));
    attrs.title_bar = Some(bar);
    if super::sidebar::on_material() {
        attrs.backdrop = Some(if menus::mac() {
            Backdrop::Sidebar
        } else {
            Backdrop::Window
        });
    }
    main_window().set(attrs);
}

/// On a terminal there is no OS menu bar: bind the full application shortcut set directly.
fn install_terminal_keys(s: Store) {
    let opts = KeyOptions::default();
    let not_editing = KeyOptions {
        when: nsh_core::keymap::When::NotEditing,
        ..Default::default()
    };
    let free = move || !s.modal();
    on_key_with("ctrl+n", opts, move |_| {
        if free() {
            s.dialog.set(Some(Dialog::NewRun))
        }
    });
    on_key_with("ctrl+r", opts, move |_| {
        if free() {
            s.run_again()
        }
    });
    on_key_with("ctrl+.", opts, move |_| {
        s.stop_run();
        s.stop_prove();
    });
    on_key_with("ctrl+shift+s", opts, move |_| {
        if free() {
            s.sidebar.update(|v| *v = !*v)
        }
    });
    on_key_with("ctrl+shift+p", opts, move |_| {
        if free()
            && s.selection.with_untracked(|v| v.len() == 1)
            && s.current.with_untracked(|c| c.is_some())
        {
            s.page.set(Page::Operator);
            s.start_prove();
        }
    });
    on_key_with("ctrl+shift+c", not_editing, move |_| {
        if free() {
            let ids = s.selection.get_untracked();
            if !ids.is_empty() {
                s.copy(ids.join(" "));
            }
        }
    });
    on_key_with("ctrl+1", opts, move |_| s.page.set(Page::Profile));
    on_key_with("ctrl+2", opts, move |_| s.page.set(Page::Operator));
    on_key_with("ctrl+,", opts, move |_| {
        if free() {
            s.open_settings()
        }
    });
    on_key_with("f10", opts, move |_| menus::show_primary(s, None));
}

/// The frame of the detail's content: scrolling and inset, its width capped and centred on
/// wide windows.
pub fn page(body: View) -> impl IntoElement {
    let term = kit::in_terminal();
    scroll().flex_1().min_h(px(0.0)).child(
        view().w_full().items_center().child(
            body.w_full()
                .px(px(if term { 1.0 } else { 24.0 }))
                .py(px(if term { 0.0 } else { 20.0 }))
                .gap(px(if term { 16.0 } else { 20.0 }))
                .min_w(px(0.0))
                .max_w(px(1100.0)),
        ),
    )
}

/// `{}` filled in a translated string.
pub fn fill(key: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    trf(key, args)
}
