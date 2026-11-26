//! Operator: the run's layer stack and read/write/HTTP settings as switches and fields, each
//! finding's remedy highlighted beside them. A finding's Prove sizes its remedy from the run and
//! proves it, as `oprobe prove RUN CODE` does; a value changed by hand is proved as it is
//! (`--with`). `App::prove` runs the trials A/B and its pairs stream in, drawn as they converge
//! (each pair's change, the estimate and its interval against the A/A noise floor); the proven
//! diff copies as Rust or TOML.

use std::collections::BTreeSet;
use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::app::{ProveObjective, ProveVerdict};
use crate::ui::chart::chart;
use crate::ui::data::{self, Field, FieldKind, ProfileModel, SECTIONS};
use crate::ui::fmt;
use crate::ui::i18n::{finding_title, tr, tr_dyn};
use crate::ui::jobs::Outcome;
use crate::ui::kit::{
    self, Kind, Tone, badge, body, btn, caption, empty, field, mono, section, segmented, small,
    strong, switch_on,
};
use crate::ui::plots;
use crate::ui::store::{Store, store};

pub fn operator() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let cur = s.current.get();
        untracked(|| match cur {
            None => empty(tr("operator.none"), tr("operator.none_hint")).into_element(),
            Some(m) => page(content(s, m)).into_element(),
        })
    })
}

fn content(s: Store, m: Rc<ProfileModel>) -> View {
    // Every path a finding names as its remedy, with the finding.
    let remedies: Vec<(String, String)> = m
        .findings
        .iter()
        .flat_map(|f| {
            f.remedy
                .iter()
                .map(move |(p, _)| (p.clone(), f.code.clone()))
        })
        .collect();
    let base = m.operator.clone();
    let edited = computed(move || data::apply(&base, &s.edits.get()));
    let mut v = view().test_id("operator").child(remedy_list(s, &m));
    for (name, fields) in SECTIONS {
        v = v.child(section_of(s, name, fields, &m, &remedies, edited));
    }
    let has_file = m.operator_file.is_some();
    let is_trace = m.rows.0.report.is_trace();
    let has_remedies = !remedies.is_empty();
    let stored_proves = m.proves.clone();
    v.child(changes(s, m.clone(), edited)).child(prove(
        s,
        has_file,
        is_trace,
        has_remedies,
        edited,
        stored_proves,
    ))
}

fn remedy_list(s: Store, m: &ProfileModel) -> View {
    let with: Vec<_> = m
        .findings
        .iter()
        .filter(|f| !f.remedy.is_empty())
        .cloned()
        .collect();
    if with.is_empty() {
        return kit::section_first(tr("operator.remedies"))
            .child(small(tr("operator.no_remedies")));
    }
    kit::section_first(tr("operator.remedies")).children(with.into_iter().map(move |f| {
        let code = f.code.clone();
        view()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(strong(finding_title(&f.code)))
            .child(small("→"))
            .child(mono(
                f.remedy
                    .iter()
                    .map(|(p, v)| {
                        if v.is_empty() {
                            p.clone()
                        } else {
                            format!("{p}={v}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("  "),
            ))
            .child(view().flex_1())
            .child(
                btn(
                    tr("operator.apply"),
                    || Kind::Plain,
                    move || s.prove_remedy(&code),
                )
                .accessibility_label(fill("profile.try_remedy_of", &[&f.code])),
            )
    }))
}

type Edited = Memo<(crate::operator::OperatorSpec, Option<(String, String)>)>;

fn section_of(
    s: Store,
    name: &'static str,
    fields: &'static [Field],
    m: &ProfileModel,
    remedies: &[(String, String)],
    edited: Edited,
) -> View {
    let rem = |f: &Field| -> BTreeSet<String> {
        remedies
            .iter()
            .filter(|(p, _)| p == f.path || data::layer_of(p) == Some(f.path))
            .map(|(_, c)| c.clone())
            .collect()
    };
    let row = |f: &Field, first: bool| {
        field_row(
            s,
            *f,
            data::value_of(&m.operator, f.path),
            rem(f),
            edited,
            first,
        )
    };
    let sec = section(tr_dyn(&format!("operator.section.{name}")).unwrap_or(name));
    if name != "layers" {
        return sec.child(group().children(fields.iter().enumerate().map(|(i, f)| row(f, i == 0))));
    }
    // Each layer is a group: the layer with its switch, then its parameters.
    let mut groups: Vec<Vec<&Field>> = Vec::new();
    for f in fields.iter() {
        if data::layer_of(f.path).is_none() {
            groups.push(vec![f]);
        } else if let Some(g) = groups.last_mut() {
            g.push(f);
        }
    }
    sec.children(
        groups
            .into_iter()
            .map(|g| group().children(g.into_iter().enumerate().map(|(i, f)| row(f, i == 0)))),
    )
}

/// A grouped inset surface for rows of settings.
fn group() -> View {
    kit::panel().px(px(if kit::in_terminal() { 1.0 } else { 12.0 }))
}

fn field_row(
    s: Store,
    f: Field,
    base: String,
    remedy_of: BTreeSet<String>,
    edited: Edited,
    first: bool,
) -> View {
    let path = f.path;
    let now = move || edited.with(|(e, _)| data::value_of(e, path));
    let changed = move || s.edits.with(|e| e.contains_key(path));
    let layer_on = move || {
        data::layer_of(path).is_none_or(|l| edited.with(|(e, _)| data::value_of(e, l) == "on"))
    };
    let is_layer = f.kind == FieldKind::Layer;
    let control: AnyElement = match f.kind {
        FieldKind::Layer | FieldKind::Switch => switch_on(
            path,
            move || now() == "on",
            move |on| s.edit(path, if on { "on" } else { "off" }),
        )
        .into_element(),
        FieldKind::Choice(options) => segmented(
            options
                .iter()
                .map(|o| (*o, SharedString::from(*o)))
                .collect(),
            move || {
                options
                    .iter()
                    .copied()
                    .find(|o| *o == now())
                    .unwrap_or(options[0])
            },
            move |o| s.edit(path, o),
        )
        .into_element(),
        FieldKind::Count | FieldKind::Number | FieldKind::Size | FieldKind::Duration => field(
            path,
            path,
            if base.is_empty() {
                tr("operator.default").to_owned()
            } else {
                base.clone()
            },
            move || s.edits.with(|e| e.get(path).cloned()).unwrap_or_default(),
            move |v| s.edit(path, &v),
        )
        .w(px(120.0))
        .into_element(),
    };
    // The row's name: the layer's or the parameter's own (the full path is the accessible
    // name and shows in the diff).
    let name = path.rsplit('.').next().unwrap_or(path).to_owned();
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(px(if kit::in_terminal() { 16.0 } else { 40.0 }))
        .py(px(if kit::in_terminal() { 0.0 } else { 6.0 }))
        .border_t(px(if first { 0.0 } else { 1.0 }))
        .border_color(move || theme().border)
        .opacity(move || if layer_on() { 1.0_f32 } else { 0.45 })
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    view()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            text(name)
                                .text_size(px(kit::metrics().body))
                                .font_weight(if is_layer {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::NORMAL
                                })
                                .number_of_lines(1),
                        )
                        .child(dynamic(move || {
                            let c = changed();
                            untracked(|| c.then(|| badge(tr("operator.changed"), || Tone::Info)))
                        }))
                        .children(remedy_of.into_iter().map(|code| {
                            badge(fill("operator.remedy_of", &[&finding_title(&code)]), || {
                                Tone::Warn
                            })
                        })),
                )
                .child(small(f.api).number_of_lines(1)),
        )
        .child(control)
}

fn copy_row(s: Store, rust: String, toml: String) -> View {
    view()
        .flex_row()
        .gap_2()
        .child(btn(
            tr("operator.copy_rust"),
            || Kind::Plain,
            move || s.copy(rust.clone()),
        ))
        .child(btn(
            tr("operator.copy_toml"),
            || Kind::Plain,
            move || s.copy(toml.clone()),
        ))
}

fn code(t: String) -> View {
    view()
        .p_2()
        .rounded(px(4.0))
        .bg(move || theme().muted)
        .child(mono(t).selectable(true))
}

fn changes(s: Store, m: Rc<ProfileModel>, edited: Edited) -> View {
    section(tr("operator.changes")).child(dynamic(move || {
        let edits = s.edits.get();
        let (spec, bad) = edited.get();
        let base = m.operator.clone();
        untracked(|| {
            if edits.is_empty() {
                // Nothing pending: "Copy Rust" copies the whole operator, ready to paste.
                let rust = data::rust(&base);
                return view()
                    .gap_2()
                    .child(small(tr("operator.no_changes")))
                    .child(view().flex_row().child(btn(
                        tr("operator.copy_rust"),
                        || Kind::Plain,
                        move || s.copy(rust.clone()),
                    )))
                    .into_element();
            }
            let (rust, toml) = data::diff(&base, &spec);
            view()
                .gap_2()
                .children(edits.iter().map(|(k, v)| mono(format!("{k} = {v}"))))
                .child(bad.map(|(p, why)| {
                    let clean = why
                        .strip_prefix(&format!("--set {p}="))
                        .and_then(|r| r.split_once(": ").map(|(_, m)| m))
                        .unwrap_or(&why);
                    let msg = if p.is_empty() {
                        clean.to_owned()
                    } else {
                        format!("{p}: {clean}")
                    };
                    text(msg)
                        .text_size(px(12.0))
                        .text_color(move || Tone::Crit.color())
                }))
                .child(caption(tr("operator.diff")))
                .child(code(rust.clone()))
                .child(code(toml.clone()))
                .child(copy_row(s, rust, toml).child(btn(
                    tr("operator.reset"),
                    || Kind::Ghost,
                    move || s.edits.set(Default::default()),
                )))
                .into_element()
        })
    }))
}

fn prove(
    s: Store,
    has_file: bool,
    is_trace: bool,
    has_remedies: bool,
    edited_spec: Edited,
    stored_proves: Vec<crate::ui::jobs::Proved>,
) -> View {
    let can_prove = move || {
        !is_trace
            && edited_spec.with(|(_, bad)| bad.is_none())
            && (has_remedies || s.edits.with(|e| !e.is_empty()))
    };
    let history =
        (stored_proves.len() > 1).then(|| {
            let count = stored_proves.len();
            view()
                .gap_1()
                .test_id("stored-proves")
                .child(caption(fill("operator.stored_proves", &[&count])))
                .child(view().flex_row().flex_wrap().gap_2().children(
                    stored_proves.into_iter().map(move |p| {
                        let label = format!(
                            "{} · {}",
                            p.prove_id.rsplit('-').next().unwrap_or(&p.prove_id),
                            p.change.first().map(String::as_str).unwrap_or("prove")
                        );
                        let p_clone = p.clone();
                        btn(
                            label,
                            || Kind::Plain,
                            move || {
                                s.prove_state.set(Some(crate::ui::jobs::State {
                                    outcome: Some(Outcome::Proved(Box::new(p_clone.clone()))),
                                    ..Default::default()
                                }));
                            },
                        )
                    }),
                ))
        });
    section(tr("operator.prove"))
        .test_id("prove")
        .child(small(tr("operator.prove_hint")))
        .child(history)
        .child(dynamic(move || {
            let edited = s.edits.with(|e| !e.is_empty());
            let finding = s.prove_finding.get();
            untracked(|| {
                let what = if edited {
                    tr("operator.proves_edits").to_owned()
                } else if let Some(code) = finding {
                    fill("operator.proves_finding", &[&finding_title(&code)])
                } else {
                    tr("operator.proves_top").to_owned()
                };
                body(what).test_id("prove-target")
            })
        }))
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_3()
                .child(small(tr("operator.objective")))
                .child(segmented(
                    vec![
                        (
                            ProveObjective::Goodput,
                            SharedString::from(tr("metric.goodput")),
                        ),
                        (ProveObjective::P99, SharedString::from(tr("metric.p99"))),
                    ],
                    move || s.objective.get(),
                    move |o| s.objective.set(o),
                ))
                .child(small(tr("operator.budget")))
                .child(
                    field(
                        "prove-budget",
                        tr("operator.budget"),
                        "2m",
                        move || s.prove_budget.get(),
                        move |v| s.prove_budget.set(v),
                    )
                    .w(px(64.0)),
                )
                .child(view().flex_1())
                .child(
                    btn(
                        move || {
                            SharedString::from(if s.proving() {
                                tr("runs.stop")
                            } else {
                                tr("operator.prove_run")
                            })
                        },
                        move || {
                            if s.proving() {
                                Kind::Danger
                            } else {
                                Kind::Primary
                            }
                        },
                        move || {
                            if s.proving() {
                                s.stop_prove()
                            } else {
                                s.start_prove()
                            }
                        },
                    )
                    .disabled(move || !s.proving() && !can_prove())
                    .busy(move || s.proving())
                    .accessibility_hint(move || {
                        Some(SharedString::from(if s.proving() {
                            tr("operator.prove_stop_hint")
                        } else {
                            tr("operator.prove_run_hint")
                        }))
                    })
                    .test_id("prove-start"),
                ),
        )
        .child(dynamic(move || {
            let st = s.prove_state.get();
            let live_p99 = s.objective.get() == ProveObjective::P99;
            untracked(|| {
                st.map(|st| {
                    let p99 = match &st.outcome {
                        Some(Outcome::Proved(p)) => p.objective == ProveObjective::P99,
                        _ => live_p99,
                    };
                    prove_state(s, st, p99, has_file)
                })
            })
        }))
}

fn prove_state(s: Store, st: crate::ui::jobs::State, p99: bool, has_file: bool) -> View {
    let unit = |v: f64| {
        if p99 {
            fmt::dur_ms(v)
        } else {
            format!("{}/s", fmt::compact(v))
        }
    };
    let mut v = view().gap_1();
    if st.outcome.is_none() {
        v = v.child(small(fill("operator.trials", &[&st.trials.len()])));
    }
    // The pairs as the prove recorded them once it ends, else as they stream in.
    // The interval is the one of the prove's plan (its sequential boundary).
    let (ab, max_pairs) = match &st.outcome {
        Some(Outcome::Proved(p)) if !p.ab.is_empty() => (p.ab.clone(), p.max_pairs),
        _ => (
            crate::app::prove::ab_pairs(
                if p99 {
                    ProveObjective::P99
                } else {
                    ProveObjective::Goodput
                },
                &st.trials,
            ),
            st.prove_pairs,
        ),
    };
    let ps: Vec<(f64, f64, f64)> = ab
        .iter()
        .map(|p| {
            let (a, b) = (p.baseline, p.candidate);
            (a, b, if a > 0.0 { b / a - 1.0 } else { 0.0 })
        })
        .collect();
    if let Some(plot) = plots::prove(&ab, p99, max_pairs) {
        v = v.child(view().test_id("prove-chart").child(chart(
            tr("plot.convergence"),
            150.0,
            move || Some(plot.clone()),
        )));
    }
    v = v.children(ps.iter().enumerate().map(|(i, (a, b, d))| {
        let good = if p99 { *d < 0.0 } else { *d > 0.0 };
        view()
            .flex_row()
            .gap_3()
            .child(small(fill("operator.pair", &[&(i + 1)])).w(px(64.0)))
            .child(small(format!("A {}", unit(*a))).w(px(110.0)).tabular_nums())
            .child(small(format!("B {}", unit(*b))).w(px(110.0)).tabular_nums())
            .child(
                text(fmt::delta(*d))
                    .text_size(px(12.0))
                    .tabular_nums()
                    .text_color(move || {
                        if good {
                            Tone::Ok.color()
                        } else {
                            Tone::Warn.color()
                        }
                    }),
            )
    }));
    match st.outcome {
        Some(Outcome::Proved(p)) => {
            let (tone, key) = match p.verdict {
                ProveVerdict::Improved => (Tone::Ok, "prove.improved"),
                ProveVerdict::Worse => (Tone::Crit, "prove.worse"),
                ProveVerdict::Inconclusive => (Tone::Inconclusive, "prove.inconclusive"),
            };
            let (e, lo, hi) = p.delta;
            v.test_id("prove-result")
                .child(
                    view()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .pt_2()
                        .child(badge(tr(key), move || tone))
                        .child(strong(format!(
                            "{} {} [{}, {}]",
                            if p99 { "p99" } else { tr("metric.goodput") },
                            fmt::delta(e),
                            fmt::delta(lo),
                            fmt::delta(hi)
                        )))
                        .child(small(fill("operator.pairs_count", &[&p.pairs]))),
                )
                .child(small(crate::ui::jobs::secondary_line(&p.secondary)))
                .child(plots::sizing(&p.sizing, p99).map(|plot| {
                    let heading = match &p.sized_path {
                        Some(path) => fill("plot.sizing_of", &[path]),
                        None => tr("plot.sizing").to_owned(),
                    };
                    view()
                        .test_id("prove-sizing")
                        .child(chart(heading, 120.0, move || Some(plot.clone())))
                }))
                .child((!p.notes.is_empty() || !p.environment.is_empty()).then(|| {
                    view()
                        .gap_1()
                        .test_id("prove-notes")
                        .child(caption(tr("operator.prove_notes")))
                        .children(p.notes.iter().map(|n| small(n.clone()).selectable(true)))
                        .child((!p.environment.is_empty()).then(|| {
                            kit::prop(tr("operator.prove_environment"), p.environment.clone())
                        }))
                }))
                .child(caption(tr("operator.proven_diff")))
                .child(code(p.rust.clone()))
                .child((!p.toml.is_empty()).then(|| code(p.toml.clone())))
                .child({
                    let mut actions = copy_row(s, p.rust.clone(), p.toml.clone())
                        .flex_wrap()
                        .items_center();
                    if has_file && !p.change.is_empty() {
                        let change = p.change.clone();
                        let run_id = p.run_id.clone();
                        actions = actions.child(
                            btn(
                                tr("operator.save_toml"),
                                || Kind::Plain,
                                move || s.save_proven_to_file(&run_id, &change),
                            )
                            .accessibility_hint(Some(SharedString::from(tr(
                                "operator.save_toml_hint",
                            ))))
                            .test_id("save-operator"),
                        );
                    } else if !has_file && !p.change.is_empty() && !kit::in_terminal() {
                        let change = p.change.clone();
                        let run_id = p.run_id.clone();
                        actions = actions.child(
                            btn(
                                tr("operator.save_as_toml"),
                                || Kind::Plain,
                                move || s.save_operator_as(&run_id, change.clone()),
                            )
                            .accessibility_hint(Some(SharedString::from(tr(
                                "operator.save_toml_hint",
                            ))))
                            .test_id("save-operator-as"),
                        );
                    }
                    if let Some((a, b)) = p.trial_ids.clone()
                        && s.run_file(&a).is_file()
                        && s.run_file(&b).is_file()
                    {
                        actions = actions.child(
                            btn(
                                tr("operator.compare_trials"),
                                || Kind::Plain,
                                move || s.show_selection(vec![a.clone(), b.clone()]),
                            )
                            .test_id("compare-prove-trials"),
                        );
                    }
                    actions
                })
        }
        Some(Outcome::Stopped) => v.child(small(tr("operator.stopped"))),
        Some(Outcome::Failed { message, .. }) => {
            v.child(body(message).text_color(move || Tone::Crit.color()))
        }
        _ => v,
    }
}
