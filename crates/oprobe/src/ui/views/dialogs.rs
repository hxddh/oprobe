//! The app's dialogs (design app-native "Windows and dialogs"): New Run, Settings, About and,
//! on a terminal, the confirmation before deleting runs (a window asks with the system's
//! alert). Each is modal over the window, closed by Escape; Return anywhere in New Run presses
//! its default button. Buttons follow the platform's order (macOS and GNOME end with the
//! default button, Windows starts with it). On macOS Settings is a window of its own that
//! applies each change at once; elsewhere it is a preferences dialog with a close button.

use nsh_base::{SelectItem, select, theme};
use nsh_core::desktop::WindowKind;
use nsh_core::element::desktop::window;
use nsh_core::prelude::*;

use super::shell::fill;
use crate::ui::data::PlanView;
use crate::ui::i18n::{Lang, tr, tr_dyn};
use crate::ui::kit::{self, Kind, body, btn, caption, field, mono, panel, segmented, small, title};
use crate::ui::menus;
use crate::ui::store::{Dialog, LAB, PRESETS, PlanState, Store};

pub fn dialogs(s: Store) -> impl IntoElement {
    dynamic(move || {
        let d = s.dialog.get();
        untracked(|| {
            d.map(|d| {
                let (label, content) = match &d {
                    Dialog::NewRun => (tr("menu.new_run"), new_run(s)),
                    Dialog::Settings => (tr("settings.title"), preferences(s)),
                    Dialog::About => (tr("menu.about"), about(s)),
                    Dialog::Delete(ids) => (tr("delete.title"), delete(s, ids.clone())),
                };
                modal_frame(s, label, content)
            })
        })
    })
}

/// A dialog over the window: dimmed behind, centred, fading in unless the user asked for
/// less motion.
fn modal_frame(s: Store, label: &'static str, content: View) -> impl IntoElement {
    // Escape cancels wherever the focus is, a field included.
    on_key_with(
        "escape",
        KeyOptions {
            when: nsh_core::keymap::When::Always,
            ..Default::default()
        },
        move |_| s.dialog.set(None),
    );
    nsh_base::dialog()
        .label(label)
        .animation(if reduce_motion() {
            ModalAnimation::None
        } else {
            ModalAnimation::Fade
        })
        .on_cancel(move || s.dialog.set(None))
        .child(
            view()
                .flex_1()
                .items_center()
                .justify_center()
                .bg(move || -> AdaptiveColor {
                    let mut c: Hsla = color(theme().foreground);
                    c.a = 0.18;
                    c.into()
                })
                .child(
                    view()
                        .w(px(if kit::in_terminal() {
                            64.0 * 8.0
                        } else {
                            500.0
                        }))
                        .max_w(relative(0.94))
                        .gap(px(16.0))
                        .p(px(if kit::in_terminal() { 8.0 } else { 20.0 }))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(move || theme().border)
                        .bg(move || theme().popover)
                        .shadow_lg()
                        .test_id("dialog")
                        .child(content),
                ),
        )
}

/// The Settings window of macOS: open while Settings is, not modal, each change applied at
/// once (no Done); its close button or Escape closes it.
pub fn settings_window(s: Store) -> impl IntoElement {
    dynamic(move || {
        let on = menus::settings_window();
        untracked(|| {
            on.then(|| {
                window()
                    .id("settings")
                    .kind(WindowKind::Dialog)
                    .parent("main")
                    .title(move || tr("settings.title").to_owned())
                    .default_size(size(px(520.0), px(260.0)))
                    .open(move || s.settings_open.get())
                    .on_open_change(move |_| s.settings_open.set(false))
                    .content(move || {
                        view()
                            .flex_1()
                            .p(px(20.0))
                            .bg(move || theme().background)
                            .text_color(move || theme().foreground)
                            .child(settings_body(s))
                    })
            })
        })
    })
}

/// The dialog's buttons, in the platform's order: `(label, kind, action, test id)`, the
/// default button last (first on Windows).
fn buttons(
    cancel: impl Fn() + 'static,
    default: (SharedString, Kind, Box<dyn Fn()>, &'static str),
) -> View {
    buttons_reactive(
        cancel,
        move || default.0.clone(),
        default.1,
        default.2,
        default.3,
    )
}

fn buttons_reactive(
    cancel: impl Fn() + 'static,
    label: impl Fn() -> SharedString + 'static,
    kind: Kind,
    act: Box<dyn Fn()>,
    id: &'static str,
) -> View {
    let ok = btn(label, move || kind, act).test_id(id);
    // Run: busy while a run goes on, and what it does for a screen reader.
    let ok = if id == "run-start" {
        let s = crate::ui::store::store();
        ok.busy(move || s.running()).accessibility_hint(move || {
            Some(SharedString::from(if s.form_is_lab() {
                tr("runs.start_hint")
            } else {
                tr("runs.plan_hint")
            }))
        })
    } else {
        ok
    };
    let no = btn(tr("dialog.cancel"), || Kind::Plain, cancel).test_id("dialog-cancel");
    let row = view()
        .flex_row()
        .justify_end()
        .gap_2()
        .pt(px(4.0))
        .test_id("dialog-buttons");
    if menus::windows() {
        row.child(ok).child(no)
    } else {
        row.child(no).child(ok)
    }
}

/// A labelled row of a form: the label in a fixed column, the control beside it.
fn form_row(label: &'static str, control: impl IntoElement) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .min_w(px(0.0))
        .child(
            view()
                .w(px(if kit::in_terminal() {
                    14.0 * 8.0
                } else {
                    104.0
                }))
                .flex_shrink_0()
                .child(caption(tr(label)).number_of_lines(1)),
        )
        .child(view().flex_1().min_w(px(0.0)).child(control))
}

/// A pop-up button's face: its value and a chevron, bordered like a field.
fn popup_face(id: &'static str, value: impl Fn() -> SharedString + 'static) -> View {
    view()
        .test_id(id)
        .flex_row()
        .items_center()
        .gap_2()
        .min_h(px(kit::control_h()))
        .px(px(if kit::in_terminal() { 1.0 } else { 8.0 }))
        .rounded(px(6.0))
        .border_1()
        .border_color(move || theme().input)
        .bg(move || theme().background)
        .focus_visible(kit::ring)
        .child(
            text(value)
                .text_size(px(kit::metrics().body))
                .number_of_lines(1)
                .flex_1()
                .min_w(px(0.0)),
        )
        .child((!kit::in_terminal()).then(|| small("▾")))
}

const POPUP_CLASS: &str = "bg-popover border border-border rounded-md shadow-lg p-1";
const ITEM_CLASS: &str = "px-2 py-1 rounded-sm focus:bg-accent selected:font-semibold";

fn new_run(s: Store) -> View {
    let f = s.form;
    let scenario = move || {
        f.operator
            .get()
            .strip_prefix("lab://")
            .map(str::to_owned)
            .filter(|n| !n.contains(char::is_whitespace))
    };
    // Return anywhere in the dialog presses its default button (a field takes its own).
    on_key("enter", move |_| s.submit_new_run());
    view()
        .gap(px(14.0))
        .test_id("new-run-dialog")
        .child(title(tr("dialog.new_run")))
        .child(form_row(
            "runs.operator",
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    field(
                        "run-operator",
                        tr("runs.operator"),
                        tr("runs.operator_hint"),
                        move || f.operator.get(),
                        move |v| f.operator.set(v),
                    )
                    .auto_focus(true)
                    .on_submit(move |_: &str| s.submit_new_run())
                    .flex_1()
                    .min_w(px(0.0)),
                )
                .child(operator_choices(s)),
        ))
        .child(dynamic(move || {
            let sc = scenario();
            untracked(|| {
                sc.and_then(|n| tr_dyn(&format!("lab.{n}")))
                    .map(|t| form_row("runs.blank", small(t)))
            })
        }))
        .child(form_row(
            "runs.preset",
            select()
                .items(
                    PRESETS
                        .iter()
                        .map(|p| SelectItem::new(*p, *p))
                        .collect::<Vec<_>>(),
                )
                .value(move || Some(SharedString::from(f.preset.get())))
                .on_change(move |v: &Option<SharedString>| {
                    if let Some(p) = v.as_deref().and_then(|v| PRESETS.iter().find(|p| **p == v)) {
                        f.preset.set(p);
                    }
                })
                .accessibility_label(tr("runs.preset"))
                .popup_class(POPUP_CLASS)
                .item_class(ITEM_CLASS)
                .trigger(popup_face("run-preset", move || {
                    SharedString::from(f.preset.get())
                }))
                .w(px(200.0)),
        ))
        .child(form_row(
            "runs.duration",
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    field(
                        "run-duration",
                        tr("runs.duration"),
                        "10s",
                        move || f.duration.get(),
                        move |v| f.duration.set(v),
                    )
                    .on_submit(move |_: &str| s.submit_new_run())
                    .w(px(80.0)),
                )
                .child(caption(tr("runs.concurrency")))
                .child(
                    field(
                        "run-concurrency",
                        tr("runs.concurrency"),
                        "auto",
                        move || f.concurrency.get(),
                        move |v| f.concurrency.set(v),
                    )
                    .on_submit(move |_: &str| s.submit_new_run())
                    .w(px(72.0)),
                ),
        ))
        .child(form_row(
            "runs.existing",
            field(
                "run-existing",
                tr("runs.existing"),
                tr("runs.existing_hint"),
                move || f.existing.get(),
                move |v| f.existing.set(v),
            )
            .on_submit(move |_: &str| s.submit_new_run()),
        ))
        .child(dynamic(move || {
            let command = s.replayed_command();
            untracked(|| {
                command.map(|c| {
                    form_row(
                        "runs.again",
                        small(c).selectable(true).test_id("run-again-command"),
                    )
                })
            })
        }))
        .child(plan_part(s))
        .child(small(tr("runs.several")))
        .child(buttons_reactive(
            move || s.dialog.set(None),
            move || {
                let ready = s.form_is_lab() || matches!(s.plan.get(), Some(PlanState::Ready(_)));
                SharedString::from(if ready {
                    tr("runs.start")
                } else {
                    tr("runs.review_plan")
                })
            },
            Kind::Primary,
            Box::new(move || s.submit_new_run()),
            "run-start",
        ))
}

/// The operators to choose from: the lab's scenarios, the config file's operators, the
/// operator files used before, and an operator file to pick.
fn operator_choices(s: Store) -> impl IntoElement {
    const PICK: &str = "\u{1}pick";
    let mut items: Vec<SelectItem> = LAB
        .iter()
        .map(|n| SelectItem::new(format!("lab://{n}"), format!("lab://{n}")).group(tr("runs.lab")))
        .collect();
    items.extend(
        s.configured
            .get_untracked()
            .into_iter()
            .map(|n| SelectItem::new(n.clone(), n).group(tr("runs.configured"))),
    );
    items.extend(
        s.recent
            .get_untracked()
            .into_iter()
            .map(|p| SelectItem::new(p.clone(), p).group(tr("runs.recent"))),
    );
    if !kit::in_terminal() {
        items.push(SelectItem::new(PICK, tr("runs.pick_file")).group(tr("runs.files")));
    }
    select()
        .items(items)
        .value(move || Some(SharedString::from(s.form.operator.get())))
        .on_change(move |v: &Option<SharedString>| match v.as_deref() {
            Some(PICK) => s.pick_operator_file(),
            Some(op) => s.form.operator.set(if op.ends_with(".toml") {
                crate::operator::resolve::quote_operator_arg(op)
            } else {
                op.to_owned()
            }),
            None => {}
        })
        .accessibility_label(tr("runs.choose"))
        .popup_class(format!("{POPUP_CLASS} min-w-64"))
        .item_class(ITEM_CLASS)
        .group_class("px-2 pt-2 pb-1 text-xs text-muted-foreground")
        .trigger(popup_face("run-choices", || {
            SharedString::from(tr("runs.choose"))
        }))
        .w(px(if kit::in_terminal() {
            12.0 * 8.0
        } else {
            104.0
        }))
}

/// The plan of a real target (`App::plan`) before it runs: what it writes, reads and
/// deletes, how long it takes, the budgets that stop it.
fn plan_part(s: Store) -> impl IntoElement {
    dynamic(move || {
        let lab = s.form_is_lab();
        let plan = s.plan.get();
        untracked(|| {
            if lab {
                return None;
            }
            Some(
                view()
                    .test_id("run-plan")
                    .child(match plan {
                        None => small(tr("plan.first")).into_element(),
                        Some(PlanState::Planning) => small(tr("plan.planning")).into_element(),
                        Some(PlanState::Failed(e)) => body(e)
                            .text_color(move || kit::Tone::Crit.color())
                            .selectable(true)
                            .into_element(),
                        Some(PlanState::Ready(p)) => plan_summary(&p).into_element(),
                    })
                    .into_element(),
            )
        })
    })
}

fn plan_summary(p: &PlanView) -> View {
    let mut v = panel()
        .p(px(if kit::in_terminal() { 0.0 } else { 10.0 }))
        .gap_1()
        .test_id("run-plan-ready")
        .child(caption(tr("plan.title")));
    for t in &p.targets {
        v = v.child(small(t.clone()).number_of_lines(1));
    }
    v = v.child(body(fill("plan.traffic", &[&p.ops, &p.requests, &p.bytes])));
    v = v.child(body(if p.read_only {
        tr("plan.read_only").to_owned()
    } else {
        fill(
            "plan.writes",
            &[&p.written, &p.prefixes.join(", "), &p.deleted],
        )
    }));
    v = v.child(body(fill("plan.eta", &[&crate::ui::fmt::clock_s(p.eta_s)])));
    v = v.child(body(match &p.budgets {
        Some(b) => fill("plan.budgets", &[b]),
        None => tr("plan.no_budgets").to_owned(),
    }));
    for w in &p.warnings {
        v = v.child(small(w.clone()).text_color(move || kit::Tone::Warn.color()));
    }
    v
}

/// What Settings shows (the window of macOS and the dialog elsewhere): the language, where
/// runs are stored and the config file. Each change applies at once.
fn settings_body(s: Store) -> View {
    let (dir, config) = (s.data_dir(), s.config_file());
    view()
        .gap(px(14.0))
        .test_id("settings-dialog")
        .child(form_row(
            "settings.language",
            segmented(
                vec![
                    (None, SharedString::from(tr("app.lang_auto"))),
                    (Some(Lang::En), SharedString::from("English")),
                    (Some(Lang::Zh), SharedString::from("简体中文")),
                ],
                move || s.lang_choice.get(),
                move |l| s.set_lang(l),
            ),
        ))
        .child(form_row(
            "settings.data",
            mono(dir).selectable(true).number_of_lines(2),
        ))
        .child(form_row(
            "settings.config",
            mono(config).selectable(true).number_of_lines(2),
        ))
        .child(small(tr("settings.config_hint")))
        .child(form_row(
            "settings.storage",
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_1()
                .child(small(tr("settings.storage_hint")))
                .child(
                    btn(
                        tr("settings.storage_docs"),
                        || Kind::Plain,
                        || {
                            let _ = open_url(crate::util::docs_url("docs/operator.md"));
                        },
                    )
                    .test_id("settings-storage-docs"),
                ),
        ))
}

/// Settings as a preferences dialog (GNOME, Windows; Done on a terminal): a title bar with
/// the close button, the settings below.
fn preferences(s: Store) -> View {
    view()
        .gap(px(16.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .child(view().w(px(28.0)))
                .child(view().flex_1().items_center().child(title(tr(
                    if menus::mac() || menus::windows() {
                        "settings.title"
                    } else {
                        "settings.preferences"
                    },
                ))))
                .child(
                    btn(
                        if kit::in_terminal() {
                            tr("dialog.done")
                        } else {
                            "×"
                        },
                        || Kind::Ghost,
                        move || s.dialog.set(None),
                    )
                    .accessibility_label(tr("dialog.close"))
                    .test_id("dialog-done"),
                ),
        )
        .child(settings_body(s))
}

/// About oprobe (Windows, Linux, a terminal; macOS shows the system's About panel): the
/// version and where to read more.
fn about(s: Store) -> View {
    view()
        .gap(px(12.0))
        .items_center()
        .test_id("about-dialog")
        .child(
            text("oprobe")
                .text_size(px(kit::metrics().body + 8.0))
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(small(fill("about.version", &[&env!("CARGO_PKG_VERSION")])).tabular_nums())
        .child(body(tr("about.tagline")).text_center())
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(btn(
                    tr("about.website"),
                    || Kind::Plain,
                    || {
                        let _ = open_url(menus::HOME);
                    },
                ))
                .child(btn(
                    tr("menu.docs"),
                    || Kind::Plain,
                    || {
                        let _ = open_url(menus::docs());
                    },
                )),
        )
        .child(small(tr("about.license")))
        .child(
            view().flex_row().justify_end().w_full().child(
                btn(
                    tr("dialog.close"),
                    || Kind::Primary,
                    move || s.dialog.set(None),
                )
                .test_id("dialog-done"),
            ),
        )
}

/// Delete on a terminal (a window asks with the system's alert).
fn delete(s: Store, ids: Vec<String>) -> View {
    let n = ids.len();
    let what = if n == 1 {
        fill("delete.one", &[&ids[0]])
    } else {
        fill("delete.many", &[&n])
    };
    view()
        .gap(px(12.0))
        .test_id("delete-dialog")
        .child(title(what))
        .child(body(tr("delete.detail")).text_color(move || theme().muted_foreground))
        .child(buttons(
            move || s.dialog.set(None),
            (
                SharedString::from(tr("delete.confirm")),
                Kind::Danger,
                Box::new(move || {
                    s.dialog.set(None);
                    s.delete_runs(&ids);
                }),
                "delete-confirm",
            ),
        ))
}
