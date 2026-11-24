//! Compare: two runs, or several operators run on one workload, side by side: the key numbers
//! with the change against the first (latency to p99.9, requests, retries, failures and
//! throttled responses), their goodput, p99 and requests over time and latency tails drawn on
//! one chart each, where the time went, the findings, each backend API's requests and TTFB p99,
//! and how each operator differs from the first.

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::profile::{stage_bar, verdict_badge};
use super::shell::page;

use crate::ui::chart::chart;
use crate::ui::data::{self, ProfileModel};
use crate::ui::fmt;
use crate::ui::i18n::{finding_title, tr};
use crate::ui::kit::{caption, empty, mono, section, small, strong};
use crate::ui::plots;
use crate::ui::store::store;

pub fn compare() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let (ms, loading) = (s.compared.get(), s.loading.get());
        untracked(|| {
            if loading && ms.len() < 2 {
                empty(tr("app.loading"), "").into_element()
            } else if ms.len() < 2 {
                empty(tr("compare.none"), tr("compare.none_hint")).into_element()
            } else {
                page(
                    view()
                        .child(table(&ms))
                        .child(overlays(&ms))
                        .child(api_table(&ms)),
                )
                .into_element()
            }
        })
    })
}

/// Their goodput, p99 and requests over time and their latency tails, one line per run.
fn overlays(ms: &[Rc<ProfileModel>]) -> View {
    let refs: Vec<&ProfileModel> = ms.iter().map(|m| &**m).collect();
    let ops = plots::goodput_overlay(&refs);
    let p99 = plots::p99_overlay(&refs);
    let reqs = plots::requests_overlay(&refs);
    let tail = plots::tail_overlay(&refs);
    section("")
        .test_id("compare-charts")
        .child(small(tr("compare.note")).test_id("compare-note"))
        .child(ops.map(|p| chart(tr("compare.goodput"), 120.0, move || Some(p.clone()))))
        .child(p99.map(|p| {
            view()
                .test_id("compare-p99")
                .child(chart(tr("compare.p99"), 110.0, move || Some(p.clone())))
        }))
        .child(reqs.map(|p| {
            view().test_id("compare-requests").child(chart(
                tr("compare.requests"),
                110.0,
                move || Some(p.clone()),
            ))
        }))
        .child(tail.map(|p| chart(tr("compare.cdf"), 150.0, move || Some(p.clone()))))
}

/// Each backend API: its requests and TTFB p99 in every run, the change against the first.
fn api_table(ms: &[Rc<ProfileModel>]) -> Option<View> {
    let plain: Vec<ProfileModel> = ms.iter().map(|m| (**m).clone()).collect();
    let apis = data::compare_apis(&plain);
    if apis.is_empty() {
        return None;
    }
    let change =
        |v: f64, base: f64| -> Option<String> { (base > 0.0).then(|| fmt::delta(v / base - 1.0)) };
    let mut t = section(tr("compare.apis"))
        .test_id("compare-apis")
        .child(row("").children(ms.iter().map(|m| cell(strong(m.id.clone())))));
    for a in &apis {
        let base_n = a.requests_count[0] as f64;
        let base_t = a.ttfb_p99_ms[0];
        t =
            t.child(
                row(a.api.clone()).children(
                    a.requests_count.iter().zip(&a.ttfb_p99_ms).enumerate().map(
                        |(i, (n, ttfb))| {
                            let d = (i > 0).then(|| change(*n as f64, base_n)).flatten();
                            let dt = (i > 0).then(|| change(*ttfb, base_t)).flatten();
                            cell(view().gap(px(2.0)).child(pair(fill_req(*n), d)).child(pair(
                                if *n > 0 {
                                    format!("{} {}", tr("compare.ttfb_p99"), fmt::dur_ms(*ttfb))
                                } else {
                                    "—".to_owned()
                                },
                                dt,
                            )))
                        },
                    ),
                ),
            );
    }
    Some(t)
}

fn fill_req(n: u64) -> String {
    crate::ui::i18n::trf("compare.requests_of", &[&fmt::count(n)])
}

/// A value and, muted beside it, its change against the first run.
fn pair(v: String, d: Option<String>) -> View {
    view()
        .flex_row()
        .gap_2()
        .child(text(v).text_size(px(13.0)).tabular_nums())
        .child(d.map(|d| {
            text(d)
                .text_size(px(11.0))
                .tabular_nums()
                .text_color(move || theme().muted_foreground)
        }))
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
        "goodput" | "requests_per_s" => format!("{}/s", fmt::compact(v)),
        "bandwidth" => format!("{} MiB/s", fmt::sig3(v)),
        "p50" | "p99" | "p999" => fmt::dur_ms(v),
        "failed" | "retries" | "throttled_count" => fmt::count(v.max(0.0) as u64),
        _ => fmt::ratio(v),
    }
}

/// The row label of a compared metric.
fn label(key: &str) -> &'static str {
    tr(match key {
        "goodput" => "metric.goodput",
        "bandwidth" => "metric.bandwidth",
        "requests_per_s" => "compare.requests_per_s",
        "p50" => "metric.p50",
        "p99" => "metric.p99",
        "p999" => "metric.p999",
        "errors" => "metric.errors",
        "failed" => "compare.failed",
        "retries" => "compare.retries",
        "throttled_count" => "compare.throttled_count",
        _ => "metric.throttled",
    })
}

fn table(ms: &[Rc<ProfileModel>]) -> View {
    let plain: Vec<ProfileModel> = ms.iter().map(|m| (**m).clone()).collect();
    let first = plain[0].operator.clone();
    let mut t = crate::ui::kit::section_first("")
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
            row(label(r.key))
                .test_id(&format!("compare-row-{}", r.key))
                .children(r.values.iter().enumerate().map(|(i, v)| {
                    // Shares (errors, throttled) change by percentage points; the others by ratio
                    // (none against a first run of zero: from 0 to 12 retries is no percentage).
                    let share = matches!(r.key, "errors" | "throttled");
                    let shown = if share {
                        fmt::points(v - base)
                    } else if base > 0.0 {
                        fmt::delta(v / base - 1.0)
                    } else if *v == base {
                        fmt::delta(0.0)
                    } else {
                        String::new()
                    };
                    // Neutral: one run each side, so a change is not yet better or worse (run-to-run
                    // noise is in it); the note under the table points to prove.
                    cell(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(text(value(r.key, *v)).text_size(px(13.0)).tabular_nums())
                            .child((i > 0).then(|| {
                                text(shown.clone())
                                    .text_size(px(11.0))
                                    .tabular_nums()
                                    .text_color(move || theme().muted_foreground)
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
