//! Compare: two runs, or several operators run on one workload, side by side: the key numbers
//! with the change against the first, their goodput over time and latency distributions drawn
//! on one chart each, where the time went, the findings and how each operator differs from the
//! first.

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::profile::{stage_bar, verdict_badge};
use super::shell::{fill, page};
use crate::ui::chart::chart;
use crate::ui::data::{self, ProfileModel};
use crate::ui::fmt;
use crate::ui::i18n::{finding_title, tr};
use crate::ui::kit::{Kind, Tone, btn, caption, empty, mono, row_button, section, small, strong};
use crate::ui::plots;
use crate::ui::store::{Store, store};

pub fn compare() -> impl IntoElement {
    let s = store();
    page(view().child(picker(s)).child(dynamic(move || {
        let (ms, loading) = (s.compared.get(), s.loading.get());
        untracked(|| {
            if loading {
                empty(tr("app.loading"), "").into_element()
            } else if ms.len() < 2 {
                empty(tr("compare.none"), tr("compare.none_hint")).into_element()
            } else {
                view().child(table(&ms)).child(overlays(&ms)).into_element()
            }
        })
    })))
}

fn picker(s: Store) -> View {
    section(tr("compare.pick"))
        .child(dynamic(move || {
            let runs = s.runs.get();
            untracked(|| {
                view()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(runs.into_iter().take(24).map(|r| {
                        let (id, id2) = (r.id.clone(), r.id.clone());
                        let on = computed(move || s.picked.with(|p| p.contains(&id2)));
                        let on = move || on.get();
                        row_button(fill("runs.pick", &[&r.id]), on, move || s.toggle_pick(&id))
                            .border_1()
                            .border_color(move || theme().border)
                            .child(mono(r.id.clone()))
                            .child(
                                small(r.operator.clone())
                                    .number_of_lines(1)
                                    .max_w(px(180.0)),
                            )
                    }))
                    .into_element()
            })
        }))
        .child(
            btn(
                move || {
                    SharedString::from(fill("runs.compare_picked", &[&s.picked.with(|p| p.len())]))
                },
                || Kind::Primary,
                move || {
                    let ids = s.picked.get_untracked();
                    if ids.len() >= 2 {
                        s.compare(ids)
                    }
                },
            )
            .disabled(move || s.picked.with(|p| p.len() < 2))
            .test_id("compare-go"),
        )
}

/// Their goodput over time and latency distributions, one line per run.
fn overlays(ms: &[Rc<ProfileModel>]) -> View {
    let refs: Vec<&ProfileModel> = ms.iter().map(|m| &**m).collect();
    let (ops, cdf) = (plots::goodput_overlay(&refs), plots::cdf_overlay(&refs));
    section("")
        .test_id("compare-charts")
        .child(ops.map(|p| chart(tr("compare.goodput"), 120.0, move || Some(p.clone()))))
        .child(cdf.map(|p| chart(tr("compare.cdf"), 130.0, move || Some(p.clone()))))
}

const COL: f32 = 170.0;
const HEAD: f32 = 120.0;

fn cell(content: impl IntoElement) -> View {
    view()
        .w(px(COL))
        .flex_shrink_0()
        .min_w(px(0.0))
        .child(content)
}

fn row(label: impl IntoReactive<SharedString>) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .py_1()
        .border_b_1()
        .border_color(move || theme().border)
        .child(
            view()
                .w(px(HEAD))
                .flex_shrink_0()
                .child(caption(label).number_of_lines(1)),
        )
}

fn value(key: &str, v: f64) -> String {
    match key {
        "goodput" => format!("{}/s", fmt::compact(v)),
        "bandwidth" => format!("{} MiB/s", fmt::sig3(v)),
        "p50" | "p99" => fmt::dur_ms(v),
        _ => fmt::ratio(v),
    }
}

fn table(ms: &[Rc<ProfileModel>]) -> View {
    let plain: Vec<ProfileModel> = ms.iter().map(|m| (**m).clone()).collect();
    let first = plain[0].operator.clone();
    let mut t = section(fill("compare.of", &[&ms.len()]))
        .test_id("compare-table")
        .child(row("").children(ms.iter().map(|m| {
            cell(
                view()
                    .gap(px(2.0))
                    .child(strong(m.id.clone()))
                    .child(small(m.operator_ref.clone()).number_of_lines(1))
                    .child(verdict_badge(m.verdict)),
            )
        })));
    for r in data::compare_rows(&plain) {
        let base = r.values[0];
        t = t.child(
            row(tr(match r.key {
                "goodput" => "metric.goodput",
                "bandwidth" => "metric.bandwidth",
                "p50" => "metric.p50",
                "p99" => "metric.p99",
                "errors" => "metric.errors",
                _ => "metric.throttled",
            }))
            .children(r.values.iter().enumerate().map(|(i, v)| {
                let d = if base > 0.0 { v / base - 1.0 } else { 0.0 };
                let better = if r.higher_is_better { d > 0.0 } else { d < 0.0 };
                let tone = if i == 0 || d.abs() < 0.02 {
                    Tone::Neutral
                } else if better {
                    Tone::Ok
                } else {
                    Tone::Crit
                };
                cell(
                    view()
                        .flex_row()
                        .gap_2()
                        .child(text(value(r.key, *v)).text_size(px(13.0)).tabular_nums())
                        .child((i > 0).then(|| {
                            text(fmt::delta(d))
                                .text_size(px(11.0))
                                .tabular_nums()
                                .text_color(move || tone.color())
                        })),
                )
            })),
        );
    }
    t.child(row(tr("profile.time")).children(ms.iter().map(|m| cell(stage_bar(&m.stages)))))
        .child(row(tr("compare.findings")).children(ms.iter().map(|m| {
            cell(
                view().gap(px(2.0)).children(
                    m.findings
                        .iter()
                        .take(4)
                        .map(|f| small(finding_title(&f.code)).number_of_lines(1)),
                ),
            )
        })))
        .child(
            row(tr("compare.operator")).children(ms.iter().enumerate().map(|(i, m)| {
                let toml = if i == 0 {
                    tr("compare.baseline").to_owned()
                } else {
                    let (_, toml) = data::diff(&first, &m.operator);
                    if toml.trim().is_empty() {
                        tr("compare.same").to_owned()
                    } else {
                        toml
                    }
                };
                cell(mono(toml).selectable(true))
            })),
        )
}
