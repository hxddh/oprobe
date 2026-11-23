//! Profile: one run (`App::show`): the verdict, the key numbers, where the time went (and per
//! verb), the findings with their remedy path and the chart of their evidence, the request
//! waterfall of the slowest operations, the timeline (throttled and failed seconds shaded),
//! the latency distribution, the heatmap and the connections.

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::ui::chart::chart;
use crate::ui::data::{FindingRow, ProfileModel, VerbRow, Waterfall};
use crate::ui::fmt;
use crate::ui::heatmap::heatmap;
use crate::ui::i18n::{finding_title, tr, tr_dyn};
use crate::ui::kit::{
    self, Kind, Tone, badge, body, btn, caption, empty, meter, mono, prop, section, small,
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

fn content(s: Store, m: Rc<ProfileModel>) -> View {
    let mut v = view()
        .test_id("profile")
        .child(head(s, &m))
        .child(trust_notice(&m))
        .child(tiles(&m))
        .child(findings(s, &m))
        .child(time_went(&m))
        .child((m.verbs.len() > 1).then(|| verbs(&m.verbs)))
        .child(requests(&m))
        .child((!m.retry.quiet()).then(|| retries(&m)));
    if !m.slowest.is_empty() {
        v = v.child(slowest(&m.slowest));
    }
    let m2 = m.clone();
    let m3 = m.clone();
    v.child(timeline(&m))
        .child(distribution(&m))
        .child(
            m.heat
                .clone()
                .map(|h| section("").child(heatmap(tr("profile.heatmap"), 150.0, h))),
        )
        .child(conns(&m))
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

fn tiles(m: &ProfileModel) -> View {
    let t = |label: &'static str, v: String, tone: Tone| {
        tile(tr(label), move || v.clone().into(), move || tone)
    };
    view()
        .gap(px(6.0))
        .child(
            tile_row()
                .child(t(
                    "metric.goodput",
                    format!("{}/s", fmt::compact(m.good_ops_per_s)),
                    Tone::Neutral,
                ))
                .child(t(
                    "metric.bandwidth",
                    format!("{} MiB/s", fmt::sig3(m.mib_per_s)),
                    Tone::Neutral,
                ))
                .child(t("metric.p50", fmt::dur_ms(m.p50_ms), Tone::Neutral))
                .child(t("metric.p99", fmt::dur_ms(m.p99_ms), Tone::Neutral))
                .child(t("metric.p999", fmt::dur_ms(m.p999_ms), Tone::Neutral))
                .child(t(
                    "metric.errors_of_ops",
                    fmt::ratio(m.error_ratio),
                    if m.error_ratio > 0.0 {
                        Tone::Crit
                    } else {
                        Tone::Neutral
                    },
                ))
                .child(t(
                    "metric.throttled_of_requests",
                    fmt::ratio(m.throttled_ratio),
                    if m.throttled_ratio > 0.0 {
                        Tone::Warn
                    } else {
                        Tone::Neutral
                    },
                )),
        )
        .child(
            small(fill(
                "metric.counts",
                &[
                    &fmt::count(m.ops_count),
                    &fmt::count(m.requests_count),
                    &fmt::clock_s(m.window_s),
                ],
            ))
            .tabular_nums(),
        )
}

/// The backend's requests by API: how many, what they answered, how long they took.
fn requests(m: &ProfileModel) -> Option<View> {
    use crate::ui::kit::Col;
    if m.apis.is_empty() {
        return None;
    }
    let cols = [
        Col {
            heading: "col.api",
            width: 0.0,
            numeric: false,
        },
        Col {
            heading: "col.requests",
            width: 72.0,
            numeric: true,
        },
        Col {
            heading: "col.status",
            width: 150.0,
            numeric: false,
        },
        Col {
            heading: "col.ttfb",
            width: 132.0,
            numeric: true,
        },
        Col {
            heading: "col.total_p99",
            width: 72.0,
            numeric: true,
        },
        Col {
            heading: "col.bytes",
            width: 132.0,
            numeric: true,
        },
        Col {
            heading: "col.reused",
            width: 64.0,
            numeric: true,
        },
    ];
    let rows = m
        .apis
        .iter()
        .map(|a| {
            vec![
                a.api.clone(),
                fmt::count(a.requests_count),
                a.status
                    .iter()
                    .take(3)
                    .map(|(c, n)| {
                        let c = if *c == 0 {
                            "—".to_owned()
                        } else {
                            c.to_string()
                        };
                        format!("{c} ×{}", fmt::count(*n))
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
                format!(
                    "{} / {}",
                    fmt::dur_ms(a.ttfb_p50_ms),
                    fmt::dur_ms(a.ttfb_p99_ms)
                ),
                fmt::dur_ms(a.total_p99_ms),
                format!("{} / {}", fmt::bytes(a.down_bytes), fmt::bytes(a.up_bytes)),
                fmt::ratio(a.reused_ratio),
            ]
        })
        .collect();
    Some(
        section(tr("profile.requests"))
            .test_id("requests")
            .child(kit::table(&cols, rows)),
    )
}

/// Retries and failures.
fn retries(m: &ProfileModel) -> View {
    let r = &m.retry;
    let n = fmt::count;
    section(tr("profile.retries"))
        .test_id("retries")
        .child(prop(tr("retry.retries"), n(r.retries_count)))
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

/// Bytes, and where the traffic went.
fn details(m: &ProfileModel) -> View {
    use crate::ui::kit::Col;
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
            .child(prop(
                tr("bytes.wire"),
                format!(
                    "{} / {}",
                    fmt::bytes(b.wire_rx_bytes),
                    fmt::bytes(b.wire_tx_bytes)
                ),
            ))
            .child(
                (b.cache_hit_bytes > 0)
                    .then(|| prop(tr("bytes.cache"), fmt::bytes(b.cache_hit_bytes))),
            )
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
        let cols = [
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
                width: 64.0,
                numeric: true,
            },
            Col {
                heading: "col.throttled",
                width: 72.0,
                numeric: true,
            },
            Col {
                heading: "col.p99",
                width: 72.0,
                numeric: true,
            },
        ];
        let rows = xs
            .iter()
            .map(|x| {
                vec![
                    x.key.clone(),
                    fmt::count(x.requests_count),
                    fmt::ratio(x.share_ratio),
                    x.throttled_ratio
                        .map(fmt::ratio)
                        .unwrap_or_else(|| "—".into()),
                    x.p99_ms.map(fmt::dur_ms).unwrap_or_else(|| "—".into()),
                ]
            })
            .collect();
        view()
            .gap_1()
            .test_id(id)
            .child(caption(tr(title)))
            .child(kit::table(&cols, rows))
    };
    let sp = &m.spread;
    for (title, id, xs) in [
        ("details.hot_keys", "hot-keys", &sp.hot_keys),
        ("details.sizes", "sizes", &sp.sizes),
        ("details.prefixes", "prefixes", &sp.prefixes),
        ("details.remotes", "remotes", &sp.remotes),
    ] {
        if !xs.is_empty() {
            v = v.child(slices(title, id, xs));
        }
    }
    v
}

/// How the run was set up, and the command that runs it again.
fn setup(m: &ProfileModel) -> View {
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
                    move || set_clipboard(cmd.clone()),
                )),
        )
}

/// One bar of every stage's share, then each stage with its share.
pub fn stage_bar(stages: &[crate::ui::data::StageBar]) -> View {
    let total: f64 = stages.iter().map(|s| s.ratio).sum::<f64>().max(1e-9);
    view()
        .flex_row()
        .h(px(10.0))
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

fn time_went(m: &ProfileModel) -> View {
    let max = m
        .stages
        .iter()
        .map(|s| s.ratio)
        .fold(0.0, f64::max)
        .max(1e-9);
    section(tr("profile.time"))
        .child(stage_bar(&m.stages))
        .children(m.stages.iter().filter(|s| s.ratio >= 0.001).map(|st| {
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(swatch(stage_token(&st.key)))
                .child(
                    body(
                        tr_dyn(&format!("stage.{}", st.key))
                            .map(str::to_owned)
                            .unwrap_or_else(|| st.key.clone()),
                    )
                    .w(px(150.0))
                    .number_of_lines(1),
                )
                .child(meter(st.ratio / max, stage_token(&st.key)))
                .child(small(fmt::ratio(st.ratio)).w(px(64.0)).tabular_nums())
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
    section(tr("profile.verbs"))
        .test_id("verbs")
        .children(vs.iter().map(|v| {
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_3()
                .py_1()
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
                .child(view().flex_1().min_w(px(120.0)).child(stage_bar(&v.stages)))
                .child(
                    small(if v.error_ratio > 0.0 {
                        format!("{} {}", tr("metric.errors"), fmt::ratio(v.error_ratio))
                    } else {
                        String::new()
                    })
                    .text_color(move || Tone::Crit.color()),
                )
        }))
}

/// The evidence chart of a finding (nothing when the run lacks its data).
fn evidence(m: &ProfileModel, f: &FindingRow) -> Option<View> {
    use crate::report::view::EvidenceChart;
    use crate::ui::kit::Col;
    match f.chart? {
        EvidenceChart::HotKeys if !m.spread.hot_keys.is_empty() => {
            let cols = [
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
                    width: 64.0,
                    numeric: true,
                },
                Col {
                    heading: "col.throttled",
                    width: 72.0,
                    numeric: true,
                },
            ];
            let rows = m
                .spread
                .hot_keys
                .iter()
                .take(5)
                .map(|k| {
                    vec![
                        k.key.clone(),
                        fmt::count(k.requests_count),
                        fmt::ratio(k.share_ratio),
                        k.throttled_ratio.map(fmt::ratio).unwrap_or_default(),
                    ]
                })
                .collect();
            return Some(
                view()
                    .gap_1()
                    .pt_1()
                    .test_id(&format!("evidence-{}", f.code))
                    .child(caption(tr("details.hot_keys")))
                    .child(kit::table(&cols, rows)),
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
        .child(body(f.cause.clone()).selectable(true))
        .child(small(f.evidence.clone()).selectable(true))
        .children(f.items.iter().map(|(k, v)| prop(k.clone(), v.clone())))
        .child(evidence(m, &f))
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

fn slowest(ws: &[Waterfall]) -> View {
    let hover = signal(None::<String>);
    let span = ws.iter().map(|w| w.latency_us).max().unwrap_or(1).max(1) as f64;
    section(tr("profile.slowest"))
        .child(
            small(move || {
                SharedString::from(
                    hover
                        .get()
                        .unwrap_or_else(|| tr("profile.hover").to_owned()),
                )
            })
            .tabular_nums()
            .number_of_lines(1),
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
            let readout = format!(
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
            let total = b.total_us.max(1) as f64;
            let failed = b.failed;
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
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
                        .h(px(10.0))
                        .min_w(px(0.0))
                        // Before the request: the backoff it waited since the previous attempt,
                        // as a thin line in the retry colour.
                        .child(view().w(relative(
                            ((b.start_us.saturating_sub(b.backoff_us)) as f64 / span).min(1.0)
                                as f32,
                        )))
                        .child((b.backoff_us > 0).then(|| {
                            view()
                                .h(px(2.0))
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
}

fn timeline(m: &ProfileModel) -> View {
    let (Some(ops), Some(lat)) = (plots::goodput(&m.timeline), plots::latency(&m.timeline)) else {
        return view();
    };
    section(tr("profile.timeline"))
        .test_id("timeline")
        .child(chart(tr("metric.goodput"), 110.0, move || {
            Some(ops.clone())
        }))
        .child(chart(tr("profile.latency"), 110.0, move || {
            Some(lat.clone())
        }))
        .child(
            plots::inflight(&m.timeline)
                .map(|p| chart(tr("plot.inflight"), 90.0, move || Some(p.clone()))),
        )
}

/// The latency distribution with its median and tail marked.
fn distribution(m: &ProfileModel) -> View {
    let Some(p) = plots::own_cdf(m) else {
        return view();
    };
    let heading = if m.cdf.exact {
        tr("plot.cdf")
    } else {
        tr("plot.cdf_buckets")
    };
    section(tr("profile.distribution"))
        .test_id("distribution")
        .child(chart(heading, 130.0, move || Some(p.clone())))
}

fn conns(m: &ProfileModel) -> View {
    let c = &m.conns;
    section(tr("profile.connections"))
        .child(prop(tr("conn.dialed"), c.dialed.to_string()))
        .child(prop(tr("conn.reuse"), fmt::ratio(c.reuse_ratio)))
        .child(prop(
            tr("conn.protocols"),
            format!("HTTP/1.1 {} · HTTP/2 {}", c.http1, c.http2),
        ))
        .child((!c.tls.is_empty()).then(|| prop("TLS", c.tls.clone())))
        .child(prop(tr("conn.peak"), c.peak_inflight.to_string()))
        .children(
            c.setup
                .iter()
                .filter(|(_, p50, _)| *p50 > 0.0)
                .map(|(k, p50, p99)| {
                    prop(
                        k.to_uppercase(),
                        format!("p50 {} · p99 {}", fmt::dur_ms(*p50), fmt::dur_ms(*p99)),
                    )
                }),
        )
}
