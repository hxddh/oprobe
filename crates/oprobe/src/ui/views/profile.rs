//! Profile: one run (`App::show`): the verdict, the key numbers, where the time went (and per
//! verb), the findings with their remedy path, the chart of their evidence and their example
//! operations, the request waterfall of the slowest operations, the timeline (goodput over the
//! attempted rate, the intervals with throttled responses or failures shaded, quantiles muted
//! where too few operations back them, throughput, requests, the problems per second on their
//! own axis, in flight), the latency distribution as a tail plot, the heatmap and the
//! connections.
//!
//! The timeline charts and the heatmap share one time cursor and one selected range
//! ([`TimeLink`]): a drag selects a range, and the key numbers, the requests by API, the verbs,
//! the distribution and the slowest operations then show that range, recomputed from the trace
//! rows in core (`report::range`). Escape, a double-click or Clear drops it. Drill-downs (a
//! finding's examples, a heatmap cell, an API) show nothing until asked.

use std::rc::Rc;

use nsh_base::{button, theme};
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::report::view::{OpLine, ReqLine, ThroughputChart};
use crate::ui::chart::{TimeLink, chart, linked_chart};
use crate::ui::data::{FindingRow, ProfileModel, RangeModel, VerbRow, Waterfall};
use crate::ui::fmt;
use crate::ui::heatmap::{Cell, heatmap};
use crate::ui::i18n::{finding_title, tr, tr_dyn};
use crate::ui::kit::{
    self, Col, Kind, Tone, badge, body, btn, caption, empty, meter, mono, prop, section, small,
    stage_token, strong, swatch, tile, tile_row, token,
};
use crate::ui::plots;
use crate::ui::store::{Store, store};

pub fn profile() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let (cur, loading) = (s.current.get(), s.loading.get());
        untracked(|| match cur {
            _ if loading => empty(tr("app.loading"), "").into_element(),
            None => empty(tr("profile.none"), tr("profile.none_hint")).into_element(),
            Some(m) => page(content(s, m)).into_element(),
        })
    })
}

pub fn verdict_badge(v: crate::model::VerdictLevel) -> View {
    badge(
        tr_dyn(&format!("verdict.{}", v.as_str())).unwrap_or(v.as_str()),
        move || Tone::of_verdict(v),
    )
}

/// What the Profile's linked parts share: the time link, the selected range recomputed, and the
/// verb the timeline is filtered to (0: every verb, `i + 1`: `timeline_verbs[i]`).
#[derive(Clone, Copy)]
struct Ctx {
    link: TimeLink,
    range: Memo<Option<Rc<RangeModel>>>,
    verb: Signal<usize>,
}

fn content(s: Store, m: Rc<ProfileModel>) -> View {
    let link = TimeLink::new();
    let rm = m.clone();
    let range = computed(move || {
        link.range
            .get()
            .filter(|_| rm.has_rows())
            .map(|(a, b)| Rc::new(rm.range(a, b)))
    });
    let cx = Ctx {
        link,
        range,
        verb: signal(0usize),
    };
    // Escape drops the selected range (and goes on to whatever else listens).
    on_key_with(
        "escape",
        KeyOptions {
            pass_through: true,
            ..Default::default()
        },
        move |_| {
            if link.range.get_untracked().is_some() || link.drag.get_untracked().is_some() {
                link.clear();
            }
        },
    );
    let mut v = view()
        .test_id("profile")
        .child(head(s, &m))
        .child(trust_notice(&m))
        .child(range_bar(&m, cx))
        .child(tiles(&m, cx))
        .child(findings(s, &m))
        .child(time_went(&m, cx))
        .child(verbs_section(&m, cx))
        .child(requests(&m, cx))
        .child((!m.retry.quiet() || m.capacity.is_some()).then(|| retries(&m, cx)));
    if !m.slowest.is_empty() {
        v = v.child(slowest_section(&m, cx));
    }
    let m2 = m.clone();
    let m3 = m.clone();
    v.child(timeline(&m, cx))
        .child(distribution(&m, cx))
        .child(heat_section(&m, cx))
        .child(conns(&m, cx))
        .child(traffic(&m, cx))
        .child(kit::disclosure(
            tr("profile.details"),
            "details",
            move || details(&m2),
        ))
        .child(kit::disclosure(tr("profile.setup"), "setup", move || {
            setup(&m3)
        }))
        .child((!m.warnings.is_empty()).then(|| {
            section(tr("profile.warnings"))
                .children(m.warnings.iter().map(|w| small(w.clone()).selectable(true)))
        }))
}

/// The selected range: what it is, how far to trust it, and Clear.
fn range_bar(m: &ProfileModel, cx: Ctx) -> Option<View> {
    if !m.has_rows() {
        return None;
    }
    Some(view().child(dynamic(move || {
        let r = cx.range.get();
        untracked(|| match r {
            None => small(tr("range.hint")).test_id("range-hint").into_element(),
            Some(r) => {
                let v = &r.view;
                kit::panel()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .test_id("range-bar")
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                strong(fill(
                                    "range.title",
                                    &[&fmt::clock_s(v.since_s), &fmt::clock_s(v.until_s)],
                                ))
                                .flex_1()
                                .min_w(px(0.0)),
                            )
                            .child(
                                btn(tr("range.clear"), || Kind::Plain, move || cx.link.clear())
                                    .test_id("range-clear"),
                            ),
                    )
                    .child(v.sampled.then(|| {
                        small(fill("range.sampled", &[&fmt::ratio(v.kept_ratio)]))
                            .test_id("range-sampled")
                    }))
                    .child(v.is_empty().then(|| small(tr("range.empty"))))
                    .into_element()
            }
        })
    })))
}

/// What makes the numbers less certain, up front.
fn trust_notice(m: &ProfileModel) -> Option<View> {
    let t = &m.trust;
    if !t.concerns() {
        return None;
    }
    let mut lines = Vec::new();
    if t.client_saturated {
        lines.push(fill(
            "trust.saturated",
            &[
                &t.cpu_ratio.map(fmt::ratio).unwrap_or_else(|| "—".into()),
                &t.sched_lag_p99_ms
                    .map(fmt::dur_ms)
                    .unwrap_or_else(|| "—".into()),
            ],
        ));
    }
    if let Some(p) = &t.partial {
        lines.push(fill("trust.partial", &[p]));
    }
    if let Some(mr) = t.missed_arrivals_ratio.filter(|m| *m > 0.01) {
        lines.push(fill("trust.missed", &[&fmt::ratio(mr)]));
    }
    if t.ops_count < 100 {
        lines.push(fill("trust.few", &[&t.ops_count]));
    }
    if t.corrupted_count > 0 {
        lines.push(fill(
            "trust.corrupted",
            &[
                &fmt::count(t.corrupted_count),
                &fmt::count(t.verified_count),
            ],
        ));
    }
    if !t.breaches.is_empty() {
        lines.push(fill("trust.breaches", &[&t.breaches.join("; ")]));
    }
    Some(
        kit::panel()
            .p_3()
            .gap_1()
            .test_id("trust-notice")
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(kit::dot(|| Tone::Warn))
                    .child(strong(tr("trust.title"))),
            )
            .children(lines.into_iter().map(|l| small(l).selectable(true))),
    )
}

/// The verdict and its one-line headline (the run's id and operator are the toolbar's title).
fn head(_s: Store, m: &ProfileModel) -> View {
    view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_3()
        .child(verdict_badge(m.verdict))
        .child(
            body(m.headline.clone())
                .selectable(true)
                .flex_1()
                .min_w(px(0.0)),
        )
}

/// The key numbers: of the whole run, or of the selected range.
struct Numbers {
    goodput: f64,
    mib: f64,
    p50: f64,
    p99: f64,
    p999: f64,
    errors: f64,
    throttled: f64,
    counts: String,
}

fn tiles(m: &ProfileModel, cx: Ctx) -> View {
    let whole = Numbers {
        goodput: m.good_ops_per_s,
        mib: m.mib_per_s,
        p50: m.p50_ms,
        p99: m.p99_ms,
        p999: m.p999_ms,
        errors: m.error_ratio,
        throttled: m.throttled_ratio,
        counts: fill(
            "metric.counts",
            &[
                &fmt::count(m.ops_count),
                &fmt::count(m.requests_count),
                &fmt::clock_s(m.window_s),
            ],
        ),
    };
    let whole = Rc::new(whole);
    view().test_id("tiles").child(dynamic(move || {
        let r = cx.range.get();
        let n = match &r {
            Some(r) => {
                let v = &r.view;
                Rc::new(Numbers {
                    goodput: v.goodput_ops_per_s,
                    mib: v.mib_per_s,
                    p50: v.p50_ms,
                    p99: v.p99_ms,
                    p999: v.p999_ms,
                    errors: v.error_ratio,
                    throttled: v.throttled_request_ratio,
                    counts: fill(
                        "range.counts",
                        &[
                            &fmt::count(v.ops_count),
                            &fmt::count(v.requests_count),
                            &fmt::clock_s(v.span_s()),
                        ],
                    ),
                })
            }
            None => whole.clone(),
        };
        untracked(|| numbers_view(&n).into_element())
    }))
}

fn numbers_view(n: &Numbers) -> View {
    let t = |label: &'static str, v: String, tone: Tone| {
        tile(tr(label), move || v.clone().into(), move || tone)
    };
    view()
        .gap(px(6.0))
        .child(
            tile_row()
                .child(t(
                    "metric.goodput",
                    format!("{}/s", fmt::compact(n.goodput)),
                    Tone::Neutral,
                ))
                .child(t(
                    "metric.bandwidth",
                    format!("{} MiB/s", fmt::sig3(n.mib)),
                    Tone::Neutral,
                ))
                .child(t("metric.p50", fmt::dur_ms(n.p50), Tone::Neutral))
                .child(t("metric.p99", fmt::dur_ms(n.p99), Tone::Neutral))
                .child(t("metric.p999", fmt::dur_ms(n.p999), Tone::Neutral))
                .child(t(
                    "metric.errors_of_ops",
                    fmt::ratio(n.errors),
                    if n.errors > 0.0 {
                        Tone::Crit
                    } else {
                        Tone::Neutral
                    },
                ))
                .child(t(
                    "metric.throttled_of_requests",
                    fmt::ratio(n.throttled),
                    if n.throttled > 0.0 {
                        Tone::Warn
                    } else {
                        Tone::Neutral
                    },
                )),
        )
        .child(
            small(n.counts.clone())
                .tabular_nums()
                .test_id("tile-counts"),
        )
}

/// The requests-by-API columns (the total's on a terminal wider, so `261 ms / 1.20 s` fits).
fn api_cols() -> [Col; 9] {
    let term = kit::in_terminal();
    [
        Col {
            heading: "col.api",
            width: 0.0,
            numeric: false,
        },
        Col {
            heading: "col.requests",
            width: if term { 72.0 } else { 60.0 },
            numeric: true,
        },
        Col {
            heading: "col.errors",
            width: 52.0,
            numeric: true,
        },
        Col {
            heading: "col.throttled",
            width: if term { 80.0 } else { 64.0 },
            numeric: true,
        },
        Col {
            heading: "col.status",
            width: if term { 176.0 } else { 140.0 },
            numeric: false,
        },
        Col {
            heading: "col.ttfb",
            width: if term { 144.0 } else { 116.0 },
            numeric: true,
        },
        Col {
            heading: "col.total_p50_p99",
            width: if term { 144.0 } else { 128.0 } + kit::CELL_BAR_W,
            numeric: true,
        },
        Col {
            heading: "col.bytes",
            width: if term { 160.0 } else { 128.0 },
            numeric: true,
        },
        Col {
            heading: "col.reused",
            width: 60.0,
            numeric: true,
        },
    ]
}

/// The total-latency column (its inline bar).
const API_TOTAL_COL: usize = 6;

/// An API's status codes: its commonest answers and every error code (core's
/// `ApiRow::shown_status`).
fn status_line(a: &crate::report::view::ApiRow) -> String {
    a.shown_status()
        .iter()
        .map(|(c, n)| match *c {
            // An answer below 400: the code alone (the requests column counts them).
            1..400 => c.to_string(),
            0 => format!("— ×{}", fmt::count(*n)),
            _ => format!("{c} ×{}", fmt::count(*n)),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How OpenDAL wrote: the part size it used (and why, when it is not what was asked) and the
/// parts it uploaded.
fn write_line(w: &crate::report::view::WritePathView) -> String {
    let mut parts = Vec::new();
    if w.write_chunk_bytes > 0 {
        let mut s = fill("write.part_size", &[&fmt::bytes(w.write_chunk_bytes)]);
        let mut why = Vec::new();
        if w.clamped {
            why.push(fill("write.asked", &[&fmt::bytes(w.requested_chunk_bytes)]));
        }
        if let (Some(a), Some(b)) = (w.service_min_part_bytes, w.service_max_part_bytes) {
            why.push(fill("write.service", &[&fmt::bytes(a), &fmt::bytes(b)]));
        }
        if !why.is_empty() {
            s += &format!(" ({})", why.join("; "));
        }
        parts.push(s);
    }
    if let Some(m) = w.part_mean_bytes {
        parts.push(fill(
            "write.parts",
            &[&fmt::count(w.parts_count), &fmt::bytes(m)],
        ));
    }
    if let Some(sk) = w.part_skew_ratio.filter(|x| *x >= 1.25) {
        parts.push(fill("write.skew", &[&fmt::sig3(sk)]));
    }
    if let Some(ms) = w.complete_p99_ms {
        parts.push(fill("write.complete", &[&fmt::dur_ms(ms)]));
    }
    parts.join(" · ")
}

/// The backend's requests by API (of the range when one is selected); clicking an API lists
/// its slowest requests, and clicking one of those shows its operation's waterfall.
fn requests(m: &Rc<ProfileModel>, cx: Ctx) -> Option<View> {
    if m.apis.is_empty() {
        return None;
    }
    let m = m.clone();
    let picked = signal(None::<String>);
    let opened = signal(None::<u64>);
    let writes = m
        .write_path
        .as_ref()
        .filter(|w| w.shown())
        .map(|w| prop(tr("write.path"), write_line(w)).test_id("write-path"));
    let lists = m.list_path.as_ref().map(|lp| {
        prop(
            tr("list.path"),
            fill(
                "list.summary",
                &[
                    &fmt::count(lp.pages_count),
                    &fmt::count(lp.list_ops_count),
                    &fmt::sig3(lp.pages_per_op),
                    &fmt::dur_ms(lp.page_p50_ms),
                    &fmt::dur_ms(lp.page_p99_ms),
                ],
            ),
        )
        .test_id("list-path")
    });
    Some(
        section(tr("profile.requests"))
            .test_id("requests")
            .child(writes)
            .child(lists)
            .child(m.has_rows().then(|| small(tr("drill.api_hint"))))
            .child(dynamic(move || {
                let r = cx.range.get();
                let apis = r
                    .as_ref()
                    .map(|r| r.view.apis.clone())
                    .unwrap_or_else(|| m.apis.clone());
                let range = r.as_ref().map(|r| (r.view.since_s, r.view.until_s));
                let m = m.clone();
                untracked(move || {
                    let rows = apis
                        .iter()
                        .map(|a| {
                            vec![
                                a.api.clone(),
                                fmt::count(a.requests_count),
                                fmt::count(a.errors_count),
                                fmt::count(a.throttled_count),
                                status_line(a),
                                format!(
                                    "{} / {}",
                                    fmt::dur_ms(a.ttfb_p50_ms),
                                    fmt::dur_ms(a.ttfb_p99_ms)
                                ),
                                format!(
                                    "{} / {}",
                                    fmt::dur_ms(a.total_p50_ms),
                                    fmt::dur_ms(a.total_p99_ms)
                                ),
                                format!(
                                    "{} / {}",
                                    fmt::bytes(a.down_bytes),
                                    fmt::bytes(a.up_bytes)
                                ),
                                // Unknown is not 0 %: the operator's own HTTP client.
                                if m.conns.observed {
                                    fmt::ratio(a.reused_ratio)
                                } else {
                                    "–".to_owned()
                                },
                            ]
                        })
                        .collect();
                    let names: Vec<String> = apis.iter().map(|a| a.api.clone()).collect();
                    let names_sel = names.clone();
                    let names_press = names.clone();
                    let bars = api_bars(&apis);
                    let table = if m.has_rows() {
                        kit::pressable_table_bars(
                            &api_cols(),
                            rows,
                            bars,
                            "api-row",
                            move || {
                                let cur = picked.get();
                                cur.and_then(|name| names_sel.iter().position(|n| n == &name))
                            },
                            move |i| {
                                opened.set(None);
                                let clicked = names_press.get(i).cloned();
                                picked.update(|p| *p = if *p == clicked { None } else { clicked });
                            },
                        )
                        .into_element()
                    } else {
                        kit::table_bars(&api_cols(), rows, bars).into_element()
                    };
                    view()
                        .child(table)
                        .child(dynamic(move || {
                            let p = picked.get();
                            let m = m.clone();
                            let names = names.clone();
                            untracked(move || {
                                p.filter(|api| names.contains(api)).map(|api| {
                                    let reqs = m.api_requests(&api, range);
                                    api_drill(&m, &api, reqs, opened, picked)
                                })
                            })
                        }))
                        .into_element()
                })
            })),
    )
}

/// Each API's total p50 inside its p99, as a bar scaled to the slowest API's p99: the slow API
/// stands out without reading numbers.
fn api_bars(apis: &[crate::report::view::ApiRow]) -> Vec<Vec<Option<kit::CellBar>>> {
    let top = apis.iter().map(|a| a.total_p99_ms).fold(0.0, f64::max);
    apis.iter()
        .map(|a| {
            let mut row = vec![None; api_cols().len()];
            if top > 0.0 {
                row[API_TOTAL_COL] = Some(kit::CellBar {
                    inner: a.total_p50_ms / top,
                    outer: a.total_p99_ms / top,
                    tok: "stage-server",
                });
            }
            row
        })
        .collect()
}

/// An API's slowest requests; clicking one shows its operation's waterfall.
fn api_drill(
    m: &Rc<ProfileModel>,
    api: &str,
    reqs: Vec<ReqLine>,
    opened: Signal<Option<u64>>,
    picked: Signal<Option<String>>,
) -> View {
    let cols = [
        Col {
            heading: "col.key",
            width: 0.0,
            numeric: false,
        },
        Col {
            heading: "col.status",
            width: 56.0,
            numeric: true,
        },
        Col {
            heading: "col.attempts",
            width: 64.0,
            numeric: true,
        },
        Col {
            heading: "col.at",
            width: 64.0,
            numeric: true,
        },
        Col {
            heading: "col.ttfb_only",
            width: 72.0,
            numeric: true,
        },
        Col {
            heading: "col.total",
            width: 72.0,
            numeric: true,
        },
        Col {
            heading: "col.code",
            width: 120.0,
            numeric: false,
        },
    ];
    let rows = reqs
        .iter()
        .map(|q| {
            vec![
                q.key.clone(),
                if q.status == 0 {
                    "—".into()
                } else {
                    q.status.to_string()
                },
                q.attempt.to_string(),
                fmt::clock_s(q.t_us as f64 / 1e6),
                fmt::dur_us(q.ttfb_us as f64),
                fmt::dur_us(q.total_us as f64),
                q.provider_code.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let ids: Vec<u64> = reqs.iter().map(|q| q.op_id).collect();
    let ids_empty = ids.is_empty();
    drill_panel(
        fill("drill.api", &[&api]),
        "api-drill",
        move || picked.set(None),
        if reqs.is_empty() {
            small(tr("drill.none")).into_element()
        } else {
            let ids2 = ids.clone();
            kit::pressable_table(
                &cols,
                rows,
                "api-req",
                move || {
                    let o = opened.get();
                    ids2.iter().position(|id| Some(*id) == o)
                },
                move |i| opened.set(ids.get(i).copied()),
            )
            .into_element()
        },
        !ids_empty,
        opened,
        m.clone(),
    )
}

/// A drill-down: its title and Close, its list, and the waterfall of the operation opened.
fn drill_panel(
    title: String,
    id: &'static str,
    close: impl Fn() + 'static,
    list: AnyElement,
    any: bool,
    opened: Signal<Option<u64>>,
    m: Rc<ProfileModel>,
) -> View {
    kit::panel()
        .p_3()
        .gap_2()
        .mt_1()
        .test_id(id)
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(strong(title).flex_1().min_w(px(0.0)))
                .child(btn(tr("drill.close"), || Kind::Plain, close)),
        )
        .child(any.then(|| small(tr("drill.pick"))))
        .child(list)
        .child(dynamic(move || {
            let o = opened.get();
            let m = m.clone();
            untracked(move || {
                o.and_then(|id| m.waterfall(id))
                    .map(|w| waterfalls(&[w], "drill-waterfall"))
            })
        }))
}

/// "Whole run" beside a section's heading while a range is selected: what it shows is not
/// recomputed for the range.
fn whole_run(cx: Ctx) -> impl IntoElement {
    dynamic(move || {
        let on = cx.range.get().is_some();
        untracked(|| on.then(|| small(tr("range.whole_run")).test_id("whole-run")))
    })
}

/// The capacity the run measured in one line: the limit model and its ceilings.
fn capacity_line(c: &crate::report::view::CapacityView) -> String {
    let mut parts = Vec::new();
    if !c.hypothesis.is_empty() {
        use crate::analysis::capacity::Confidence;
        let key = format!("hyp.{}", c.hypothesis.replace([' ', '-'], "_"));
        parts.push(format!(
            "{} ({})",
            tr_dyn(&key).unwrap_or(&c.hypothesis),
            tr(match c.confidence {
                Confidence::Low => "confidence.low",
                Confidence::Medium => "confidence.medium",
                Confidence::High => "confidence.high",
            })
        ));
    }
    if let Some(r) = c.rate_ceiling_requests_per_s {
        parts.push(fill("capacity.rate", &[&fmt::compact(r)]));
    }
    if let Some((e, lo, hi)) = c.inflight_ceiling {
        parts.push(fill(
            "capacity.inflight",
            &[&e.round(), &lo.round(), &hi.round()],
        ));
    }
    parts.join(" · ")
}

/// Retries and failures, and the capacity the run measured.
fn retries(m: &ProfileModel, cx: Ctx) -> View {
    let r = &m.retry;
    let n = fmt::count;
    section(tr("profile.retries"))
        .test_id("retries")
        .child(whole_run(cx))
        .child(
            m.capacity
                .as_ref()
                .map(|c| prop(tr("capacity.title"), capacity_line(c)).test_id("capacity")),
        )
        .child(prop(tr("retry.retries"), n(r.retries_count)))
        .child((r.tail_cuts_count > 0).then(|| prop(tr("retry.tail_cuts"), n(r.tail_cuts_count))))
        .child(prop(tr("retry.backoff"), fmt::dur_ms(r.backoff_ms)))
        .child(prop(
            tr("retry.throttled"),
            format!(
                "429 ×{} · 503 ×{}",
                n(r.status_429_count),
                n(r.status_503_count)
            ),
        ))
        .child(
            (r.max_retry_after_ms > 0)
                .then(|| prop(tr("retry.after"), fmt::dur_ms(r.max_retry_after_ms as f64))),
        )
        .child(prop(
            tr("retry.errors"),
            fill(
                "retry.errors_split",
                &[
                    &n(r.server_errors_count),
                    &n(r.client_errors_count),
                    &n(r.network_errors_count),
                ],
            ),
        ))
        .child(prop(tr("retry.failed"), n(r.failed_ops_count)))
        .child((!r.classes.is_empty()).then(|| {
            prop(
                tr("retry.classes"),
                r.classes
                    .iter()
                    .take(4)
                    .map(|(c, k)| format!("{c} ×{}", n(*k)))
                    .collect::<Vec<_>>()
                    .join(" · "),
            )
        }))
}

/// The traffic in one line, always shown (the tables are in Details): the remotes and how
/// they answered, the bytes asked for against the bytes moved, the requests per operation.
fn traffic(m: &ProfileModel, cx: Ctx) -> Option<View> {
    let b = &m.bytes;
    let mut parts = Vec::new();
    if let Some(r) = m.spread.remotes_summary() {
        let range = |(lo, hi): (f64, f64)| {
            if (hi - lo).abs() < 1e-9 {
                fmt::dur_ms(hi)
            } else {
                format!("{}–{}", fmt::dur_ms(lo), fmt::dur_ms(hi))
            }
        };
        parts.push(fill(
            if r.remotes_count == 1 {
                "traffic.remote"
            } else {
                "traffic.remotes"
            },
            &[
                &fmt::count(r.remotes_count),
                &fmt::ratio(r.error_ratio),
                &range(r.p50_ms),
                &range(r.p99_ms),
            ],
        ));
    }
    if b.logical_read_bytes + b.backend_down_bytes > 0 {
        parts.push(fill(
            "traffic.read",
            &[
                &fmt::bytes(b.logical_read_bytes),
                &fmt::bytes(b.backend_down_bytes),
                &fmt::sig3(b.read_amplification_ratio),
            ],
        ));
    }
    if b.logical_write_bytes + b.backend_up_bytes > 0 {
        parts.push(fill(
            "traffic.write",
            &[
                &fmt::bytes(b.logical_write_bytes),
                &fmt::bytes(b.backend_up_bytes),
                &fmt::sig3(b.write_amplification_ratio),
            ],
        ));
    }
    if b.requests_per_op > 0.0 {
        parts.push(fill("traffic.per_op", &[&fmt::sig3(b.requests_per_op)]));
    }
    if parts.is_empty() {
        return None;
    }
    Some(
        section(tr("profile.traffic"))
            .test_id("traffic")
            .child(whole_run(cx))
            .child(
                small(parts.join(" · "))
                    .selectable(true)
                    .test_id("traffic-line"),
            ),
    )
}

/// Bytes, and where the traffic went.
fn details(m: &ProfileModel) -> View {
    let b = &m.bytes;
    let mut v = view().test_id("details-body").child(
        view()
            .gap_1()
            .child(caption(tr("details.bytes")))
            .child(prop(
                tr("bytes.read"),
                format!(
                    "{} → {}",
                    fmt::bytes(b.logical_read_bytes),
                    fmt::bytes(b.backend_down_bytes)
                ),
            ))
            .child(prop(
                tr("bytes.write"),
                format!(
                    "{} → {}",
                    fmt::bytes(b.logical_write_bytes),
                    fmt::bytes(b.backend_up_bytes)
                ),
            ))
            .child((b.wire_rx_bytes + b.wire_tx_bytes > 0).then(|| {
                prop(
                    tr("bytes.wire"),
                    format!(
                        "{} / {}",
                        fmt::bytes(b.wire_rx_bytes),
                        fmt::bytes(b.wire_tx_bytes)
                    ),
                )
                .test_id("bytes-wire")
            }))
            .child(
                (b.cache_hit_bytes > 0)
                    .then(|| prop(tr("bytes.cache"), fmt::bytes(b.cache_hit_bytes))),
            )
            .child(m.cache.as_ref().map(|cv| {
                prop(
                    tr("cache.title"),
                    fill(
                        "cache.summary",
                        &[
                            &fmt::ratio(cv.hit_ratio),
                            &fmt::count(cv.hits_count),
                            &fmt::count(cv.lookups_count),
                            &fmt::dur_ms(cv.lookup_mean_ms),
                        ],
                    ),
                )
                .test_id("details-cache")
            }))
            .child(prop(
                tr("bytes.amplification"),
                fill(
                    "bytes.amplification_of",
                    &[
                        &fmt::sig3(b.read_amplification_ratio),
                        &fmt::sig3(b.overread_ratio),
                        &fmt::sig3(b.write_amplification_ratio),
                        &fmt::sig3(b.requests_per_op),
                    ],
                ),
            )),
    );
    let slices = |title: &'static str, id: &'static str, xs: &[crate::report::view::Slice]| {
        // Columns no slice fills (a hot key's errors and p99) are left out.
        let (has_thr, has_err, has_p99) = crate::report::view::SpreadView::columns(xs);
        let throttle_share = xs.iter().any(|x| x.throttle_share_ratio.is_some());
        let mut cols = vec![
            Col {
                heading: "col.key",
                width: 0.0,
                numeric: false,
            },
            Col {
                heading: "col.requests",
                width: 72.0,
                numeric: true,
            },
            Col {
                heading: "col.share",
                width: 64.0 + kit::CELL_BAR_W,
                numeric: true,
            },
        ];
        if has_thr {
            cols.push(Col {
                // Hot keys carry their share of all throttling; the other slices the share of
                // their own requests throttled.
                heading: if throttle_share {
                    "col.throttle_share"
                } else {
                    "col.throttled"
                },
                width: 120.0 + kit::CELL_BAR_W,
                numeric: true,
            });
        }
        if has_err {
            cols.push(Col {
                heading: "col.errors",
                width: 72.0,
                numeric: true,
            });
        }
        if has_p99 {
            cols.push(Col {
                heading: "col.p99",
                width: 72.0,
                numeric: true,
            });
        }
        let dash = || "—".to_owned();
        let rows = xs
            .iter()
            .map(|x| {
                let mut r = vec![
                    slice_label(x),
                    fmt::count(x.requests_count),
                    fmt::ratio(x.share_ratio),
                ];
                if has_thr {
                    r.push(
                        x.throttled_ratio
                            .or(x.throttle_share_ratio)
                            .map(fmt::ratio)
                            .unwrap_or_else(dash),
                    );
                }
                if has_err {
                    r.push(x.error_ratio.map(fmt::ratio).unwrap_or_else(dash));
                }
                if has_p99 {
                    r.push(x.p99_ms.map(fmt::dur_ms).unwrap_or_else(dash));
                }
                r
            })
            .collect();
        let bars = xs
            .iter()
            .map(|x| {
                let mut b = vec![
                    None,
                    None,
                    Some(kit::CellBar::share(x.share_ratio, "series-a")),
                ];
                if has_thr {
                    b.push(
                        x.throttled_ratio
                            .or(x.throttle_share_ratio)
                            .map(|r| kit::CellBar::share(r, "warn")),
                    );
                }
                b
            })
            .collect();
        view()
            .gap_1()
            .test_id(id)
            .child(caption(tr(title)))
            .child(kit::table_bars(&cols, rows, bars))
    };
    let sp = &m.spread;
    for (title, id, xs) in [
        ("details.hot_keys", "hot-keys", &sp.hot_keys),
        ("details.sizes", "sizes", &sp.sizes),
        ("details.prefixes", "prefixes", &sp.prefixes),
        ("details.cold_warm", "cold-warm", &sp.cold_warm),
        ("details.remotes", "remotes", &sp.remotes),
    ] {
        if !xs.is_empty() {
            v = v.child(slices(title, id, xs));
        }
    }
    v
}

/// A traffic slice's key for people: the view model's label (size buckets as ranges), first
/// read / re-read in the app's language.
fn slice_label(x: &crate::report::view::Slice) -> String {
    tr_dyn(&format!("slice.{}", x.key))
        .filter(|_| matches!(x.key.as_str(), "cold" | "warm"))
        .map(str::to_owned)
        .unwrap_or_else(|| x.label.clone())
}

/// How the run was set up, and the command that runs it again.
fn setup(m: &ProfileModel) -> View {
    let s = store();
    let st = &m.setup;
    let t = &m.trust;
    let cmd = st.command.clone();
    view()
        .gap_1()
        .test_id("setup-body")
        .child(prop(tr("setup.target"), st.target.clone()))
        .child(st.endpoint.clone().map(|e| prop(tr("setup.endpoint"), e)))
        .child(prop(tr("setup.workload"), st.preset.clone()))
        .child(prop(
            tr("setup.load"),
            match st.rate_per_s {
                Some(r) => fill("setup.open_loop", &[&fmt::compact(r)]),
                None => fill("setup.closed_loop", &[&st.concurrency_count]),
            },
        ))
        .child(st.open_loop.as_ref().map(|o| {
            prop(
                tr("setup.schedule"),
                fill(
                    "setup.schedule_of",
                    &[
                        &fmt::compact(o.target_per_s),
                        &fmt::compact(o.achieved_per_s),
                        &fmt::count(o.missed_count),
                        &o.lag_p99_ms.map(fmt::dur_ms).unwrap_or_else(|| "—".into()),
                        &fmt::dur_ms(o.max_lag_ms),
                    ],
                ),
            )
            .test_id("setup-schedule")
        }))
        .child(prop(
            tr("setup.duration"),
            fill(
                "setup.duration_of",
                &[&fmt::clock_s(st.duration_s), &fmt::clock_s(st.warmup_s)],
            ),
        ))
        .child((st.objects_count > 0).then(|| {
            prop(
                tr("setup.dataset"),
                format!(
                    "{} × {}",
                    fmt::count(st.objects_count),
                    fmt::bytes(st.object_size_bytes)
                ),
            )
        }))
        .child((!st.mix.is_empty()).then(|| {
            prop(
                tr("setup.mix"),
                st.mix
                    .iter()
                    .map(|(w, o)| format!("{w}× {o}"))
                    .collect::<Vec<_>>()
                    .join(" · "),
            )
        }))
        .child(
            m.write_path
                .as_ref()
                .filter(|w| w.shown())
                .map(|w| prop(tr("write.path"), write_line(w))),
        )
        .child(prop(tr("setup.seed"), st.seed.to_string()))
        .child(prop(tr("setup.started"), st.started_at.clone()))
        .child(prop(
            tr("setup.client"),
            format!(
                "{} · oprobe {} · CPU {} · lag p99 {}",
                st.host,
                st.oprobe_version,
                t.cpu_ratio.map(fmt::ratio).unwrap_or_else(|| "—".into()),
                t.sched_lag_p99_ms
                    .map(fmt::dur_ms)
                    .unwrap_or_else(|| "—".into())
            ),
        ))
        .child(prop(tr("setup.kept"), fmt::ratio(t.kept_ratio)))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .pt_1()
                .child(
                    kit::panel()
                        .px_2()
                        .py_1()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(mono(st.command.clone()).selectable(true)),
                )
                .child(btn(
                    tr("setup.copy"),
                    || Kind::Plain,
                    move || s.copy(cmd.clone()),
                )),
        )
}

/// One bar of every stage's share, then each stage with its share.
pub fn stage_bar(stages: &[crate::ui::data::StageBar]) -> View {
    let total: f64 = stages.iter().map(|s| s.ratio).sum::<f64>().max(1e-9);
    view()
        .flex_row()
        .test_id("stage-bar")
        .h(px(kit::bar_h(10.0)))
        .rounded(px(3.0))
        .overflow_hidden()
        .bg(move || theme().muted)
        .children(stages.iter().filter(|s| s.ratio > 0.0).map(|s| {
            view()
                .h_full()
                .w(relative((s.ratio / total) as f32))
                .bg(token(stage_token(&s.key)))
        }))
}

/// A stage's name in the app's language.
fn stage_name(key: &str) -> String {
    tr_dyn(&format!("stage.{key}"))
        .map(str::to_owned)
        .unwrap_or_else(|| key.to_owned())
}

/// Each stage's share as a row: its swatch, name, a meter and the share.
fn stage_rows(stages: &[crate::ui::data::StageBar]) -> Vec<View> {
    let max = stages.iter().map(|s| s.ratio).fold(0.0, f64::max).max(1e-9);
    stages
        .iter()
        .filter(|s| s.ratio >= 0.001)
        .map(|st| {
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(swatch(stage_token(&st.key)))
                .child(
                    body(stage_name(&st.key))
                        .w(px(if kit::in_terminal() { 136.0 } else { 150.0 }))
                        .number_of_lines(1),
                )
                .child(meter(st.ratio / max, stage_token(&st.key)))
                .child(small(fmt::ratio(st.ratio)).w(px(64.0)).tabular_nums())
        })
        .collect()
}

/// Where the time went: every operation's stages (of the range when one is selected), and
/// the slowest 1 %'s real split as a second bar (the whole run's).
fn time_went(m: &ProfileModel, cx: Ctx) -> View {
    let all = m.stages.clone();
    let tail = m.tail_stages.clone();
    view().test_id("time").child(dynamic(move || {
        let r = cx.range.get();
        let stages: Vec<crate::ui::data::StageBar> = match &r {
            Some(r) if !r.view.stages.is_empty() => r
                .view
                .stages
                .iter()
                .map(|s| crate::ui::data::StageBar {
                    key: s.key.clone(),
                    ratio: s.ratio,
                })
                .collect(),
            _ => all.clone(),
        };
        let ranged = r.is_some();
        let tail = tail.clone();
        untracked(move || {
            section(tr("profile.time"))
                .child(stage_bar(&stages))
                .children(stage_rows(&stages))
                .child((!tail.is_empty()).then(|| {
                    view()
                        .gap_1()
                        .pt_2()
                        .test_id("time-tail")
                        .child(caption(if ranged {
                            tr("profile.time_tail_whole").to_owned()
                        } else {
                            tr("profile.time_tail").to_owned()
                        }))
                        .child(stage_bar(&tail))
                        .children(stage_rows(&tail))
                }))
                .into_element()
        })
    }))
}

fn findings(s: Store, m: &ProfileModel) -> View {
    let sec = section(fill("profile.findings", &[&m.findings.len()]));
    if m.findings.is_empty() {
        return sec.child(small(tr("profile.no_findings")));
    }
    sec.children(m.findings.iter().map(|f| finding(s, m, f.clone())))
}

/// Each verb: its numbers and its own stage split.
fn verbs(vs: &[VerbRow]) -> View {
    // On a terminal every part has its width, so a verb stays one row with its bar.
    let term = kit::in_terminal();
    section(tr("profile.verbs"))
        .test_id("verbs")
        .children(vs.iter().map(move |v| {
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_3()
                .py(px(if term { 0.0 } else { 4.0 }))
                .child(mono(v.verb.clone()).w(px(150.0)).number_of_lines(1))
                .child(
                    small(fill("verb.ops", &[&fmt::count(v.ops_count)]))
                        .w(px(72.0))
                        .tabular_nums(),
                )
                .child(
                    small(format!(
                        "p50 {} · p99 {}",
                        fmt::dur_ms(v.p50_ms),
                        fmt::dur_ms(v.p99_ms)
                    ))
                    .w(px(if crate::ui::kit::in_terminal() {
                        27.0 * 8.0
                    } else {
                        170.0
                    }))
                    .tabular_nums(),
                )
                // Its rates: operations and bytes per second, requests per operation.
                .child({
                    let rates = small(fill(
                        "verb.rates",
                        &[
                            &fmt::compact(v.ops_per_s),
                            &fmt::sig3(v.mib_per_s),
                            &fmt::sig3(v.requests_per_op),
                        ],
                    ))
                    .tabular_nums();
                    if term {
                        rates.w(px(44.0 * 8.0)).flex_shrink_0()
                    } else {
                        rates
                    }
                })
                .child(if term {
                    view().w(px(20.0 * 8.0)).child(stage_bar(&v.stages))
                } else {
                    view().flex_1().min_w(px(120.0)).child(stage_bar(&v.stages))
                })
                .child((v.throttled_ratio > 0.0).then(|| {
                    small(fill("verb.throttled", &[&fmt::ratio(v.throttled_ratio)]))
                        .text_color(move || Tone::Warn.color())
                }))
                .child((v.error_ratio > 0.0).then(|| {
                    small(format!(
                        "{} {}",
                        tr("metric.errors"),
                        fmt::ratio(v.error_ratio)
                    ))
                    .text_color(move || Tone::Crit.color())
                }))
        }))
}

/// The evidence chart of a finding (nothing when the run lacks its data).
fn evidence(m: &ProfileModel, f: &FindingRow) -> Option<View> {
    use crate::report::view::EvidenceChart;
    match f.chart? {
        EvidenceChart::HotKeys if !plots::hot_key_bars(m).1.is_empty() => {
            // Each hot key as a horizontal bar of its share of all throttling (of all requests
            // when nothing was throttled).
            let (throttle, bars) = plots::hot_key_bars(m);
            let label_w = if kit::in_terminal() { 168.0 } else { 190.0 };
            let tok = if throttle { "warn" } else { "series-a" };
            return Some(
                view()
                    .gap_1()
                    .pt_1()
                    .test_id(&format!("evidence-{}", f.code))
                    .child(caption(if throttle {
                        tr("plot.hot_keys_throttling")
                    } else {
                        tr("plot.hot_keys_requests")
                    }))
                    .children(bars.into_iter().enumerate().map(move |(i, (key, share))| {
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .test_id(&format!("hot-key-bar-{i}"))
                            .child(mono(key).w(px(label_w)).number_of_lines(1))
                            .child(meter(share, tok))
                            .child(small(fmt::ratio(share)).w(px(64.0)).tabular_nums())
                    })),
            );
        }
        EvidenceChart::Amplification => {
            let b = &m.bytes;
            return Some(
                view()
                    .gap_1()
                    .pt_1()
                    .test_id(&format!("evidence-{}", f.code))
                    .child(caption(tr("details.bytes")))
                    .child(prop(
                        tr("bytes.read"),
                        format!(
                            "{} → {}",
                            fmt::bytes(b.logical_read_bytes),
                            fmt::bytes(b.backend_down_bytes)
                        ),
                    ))
                    .child(prop(
                        tr("bytes.amplification"),
                        fill(
                            "bytes.amplification_of",
                            &[
                                &fmt::sig3(b.read_amplification_ratio),
                                &fmt::sig3(b.overread_ratio),
                                &fmt::sig3(b.write_amplification_ratio),
                                &fmt::sig3(b.requests_per_op),
                            ],
                        ),
                    )),
            );
        }
        _ => {}
    }
    let (heading, plot) = plots::evidence(m, f.chart?)?;
    Some(
        view()
            .pt_1()
            .test_id(&format!("evidence-{}", f.code))
            .child(chart(heading, 96.0, move || Some(plot.clone()))),
    )
}

fn finding(s: Store, m: &ProfileModel, f: FindingRow) -> View {
    let remedy = f.remedy.clone();
    let code = f.code.clone();
    let paths = remedy
        .iter()
        .map(|(p, v)| {
            if v.is_empty() {
                p.clone()
            } else {
                format!("{p} = {v}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sev = f.severity;
    view()
        .gap_1()
        .py_2()
        .border_t_1()
        .border_color(move || theme().border)
        .test_id(&format!("finding-{}", f.code))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(badge(
                    tr_dyn(&format!("severity.{}", sev.label())).unwrap_or(sev.label()),
                    move || Tone::of_severity(sev),
                ))
                .child(strong(finding_title(&f.code)).flex_1().min_w(px(0.0)))
                .child(mono(f.code.clone()).text_color(move || theme().muted_foreground)),
        )
        .child(f.card.cause.clone().map(|c| body(c).selectable(true)))
        .child(f.card.evidence.clone().map(|e| small(e).selectable(true)))
        .children(
            f.items
                .iter()
                .map(|i| prop(evidence_label(i), evidence_value(i))),
        )
        .child(evidence(m, &f))
        .child(examples(m, &f))
        .child(if remedy.is_empty() {
            view()
                .flex_row()
                .gap_2()
                .child(caption(tr("profile.advice")))
                .child(small(f.advice.clone()).flex_1().selectable(true))
        } else {
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(caption(tr("profile.remedy")))
                .child(mono(paths).flex_1().min_w(px(0.0)))
                .child(
                    btn(
                        tr("profile.try_remedy"),
                        || Kind::Plain,
                        move || s.prove_remedy(&code),
                    )
                    .accessibility_label(fill("profile.try_remedy_of", &[&f.code])),
                )
        })
}

/// An evidence value's label in the app's language (core's label when the catalog has none).
pub fn evidence_label(i: &crate::report::view::EvidenceItem) -> String {
    tr_dyn(&format!("ev.{}", i.key))
        .map(str::to_owned)
        .unwrap_or_else(|| i.label.clone())
}

/// An evidence value in the tiles' format (`9.67 ms`, `2.60 %`, `3.20×`).
pub fn evidence_value(i: &crate::report::view::EvidenceItem) -> String {
    use crate::report::view::EvidenceUnit as U;
    let a = i.amount;
    match i.unit {
        U::Ms => fmt::dur_ms(a),
        U::Seconds => fmt::dur_ms(a * 1000.0),
        U::Share => fmt::ratio(a),
        U::Times => format!("{}×", fmt::sig3(a)),
        U::Bytes => fmt::bytes(a.max(0.0) as u64),
        U::BytesPerS => format!("{}/s", fmt::bytes(a.max(0.0) as u64)),
        U::MibPerS => format!("{} MiB/s", fmt::sig3(a)),
        U::RequestsPerS | U::OpsPerS => format!("{}/s", fmt::compact(a)),
        U::Count if a.fract() == 0.0 && a >= 0.0 => fmt::count(a as u64),
        U::Usd => format!("${a:.2}"),
        U::Count | U::Number => fmt::sig3(a),
    }
}

/// "Show N examples": the waterfalls of the operations behind a finding, on demand.
fn examples(m: &ProfileModel, f: &FindingRow) -> Option<View> {
    if f.op_ids.is_empty() || !m.has_rows() {
        return None;
    }
    let ws = Rc::new(m.examples(&f.op_ids, 5));
    if ws.is_empty() {
        return None;
    }
    let open = signal(false);
    let n = ws.len();
    let code = f.code.clone();
    let id = format!("examples-{code}");
    Some(
        view()
            .gap_1()
            .child(
                btn(
                    move || -> SharedString {
                        if open.get() {
                            tr("profile.hide_examples").into()
                        } else {
                            fill("profile.examples", &[&n]).into()
                        }
                    },
                    || Kind::Plain,
                    move || open.update(|o| *o = !*o),
                )
                .test_id(&id)
                .self_start(),
            )
            .child(dynamic(move || {
                let o = open.get();
                let ws = ws.clone();
                untracked(move || o.then(|| waterfalls(&ws, "examples")))
            })),
    )
}

/// Each verb: its numbers and its own stage split (of the range when one is selected).
fn verbs_section(m: &ProfileModel, cx: Ctx) -> Option<View> {
    if m.verbs.len() < 2 {
        return None;
    }
    let all = m.verbs.clone();
    Some(view().child(dynamic(move || {
        let r = cx.range.get();
        let vs = r.map(|r| r.verbs.clone()).unwrap_or_else(|| all.clone());
        untracked(|| verbs(&vs).into_element())
    })))
}

/// The slowest operations, request by request (of the range when one is selected).
fn slowest_section(m: &ProfileModel, cx: Ctx) -> View {
    let all = m.slowest.clone();
    view().child(dynamic(move || {
        let r = cx.range.get();
        let ws = r.map(|r| r.slowest.clone()).unwrap_or_else(|| all.clone());
        untracked(|| {
            section(tr("profile.slowest"))
                .child(waterfalls(&ws, "slowest"))
                .into_element()
        })
    }))
}

/// The timeline: a verb filter, then goodput, latency, throughput, requests, the problems per
/// second (when any) and in-flight on one time axis, sharing the crosshair and the selected
/// range.
fn timeline(m: &Rc<ProfileModel>, cx: Ctx) -> View {
    if m.timeline.is_empty() {
        return view();
    }
    let verbs = m.timeline_verbs.clone();
    let m = m.clone();
    let filter = (verbs.len() > 1).then(|| {
        let mut opts: Vec<(usize, SharedString)> = vec![(0, tr("timeline.every_verb").into())];
        opts.extend(
            verbs
                .iter()
                .enumerate()
                .map(|(i, v)| (i + 1, SharedString::from(v.clone()))),
        );
        view()
            .flex_row()
            .test_id("verb-filter")
            .label(tr("timeline.verb_filter"))
            .child(kit::segmented(
                opts,
                move || cx.verb.get(),
                move |i| cx.verb.set(i),
            ))
    });
    section(tr("profile.timeline"))
        .test_id("timeline")
        .child(filter)
        .child(dynamic(move || {
            let i = cx.verb.get();
            let m = m.clone();
            untracked(move || timeline_charts(&m, i, cx).into_element())
        }))
}

fn timeline_charts(m: &ProfileModel, i: usize, cx: Ctx) -> View {
    let verb = i
        .checked_sub(1)
        .and_then(|i| m.timeline_verbs.get(i))
        .cloned();
    let t = m.timeline_for(verb.as_deref());
    let all = &m.timeline;
    // Series the timeline does not split by verb say so.
    let every = |key: &'static str| -> String {
        if verb.is_some() {
            fill("timeline.all_verbs", &[&tr(key)])
        } else {
            tr(key).to_owned()
        }
    };
    let goodput_heading = match &verb {
        Some(v) => format!("{} · {v}", tr("metric.goodput")),
        None => tr("metric.goodput").to_owned(),
    };
    let latency_heading = match &verb {
        Some(v) => format!("{} · {v}", tr("profile.latency")),
        None => tr("profile.latency").to_owned(),
    };
    let link = cx.link;
    let layout = m.layout;
    // Every operation moved the same bytes: throughput is goodput's right axis, not a chart
    // of the same shape. A verb's goodput is not every operation's: then it has its chart.
    let on_goodput = match layout.throughput {
        ThroughputChart::OnGoodput { mib_per_op } => Some(mib_per_op),
        _ => None,
    };
    let target = m.setup.open_loop.as_ref().map(|o| o.target_per_s);
    let goodput = plots::goodput(&t).map(|p| {
        let p = match on_goodput.filter(|_| verb.is_none()) {
            Some(k) => plots::with_throughput_axis(p, k),
            None => p,
        };
        match target.filter(|_| verb.is_none()) {
            Some(r) => plots::with_rule(p, r, "axis", fill("plot.target", &[&fmt::compact(r)])),
            None => p,
        }
    });
    let cap = m.capacity.clone();
    let rate_ceiling = cap.as_ref().and_then(|c| c.rate_ceiling_requests_per_s);
    let inflight_ceiling = cap.as_ref().and_then(|c| c.inflight_ceiling);
    view()
        .gap_2()
        .child(goodput.map(|p| {
            linked_chart(
                goodput_heading,
                110.0,
                move || Some(p.clone()),
                link,
                "chart-goodput",
            )
        }))
        .child(plots::latency(&t).map(|p| {
            linked_chart(
                latency_heading,
                110.0,
                move || Some(p.clone()),
                link,
                "chart-latency",
            )
        }))
        .child(
            plots::throughput(all)
                .filter(|_| {
                    layout.throughput == ThroughputChart::Own
                        || (verb.is_some() && on_goodput.is_some())
                })
                .map(|p| {
                    linked_chart(
                        every("timeline.throughput"),
                        90.0,
                        move || Some(p.clone()),
                        link,
                        "chart-throughput",
                    )
                }),
        )
        .child(
            plots::requests(all)
                .filter(|_| layout.requests || rate_ceiling.is_some())
                .map(|p| {
                    let p = match rate_ceiling {
                        Some(r) => plots::with_rule(
                            p,
                            r,
                            "warn",
                            fill("plot.rate_ceiling", &[&fmt::compact(r)]),
                        ),
                        None => p,
                    };
                    linked_chart(
                        every("timeline.requests"),
                        90.0,
                        move || Some(p.clone()),
                        link,
                        "chart-requests",
                    )
                }),
        )
        .child(plots::problems(all).map(|p| {
            linked_chart(
                every("timeline.problems"),
                70.0,
                move || Some(p.clone()),
                link,
                "chart-problems",
            )
        }))
        .child(plots::inflight(all).map(|p| {
            let p = match inflight_ceiling {
                Some((e, _, _)) => {
                    plots::with_rule(p, e, "warn", fill("plot.limit", &[&e.round()]))
                }
                None => p,
            };
            linked_chart(
                every("plot.inflight"),
                90.0,
                move || Some(p.clone()),
                link,
                "chart-inflight",
            )
        }))
        .child(plots::cpu(all).filter(|_| layout.cpu).map(|p| {
            linked_chart(
                every("plot.cpu"),
                70.0,
                move || Some(p.clone()),
                link,
                "chart-cpu",
            )
        }))
}

/// The latency distribution with its median and tail marked: of the selected range, else of
/// the verb the timeline shows, else of every operation.
fn distribution(m: &Rc<ProfileModel>, cx: Ctx) -> View {
    let m = m.clone();
    view().child(dynamic(move || {
        let r = cx.range.get();
        let i = cx.verb.get();
        let m = m.clone();
        untracked(move || {
            let verb = i
                .checked_sub(1)
                .and_then(|i| m.timeline_verbs.get(i))
                .cloned();
            let ranged_verb = match (&r, &verb) {
                (Some(r), Some(v)) => m.range_cdf_for(r.view.since_s, r.view.until_s, v),
                _ => None,
            };
            let (cdf, marks) = match (&r, &verb) {
                (Some(_), Some(_)) if ranged_verb.is_some() => {
                    let c = ranged_verb.unwrap_or_default();
                    let marks = plots::Marks::of(&c);
                    (c, marks)
                }
                (Some(r), _) => (
                    r.view.cdf.clone(),
                    plots::Marks {
                        p50_ms: r.view.p50_ms,
                        p99_ms: r.view.p99_ms,
                        p999_ms: r.view.p999_ms,
                    },
                ),
                (None, Some(v)) => {
                    let c = m.cdf_for(Some(v));
                    let marks = plots::Marks::of(&c);
                    (c, marks)
                }
                (None, None) => (
                    m.cdf.clone(),
                    plots::Marks {
                        p50_ms: m.p50_ms,
                        p99_ms: m.p99_ms,
                        p999_ms: m.p999_ms,
                    },
                ),
            };
            let Some(p) = plots::tail(&[(tr("plot.ops").to_owned(), &cdf)], Some(marks)) else {
                return view().into_element();
            };
            let mut heading = if cdf.exact {
                tr("plot.tail").to_owned()
            } else {
                tr("plot.tail_buckets").to_owned()
            };
            if let Some(v) = &verb
                && (r.is_none() || cdf.exact)
            {
                heading = format!("{heading} · {v}");
            }
            section(tr("profile.distribution"))
                .test_id("distribution")
                .child(chart(heading, 150.0, move || Some(p.clone())))
                .into_element()
        })
    }))
}

/// The heatmap, linked to the timeline; a click on a cell lists its slowest operations.
fn heat_section(m: &Rc<ProfileModel>, cx: Ctx) -> Option<View> {
    let all = m.heat.clone()?;
    let m = m.clone();
    let mh = m.clone();
    let cell = signal(None::<Cell>);
    let opened = signal(None::<u64>);
    let on_cell: Option<Rc<dyn Fn(Cell)>> = m.has_rows().then(|| {
        Rc::new(move |c: Cell| {
            opened.set(None);
            cell.set(Some(c));
        }) as Rc<dyn Fn(Cell)>
    });
    Some(
        section("")
            .child(dynamic(move || {
                // The verb the timeline is filtered to (the whole run's heatmap when the rows
                // are sampled: it says so).
                let i = cx.verb.get();
                let verb = i
                    .checked_sub(1)
                    .and_then(|i| mh.timeline_verbs.get(i))
                    .cloned();
                let (h, heading) = match verb
                    .as_deref()
                    .and_then(|v| mh.heat_for(Some(v)).map(|h| (h, v)))
                {
                    Some((h, v)) => (h, format!("{} · {v}", tr("profile.heatmap"))),
                    None if verb.is_some() => (
                        all.clone(),
                        fill("timeline.all_verbs", &[&tr("profile.heatmap")]),
                    ),
                    None => (all.clone(), tr("profile.heatmap").to_owned()),
                };
                let on_cell = on_cell.clone();
                untracked(move || heatmap(heading, 150.0, h, Some(cx.link), on_cell).into_element())
            }))
            .child(dynamic(move || {
                let c = cell.get();
                let m = m.clone();
                untracked(move || {
                    c.map(|c| {
                        let ops = m.cell_ops(c.t0_s, c.t1_s, c.lo_ms, c.hi_ms);
                        cell_drill(&m, c, ops, opened, cell)
                    })
                })
            })),
    )
}

/// The slowest operations of a heatmap cell; clicking one shows its waterfall.
fn cell_drill(
    m: &Rc<ProfileModel>,
    c: Cell,
    ops: Vec<OpLine>,
    opened: Signal<Option<u64>>,
    cell: Signal<Option<Cell>>,
) -> View {
    let cols = [
        Col {
            heading: "col.key",
            width: 0.0,
            numeric: false,
        },
        Col {
            heading: "col.verb",
            width: 110.0,
            numeric: false,
        },
        Col {
            heading: "col.latency",
            width: 80.0,
            numeric: true,
        },
        Col {
            heading: "col.status",
            width: 56.0,
            numeric: true,
        },
        Col {
            heading: "col.attempts",
            width: 64.0,
            numeric: true,
        },
    ];
    let rows = ops
        .iter()
        .map(|o| {
            vec![
                o.key.clone(),
                o.verb.clone(),
                fmt::dur_us(o.latency_us as f64),
                if o.status == 0 {
                    "—".into()
                } else {
                    o.status.to_string()
                },
                o.attempts.to_string(),
            ]
        })
        .collect();
    let hi = if c.hi_ms.is_finite() {
        fmt::dur_ms(c.hi_ms)
    } else {
        "∞".into()
    };
    let ids: Vec<u64> = ops.iter().map(|o| o.op_id).collect();
    let ids_empty = ids.is_empty();
    drill_panel(
        fill(
            "drill.cell",
            &[&fmt::clock_s(c.t0_s), &fmt::dur_ms(c.lo_ms), &hi],
        ),
        "cell-drill",
        move || cell.set(None),
        if ops.is_empty() {
            small(tr("drill.none")).into_element()
        } else {
            let ids2 = ids.clone();
            kit::pressable_table(
                &cols,
                rows,
                "cell-op",
                move || {
                    let o = opened.get();
                    ids2.iter().position(|id| Some(*id) == o)
                },
                move |i| opened.set(ids.get(i).copied()),
            )
            .into_element()
        },
        !ids_empty,
        opened,
        m.clone(),
    )
}

/// Waterfalls on one time axis with a hover readout of each request: its stages, the wait
/// before it, and what the response and connection said.
fn waterfalls(ws: &[Waterfall], id: &'static str) -> View {
    let hover = signal(None::<String>);
    let span = ws.iter().map(|w| w.latency_us).max().unwrap_or(1).max(1) as f64;
    view()
        .gap_1()
        .test_id(id)
        .child(
            small(move || {
                SharedString::from(
                    hover
                        .get()
                        .unwrap_or_else(|| tr("profile.hover").to_owned()),
                )
            })
            .tabular_nums()
            .number_of_lines(2),
        )
        .children(ws.iter().map(|w| waterfall(w, span, hover)))
        .child(
            view().flex_row().flex_wrap().gap_3().children(
                [
                    "queue", "connect", "send", "ttfb", "body", "stall", "client", "retry",
                ]
                .map(|k| {
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(swatch(stage_token(k)))
                        .child(small(tr_dyn(&format!("req.{k}")).unwrap_or(k)))
                }),
            ),
        )
}

/// What the hover readout says of one request.
fn bar_readout(b: &crate::ui::data::Bar) -> String {
    let mut s = format!(
        "{}{} · {}",
        b.label,
        if b.backoff_us > 0 {
            format!(
                " · {} {}",
                tr("req.retry"),
                fmt::dur_us(b.backoff_us as f64)
            )
        } else {
            String::new()
        },
        b.segs
            .iter()
            .map(|s| format!(
                "{} {}",
                tr_dyn(&format!("req.{}", s.stage)).unwrap_or(s.stage),
                fmt::dur_us(s.us as f64)
            ))
            .collect::<Vec<_>>()
            .join(" · ")
    );
    for (k, v) in &b.signals {
        let shown_v = match (*k, v.as_str()) {
            ("tail_cut", "hedged") => tr("sig_val.hedged"),
            ("aborted", "incomplete") => tr("sig_val.incomplete"),
            _ => v.as_str(),
        };
        s += &format!(" · {} {shown_v}", tr_dyn(&format!("sig.{k}")).unwrap_or(k));
    }
    if let Some(err) = &b.error {
        s += &format!(" · {} {err}", tr("sig.error"));
    }
    if let Some(req_id) = &b.request_id {
        s += &format!(" · {} {req_id}", tr("sig.request_id"));
    }
    s
}

fn waterfall(w: &Waterfall, span: f64, hover: Signal<Option<String>>) -> View {
    let label_w = if crate::ui::kit::in_terminal() {
        168.0
    } else {
        190.0
    };
    view()
        .gap(px(2.0))
        .pt_1()
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(
                    strong(format!("{} {}", w.verb, fmt::dur_us(w.latency_us as f64)))
                        .flex_shrink_0(),
                )
                .child(
                    small(w.key.clone())
                        .number_of_lines(1)
                        .flex_1()
                        .min_w(px(0.0)),
                ),
        )
        .children(w.bars.iter().map(|b| {
            let readout = bar_readout(b);
            let readout_focus = readout.clone();
            let readout_a11y = readout.clone();
            let total = b.total_us.max(1) as f64;
            let failed = b.failed;
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
                .rounded(px(2.0))
                .focusable(true)
                .focus_visible(kit::ring)
                .role(AccessibilityRole::Button)
                .label(readout_a11y)
                .on_focus(move || hover.set(Some(readout_focus.clone())))
                .on_blur(move || hover.set(None))
                .on_mouse_enter(move |_: &MouseEvent| hover.set(Some(readout.clone())))
                .on_mouse_leave(move |_: &MouseEvent| hover.set(None))
                .child(
                    small(b.label.clone())
                        .w(px(label_w))
                        .flex_shrink_0()
                        .number_of_lines(1)
                        .text_color(move || {
                            if failed {
                                Tone::Crit.color()
                            } else {
                                theme().muted_foreground
                            }
                        }),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .flex_1()
                        .h(px(kit::bar_h(10.0)))
                        .min_w(px(0.0))
                        // Before the request: the backoff it waited since the previous attempt,
                        // as a thin line in the retry colour.
                        .child(view().w(relative(
                            ((b.start_us.saturating_sub(b.backoff_us)) as f64 / span).min(1.0)
                                as f32,
                        )))
                        .child((b.backoff_us > 0).then(|| {
                            view()
                                .h(px(kit::bar_h(2.0)))
                                .w(relative((b.backoff_us as f64 / span).min(1.0) as f32))
                                .bg(token("stage-retry"))
                        }))
                        .child(
                            view()
                                .flex_row()
                                .h_full()
                                .w(relative((total / span).clamp(0.002, 1.0) as f32))
                                .rounded(px(2.0))
                                .overflow_hidden()
                                .children(b.segs.iter().map(|s| {
                                    view()
                                        .h_full()
                                        .w(relative((s.us as f64 / total) as f32))
                                        .bg(token(stage_token(s.stage)))
                                })),
                        ),
                )
                .child(
                    small(fmt::dur_us(b.total_us as f64))
                        .w(px(64.0))
                        .tabular_nums(),
                )
        }))
        .child(w.await_tree.clone().map(await_tree))
}

/// The async call stack captured while a slow operation was in flight, under its waterfall;
/// closed until asked.
fn await_tree(tree: String) -> View {
    let open = signal(false);
    view()
        .gap_1()
        .test_id("await-tree")
        .child(
            button()
                .on_press(move || open.update(|o| *o = !*o))
                .accessibility_label(move || Some(SharedString::from(tr("profile.await_tree"))))
                .test_id("await-tree-toggle")
                .flex_row()
                .items_center()
                .gap_1()
                .self_start()
                .rounded(px(4.0))
                .child(small(move || {
                    SharedString::from(format!(
                        "{} {}",
                        if open.get() { "▾" } else { "▸" },
                        tr("profile.await_tree")
                    ))
                })),
        )
        .child(dynamic(move || {
            let o = open.get();
            let tree = tree.clone();
            untracked(move || {
                o.then(|| {
                    kit::panel()
                        .px_2()
                        .py_1()
                        .test_id("await-tree-body")
                        .child(mono(tree).selectable(true))
                })
            })
        }))
}

/// The connections (core's [`crate::report::view::ConnectionView`]): reuse, dials and their
/// phases (proxy CONNECT included), and whether the network or the server is the limit — the
/// kernel's round trip, retransmits and window limits, the TLS handshakes, the cold start.
fn conns(m: &ProfileModel, cx: Ctx) -> View {
    let c = &m.conns;
    let p50_p99 = |a: f64, b: f64| format!("p50 {} · p99 {}", fmt::dur_ms(a), fmt::dur_ms(b));
    section(tr("profile.connections"))
        .test_id("connections")
        .child(whole_run(cx))
        .child(
            prop(
                tr("conn.dialed"),
                if !c.observed {
                    tr("conn.not_observed").to_owned()
                } else if c.warmup_dialed_count > 0 {
                    fill(
                        "conn.dialed_of",
                        &[
                            &fmt::count(c.warmup_dialed_count),
                            &fmt::count(c.dialed_count),
                        ],
                    )
                } else {
                    fill("conn.dialed_window", &[&fmt::count(c.dialed_count)])
                },
            )
            .test_id("conn-dialed"),
        )
        .child(
            (c.dial_failures_count > 0)
                .then(|| prop(tr("conn.dial_failures"), fmt::count(c.dial_failures_count))),
        )
        .child(prop(
            tr("conn.reuse"),
            if c.observed {
                fmt::ratio(c.reuse_ratio)
            } else {
                tr("conn.not_observed").to_owned()
            },
        ))
        .child(prop(
            tr("conn.protocols"),
            [
                ("HTTP/1.1", c.http1_requests_count),
                ("HTTP/2", c.http2_requests_count),
            ]
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(p, n)| format!("{p} {}", fmt::count(*n)))
            .collect::<Vec<_>>()
            .join(" · "),
        ))
        .child((!c.tls.is_empty()).then(|| prop("TLS", c.tls.clone())))
        .child(c.proxy.clone().map(|p| prop(tr("conn.proxy"), p)))
        .child(prop(tr("conn.peak"), c.peak_inflight_count.to_string()))
        .children(c.dial.iter().map(|d| {
            prop(
                if d.key == "proxy" {
                    tr("conn.proxy_connect").to_owned()
                } else {
                    d.key.to_uppercase()
                },
                p50_p99(d.p50_ms, d.p99_ms),
            )
        }))
        .child(c.tcp.as_ref().map(|t| {
            view()
                .gap_1()
                .test_id("conn-tcp")
                .child(t.min_rtt_p50_us.map(|us| {
                    prop(tr("conn.min_rtt"), fmt::dur_us(us as f64)).test_id("conn-min-rtt")
                }))
                .child(prop(
                    tr("conn.rtt"),
                    format!(
                        "p50 {} · p99 {}",
                        fmt::dur_us(t.rtt_p50_us as f64),
                        fmt::dur_us(t.rtt_p99_us as f64)
                    ),
                ))
                // The smoothed round trip of a fast network is mostly the server's delay and
                // delayed acknowledgements: say so rather than let it read as the network.
                .child(
                    t.min_rtt_p50_us
                        .filter(|m| t.rtt_p50_us > m.saturating_mul(4) && t.rtt_p50_us > 1000)
                        .map(|_| small(tr("conn.rtt_inflated")).test_id("conn-rtt-note")),
                )
                .child(prop(
                    tr("conn.retransmitting"),
                    fill(
                        "conn.retransmitting_of",
                        &[
                            &fmt::count(t.retransmitting_connections_count),
                            &fmt::count(t.connections_count),
                            &fmt::count(t.retransmits_count),
                        ],
                    ),
                ))
                .child(prop(
                    tr("conn.limited"),
                    fill(
                        "conn.limited_of",
                        &[
                            &fmt::ratio(t.rwnd_limited_ratio),
                            &fmt::ratio(t.sndbuf_limited_ratio),
                        ],
                    ),
                ))
                .child((t.delivery_rate_p50_bits_per_s > 0).then(|| {
                    prop(
                        tr("conn.delivery"),
                        crate::report::view::fmt_bits(t.delivery_rate_p50_bits_per_s),
                    )
                }))
        }))
        .child(c.tls_handshakes.as_ref().map(|t| {
            prop(
                tr("conn.tls_handshakes"),
                fill(
                    "conn.tls_handshakes_of",
                    &[
                        &fmt::count(t.handshakes_count),
                        &fmt::ratio(t.resumption_ratio),
                        &fmt::count(t.hrr_count),
                    ],
                ),
            )
        }))
        .child(c.cold_start.as_ref().map(|s| {
            prop(
                tr("conn.cold_start"),
                fill(
                    "conn.cold_start_of",
                    &[
                        &p50_p99(s.connect_p50_ms, s.connect_p99_ms),
                        &p50_p99(s.first_ops_p50_ms, s.first_ops_p99_ms),
                    ],
                ),
            )
        }))
}
