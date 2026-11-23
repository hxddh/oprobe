//! The app's dialogs (design app-native "Windows and dialogs"): New Run, Settings and the
//! confirmation before deleting runs. Each is modal over the window, closed by Escape; Return
//! presses the default button. Buttons follow the platform's order (macOS and GNOME end with
//! the default button, Windows starts with it).

use nsh_base::theme;
use nsh_core::element::os;
use nsh_core::platform::Os;
use nsh_core::prelude::*;

use super::shell::fill;
use crate::ui::i18n::{Lang, tr, tr_dyn};
use crate::ui::kit::{self, Kind, body, btn, caption, field, panel, segmented, small, title};
use crate::ui::store::{Dialog, LAB, PRESETS, Store};

pub fn dialogs(s: Store) -> impl IntoElement {
    dynamic(move || {
        let d = s.dialog.get();
        untracked(|| {
            d.map(|d| {
                let (label, content) = match &d {
                    Dialog::NewRun => (tr("menu.new_run"), new_run(s)),
                    Dialog::Settings => (tr("settings.title"), settings(s)),
                    Dialog::Delete(ids) => (tr("delete.title"), delete(s, ids.clone())),
                };
                nsh_base::dialog()
                    .label(label)
                    .animation(ModalAnimation::Fade)
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
                                        480.0
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
            })
        })
    })
}

/// The dialog's buttons, in the platform's order: `(label, kind, action, test id)`, the
/// default button last.
fn buttons(
    cancel: impl Fn() + 'static,
    default: (String, Kind, Box<dyn Fn()>, &'static str),
) -> View {
    let (label, kind, act, id) = default;
    let ok = btn(label, move || kind, act).test_id(id);
    let no = btn(tr("dialog.cancel"), || Kind::Plain, cancel).test_id("dialog-cancel");
    let row = view().flex_row().justify_end().gap_2().pt(px(4.0));
    if matches!(os(), Os::Windows) {
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
                .w(px(if kit::in_terminal() { 14.0 * 8.0 } else { 96.0 }))
                .flex_shrink_0()
                .child(caption(tr(label)).number_of_lines(1)),
        )
        .child(view().flex_1().min_w(px(0.0)).child(control))
}

fn new_run(s: Store) -> View {
    let f = s.form;
    let scenario = move || {
        f.operator
            .get()
            .strip_prefix("lab://")
            .map(str::to_owned)
            .filter(|n| !n.contains(char::is_whitespace))
    };
    view()
        .gap(px(14.0))
        .test_id("new-run-dialog")
        .child(title(tr("dialog.new_run")))
        .child(form_row(
            "runs.operator",
            field(
                "run-operator",
                tr("runs.operator"),
                tr("runs.operator_hint"),
                move || f.operator.get(),
                move |v| f.operator.set(v),
            )
            .on_submit(move |_: &str| s.start_run()),
        ))
        .child(form_row(
            "runs.lab",
            view()
                .flex_row()
                .flex_wrap()
                .gap(px(4.0))
                .children(LAB.map(|name| {
                    let on = move || scenario().as_deref() == Some(name);
                    btn(
                        name,
                        move || if on() { Kind::Primary } else { Kind::Plain },
                        move || f.operator.set(format!("lab://{name}")),
                    )
                    .accessibility_label(format!("lab://{name}"))
                })),
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
            segmented(
                PRESETS
                    .iter()
                    .map(|p| (*p, SharedString::from(*p)))
                    .collect(),
                move || f.preset.get(),
                move |p| f.preset.set(p),
            ),
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
                    .w(px(72.0)),
                ),
        ))
        .child(small(tr("runs.several")))
        .child(buttons(
            move || s.dialog.set(None),
            (
                tr("runs.start").to_owned(),
                Kind::Primary,
                Box::new(move || s.start_run()),
                "run-start",
            ),
        ))
}

fn settings(s: Store) -> View {
    let dir = s.data_dir();
    view()
        .gap(px(14.0))
        .test_id("settings-dialog")
        .child(title(tr("settings.title")))
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
            panel()
                .px_2()
                .py_1()
                .child(kit::mono(dir).selectable(true).number_of_lines(2)),
        ))
        .child(
            view().flex_row().justify_end().child(
                btn(
                    tr("dialog.done"),
                    || Kind::Primary,
                    move || s.dialog.set(None),
                )
                .test_id("dialog-done"),
            ),
        )
}

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
                tr("menu.delete").to_owned(),
                Kind::Danger,
                Box::new(move || {
                    s.dialog.set(None);
                    s.delete_runs(&ids);
                }),
                "delete-confirm",
            ),
        ))
}
