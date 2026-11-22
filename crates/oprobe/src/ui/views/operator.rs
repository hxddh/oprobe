//! Operator: the run's layer stack and read/write/HTTP settings as switches and fields, each
//! finding's remedy highlighted beside them. Change a value, then Prove: `App::prove` replays
//! the run A/B and its pairs stream in; the proven diff copies as Rust or TOML.

use std::collections::BTreeSet;
use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::app::{ProveObjective, ProveVerdict};
use crate::ui::data::{self, Field, FieldKind, ProfileModel, SECTIONS};
use crate::ui::fmt;
use crate::ui::i18n::{finding_title, tr, tr_dyn};
use crate::ui::jobs::{Outcome, pairs};
use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, field, mono, section, segmented, small, strong,
    switch_on, title,
};
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
    let mut v = view()
        .test_id("operator")
        .child(
            view()
                .gap_1()
                .child(title(m.operator_ref.clone()))
                .child(small(fill("operator.of_run", &[&m.id]))),
        )
        .child(remedy_list(s, &m));
    for (name, fields) in SECTIONS {
        v = v.child(section_of(s, name, fields, &m, &remedies, edited));
    }
    v.child(changes(s, m.clone(), edited)).child(prove(s))
}

fn remedy_list(s: Store, m: &ProfileModel) -> View {
    let with: Vec<_> = m
        .findings
        .iter()
        .filter(|f| !f.remedy.is_empty())
        .cloned()
        .collect();
    if with.is_empty() {
        return section(tr("operator.remedies")).child(small(tr("operator.no_remedies")));
    }
    section(tr("operator.remedies")).children(with.into_iter().map(move |f| {
        let r = f.remedy.clone();
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
                    move || s.apply_remedy(&r),
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
    section(tr_dyn(&format!("operator.section.{name}")).unwrap_or(name)).children(
        fields.iter().map(|f| {
            let rem: BTreeSet<String> = remedies
                .iter()
                .filter(|(p, _)| p == f.path || data::layer_of(p) == Some(f.path))
                .map(|(_, c)| c.clone())
                .collect();
            field_row(s, *f, data::value_of(&m.operator, f.path), rem, edited)
        }),
    )
}

fn field_row(
    s: Store,
    f: Field,
    base: String,
    remedy_of: BTreeSet<String>,
    edited: Edited,
) -> View {
    let path = f.path;
    let now = move || edited.with(|(e, _)| data::value_of(e, path));
    let changed = move || s.edits.with(|e| e.contains_key(path));
    let layer_on = move || {
        data::layer_of(path).is_none_or(|l| edited.with(|(e, _)| data::value_of(e, l) == "on"))
    };
    let highlight = !remedy_of.is_empty();
    let indent = data::layer_of(path).is_some();
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
        FieldKind::Count | FieldKind::Size | FieldKind::Duration => field(
            path,
            path,
            if base.is_empty() {
                tr("operator.default").to_owned()
            } else {
                base.clone()
            },
            move || {
                s.edits
                    .with(|e| e.get(path).cloned())
                    .unwrap_or_else(now)
            },
            move |v| s.edit(path, &v),
        )
        .w(px(110.0))
        .into_element(),
    };
    view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_2()
        .py(px(2.0))
        .pl(px(if indent { 18.0 } else { 0.0 }))
        .rounded(px(4.0))
        .opacity(move || if layer_on() { 1.0_f32 } else { 0.5 })
        .bg(move || {
            if highlight {
                theme().color("selection").unwrap_or(theme().accent)
            } else {
                transparent_black().into()
            }
        })
        .child(control)
        .child(
            text(path)
                .text_size(px(12.0))
                .font_weight(if indent {
                    FontWeight::NORMAL
                } else {
                    FontWeight::SEMIBOLD
                })
                .w(px(220.0))
                .number_of_lines(1),
        )
        .child(small(f.api).flex_1().min_w(px(0.0)).number_of_lines(1))
        .child(dynamic(move || {
            let c = changed();
            untracked(|| c.then(|| badge(tr("operator.changed"), || Tone::Info)))
        }))
        .children(remedy_of.into_iter().map(|code| {
            badge(fill("operator.remedy_of", &[&finding_title(&code)]), || {
                Tone::Warn
            })
        }))
}

fn copy_row(rust: String, toml: String) -> View {
    view()
        .flex_row()
        .gap_2()
        .child(btn(
            tr("operator.copy_rust"),
            || Kind::Plain,
            move || set_clipboard(rust.clone()),
        ))
        .child(btn(
            tr("operator.copy_toml"),
            || Kind::Plain,
            move || set_clipboard(toml.clone()),
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
                return small(tr("operator.no_changes")).into_element();
            }
            let (rust, toml) = data::diff(&base, &spec);
            view()
                .gap_2()
                .children(edits.iter().map(|(k, v)| mono(format!("{k} = {v}"))))
                .child(bad.map(|(p, why)| {
                    text(format!("{p}: {why}"))
                        .text_size(px(12.0))
                        .text_color(move || Tone::Crit.color())
                }))
                .child(caption(tr("operator.diff")))
                .child(code(rust.clone()))
                .child(code(toml.clone()))
                .child(copy_row(rust, toml).child(btn(
                    tr("operator.reset"),
                    || Kind::Ghost,
                    move || s.edits.set(Default::default()),
                )))
                .into_element()
        })
    }))
}

fn prove(s: Store) -> View {
    section(tr("operator.prove"))
        .test_id("prove")
        .child(small(tr("operator.prove_hint")))
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
                    .test_id("prove-start"),
                ),
        )
        .child(dynamic(move || {
            let st = s.prove_state.get();
            let p99 = s.objective.get() == ProveObjective::P99;
            untracked(|| st.map(|st| prove_state(st, p99)))
        }))
}

fn prove_state(st: crate::ui::jobs::State, p99: bool) -> View {
    let ps = pairs(&st.trials);
    let unit = |v: f64| {
        if p99 {
            fmt::dur_ms(v)
        } else {
            format!("{}/s", fmt::compact(v))
        }
    };
    let mut v = view().gap_1();
    if st.outcome.is_none() {
        v = v.child(small(fill("operator.replays", &[&st.trials.len()])));
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
                .child(small(
                    p.secondary
                        .iter()
                        .map(|(m, d)| format!("{m} {}", fmt::delta(*d)))
                        .collect::<Vec<_>>()
                        .join(" · "),
                ))
                .child(caption(tr("operator.proven_diff")))
                .child(code(p.rust.clone()))
                .child((!p.toml.is_empty()).then(|| code(p.toml.clone())))
                .child(copy_row(p.rust.clone(), p.toml.clone()))
        }
        Some(Outcome::Stopped) => v.child(small(tr("operator.stopped"))),
        Some(Outcome::Failed { message, .. }) => {
            v.child(body(message).text_color(move || Tone::Crit.color()))
        }
        _ => v,
    }
}
