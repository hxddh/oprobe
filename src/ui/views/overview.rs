//! Overview: the verdict in one sentence, the top findings (with Verify), the key numbers,
//! honest charts, where the time goes (stage shares incl. retry) and the brushable heatmap.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::model::{Finding, Report};
use crate::ui::chart::{Line, Plot, Unit, chart};
use crate::ui::heatmap::heatmap;
use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, kpi, small, stage_color, strong, swatch, title,
};
use crate::ui::model::Tab;
use crate::ui::model::fmt;
use crate::ui::model::scale::Scale;
use crate::ui::model::series::Series;
use crate::ui::store::{Phase, Sel, Store, store};

/// The throughput chart's plot.
pub fn throughput_plot(s: &Series, b: Option<&Series>) -> Option<Plot> {
    if s.points.is_empty() {
        return None;
    }
    let mut lines = vec![Line {
        label: "ops/s",
        token: "series-a",
        pts: s.points.iter().map(|p| (p.t_s, p.ops_per_s)).collect(),
        step: true,
        dashed: false,
    }];
    let mut max = s.max(|p| p.ops_per_s);
    let mut window = s.window_s;
    if let Some(b) = b {
        lines.push(Line {
            label: "ops/s",
            token: "series-b",
            pts: b.points.iter().map(|p| (p.t_s, p.ops_per_s)).collect(),
            step: true,
            dashed: true,
        });
        max = max.max(b.max(|p| p.ops_per_s));
        window = window.max(b.window_s);
    }
    Some(Plot {
        x: Scale {
            min: 0.0,
            max: window.max(s.bucket_s),
            log: false,
        },
        y: Scale::zero_based(max),
        step_w: s.bucket_s,
        x_fmt: Unit::Seconds,
        y_fmt: Unit::Rate,
        lines,
    })
}

/// MiB/s.
pub fn bandwidth_plot(s: &Series) -> Option<Plot> {
    if s.points.is_empty() {
        return None;
    }
    Some(Plot {
        x: Scale {
            min: 0.0,
            max: s.window_s.max(s.bucket_s),
            log: false,
        },
        y: Scale::zero_based(s.max(|p| p.mib_per_s)),
        step_w: s.bucket_s,
        x_fmt: Unit::Seconds,
        y_fmt: Unit::Mib,
        lines: vec![Line {
            label: "MiB/s",
            token: "stage-download",
            pts: s.points.iter().map(|p| (p.t_s, p.mib_per_s)).collect(),
            step: true,
            dashed: false,
        }],
    })
}

/// p50 and p99 per bucket, on a log or linear axis.
pub fn latency_plot(s: &Series, b: Option<&Series>, log: bool) -> Option<Plot> {
    if s.points.is_empty() {
        return None;
    }
    let mut lines = vec![
        Line {
            label: "p50",
            token: "series-a",
            pts: s.points.iter().map(|p| (p.t_s, p.p50_ms)).collect(),
            step: true,
            dashed: false,
        },
        Line {
            label: "p99",
            token: "stage-server",
            pts: s.points.iter().map(|p| (p.t_s, p.p99_ms)).collect(),
            step: true,
            dashed: false,
        },
    ];
    let mut max = s.max(|p| p.p99_ms);
    let mut min = s.min_positive(|p| p.p50_ms);
    let mut window = s.window_s;
    if let Some(b) = b {
        lines.push(Line {
            label: "p99",
            token: "series-b",
            pts: b.points.iter().map(|p| (p.t_s, p.p99_ms)).collect(),
            step: true,
            dashed: true,
        });
        max = max.max(b.max(|p| p.p99_ms));
        min = min.min(b.min_positive(|p| p.p50_ms));
        window = window.max(b.window_s);
    }
    let y = if log {
        Scale::log(if min.is_finite() { min } else { 0.1 }, max.max(0.1))
    } else {
        Scale::zero_based(max)
    };
    Some(Plot {
        x: Scale {
            min: 0.0,
            max: window.max(s.bucket_s),
            log: false,
        },
        y,
        step_w: s.bucket_s,
        x_fmt: Unit::Seconds,
        y_fmt: Unit::Ms,
        lines,
    })
}

/// Failed and throttled operations per second.
pub fn errors_plot(s: &Series) -> Option<Plot> {
    if s.points.is_empty() {
        return None;
    }
    Some(Plot {
        x: Scale {
            min: 0.0,
            max: s.window_s.max(s.bucket_s),
            log: false,
        },
        y: Scale::zero_based(
            s.max(|p| p.errors_per_s)
                .max(s.max(|p| p.throttled_per_s))
                .max(1.0),
        ),
        step_w: s.bucket_s,
        x_fmt: Unit::Seconds,
        y_fmt: Unit::Rate,
        lines: vec![
            Line {
                label: "failed",
                token: "crit",
                pts: s.points.iter().map(|p| (p.t_s, p.errors_per_s)).collect(),
                step: true,
                dashed: false,
            },
            Line {
                label: "throttled",
                token: "stage-retry",
                pts: s
                    .points
                    .iter()
                    .map(|p| (p.t_s, p.throttled_per_s))
                    .collect(),
                step: true,
                dashed: true,
            },
        ],
    })
}

/// The stage-share bar with its legend (shares of operation time, retry included).
pub fn stage_bar(parts: Vec<(&'static str, f64)>) -> View {
    let total: f64 = parts.iter().map(|(_, v)| v).sum::<f64>().max(1e-9);
    let shown: Vec<(&'static str, f64)> = parts.into_iter().filter(|(_, v)| *v >= 0.05).collect();
    view()
        .gap_2()
        .child(
            view()
                .flex_row()
                .h(px(14.0))
                .rounded(px(3.0))
                .overflow_hidden()
                .bg(move || theme().muted)
                .children(shown.clone().into_iter().map(move |(name, v)| {
                    view()
                        .h_full()
                        .w(relative((v / total) as f32))
                        .bg(stage_color(name))
                        .label(format!("{name} {}", fmt::pct(v)))
                })),
        )
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .gap_3()
                .children(shown.into_iter().map(|(name, v)| {
                    view()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(swatch(stage_color(name)))
                        .child(small(format!("{name} {}", fmt::pct(v))).tabular_nums())
                })),
        )
}

fn verdict_word(r: &Report) -> &'static str {
    match r.verdict.level {
        crate::model::VerdictLevel::Ok => "Healthy",
        crate::model::VerdictLevel::Review => "Review",
        crate::model::VerdictLevel::Attention => "Attention",
        crate::model::VerdictLevel::Critical => "Critical",
    }
}

/// A finding as a compact row (Overview's top findings and the Findings tab).
pub fn finding_row(s: Store, f: Finding, full: bool) -> View {
    let tone = Tone::of_severity(f.severity);
    let has_cmd = f.verification.as_ref().is_some_and(|v| v.command.is_some());
    let verify_label = f
        .verification
        .as_ref()
        .and_then(|v| v.command.clone())
        .unwrap_or_default();
    let f2 = f.clone();
    view()
        .gap_1()
        .py(px(6.0))
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(badge(f.severity.label(), move || tone))
                .child(strong(f.cause.clone()).flex_1().min_w(px(0.0)))
                .child(has_cmd.then(|| {
                    btn("Verify", || Kind::Plain, move || s.verify(&f2))
                        .accessibility_label(format!("Verify: {verify_label}"))
                })),
        )
        .child(small(f.evidence.text.clone()))
        .child(
            body(format!("→ {}", f.advice))
                .text_size(px(12.0))
                .text_color(move || theme().foreground),
        )
        .child(
            full.then(|| {
                f.verification.as_ref().map(|v| {
                    small(match &v.command {
                        Some(c) => format!("verify: {} ({c})", v.description),
                        None => format!("verify: {}", v.description),
                    })
                })
            })
            .flatten(),
        )
}

fn finished(s: Store) -> View {
    let cur = s.current;
    let report = move || cur.with(|c| c.as_ref().map(|c| c.run.report.clone()));
    let r = computed(report);
    let series = computed(move || cur.with(|c| c.as_ref().map(|c| c.series.clone())));
    let log = s.log_latency;
    let num = move |f: fn(&Report) -> SharedString| {
        move || r.with(|r| r.as_ref().map(f).unwrap_or_default())
    };
    view()
        .gap_4()
        .p_4()
        .min_w(px(0.0))
        // Verdict sentence.
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(badge(
                    move || r.with(|r| r.as_ref().map(verdict_word).unwrap_or("")),
                    move || {
                        r.with(|r| {
                            r.as_ref()
                                .map(|r| Tone::of_verdict(r.verdict.level))
                                .unwrap_or(Tone::Neutral)
                        })
                    },
                ))
                .child(
                    title(move || {
                        r.with(|r| {
                            r.as_ref()
                                .map(|r| r.verdict.headline.clone())
                                .unwrap_or_default()
                        })
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .selectable(true),
                ),
        )
        // Key numbers: one row, never wrapping.
        .child(
            view()
                .flex_row()
                .rounded(px(6.0))
                .border_1()
                .border_color(move || theme().border)
                .overflow_hidden()
                .child(kpi(
                    "OPS/S",
                    num(|r| fmt::compact(r.summary.ops_per_s).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "MiB/S",
                    num(|r| fmt::compact(r.summary.mib_per_s).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "P50",
                    num(|r| fmt::dur_ms(r.summary.latency.p50).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "P99",
                    num(|r| fmt::dur_ms(r.summary.latency.p99).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "ERRORS",
                    num(|r| fmt::ratio(r.summary.error_rate).into()),
                    move || {
                        if r.with(|r| r.as_ref().is_some_and(|r| r.summary.error_rate > 0.0)) {
                            Tone::Crit
                        } else {
                            Tone::Neutral
                        }
                    },
                ))
                .child(kpi(
                    "REQ/OP",
                    num(|r| fmt::sig3(r.bytes.request_amplification).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "COST/MO",
                    num(|r| {
                        if r.cost.applicable {
                            fmt::usd(r.cost.selected.total_usd).into()
                        } else {
                            "n/a".into()
                        }
                    }),
                    || Tone::Neutral,
                )),
        )
        // Top findings.
        .child(dynamic(move || {
            let findings: Vec<Finding> = r.with(|r| {
                r.as_ref()
                    .map(|r| {
                        let mut f = r.findings.clone();
                        f.sort_by_key(|f| std::cmp::Reverse(f.severity));
                        f.into_iter().take(3).collect()
                    })
                    .unwrap_or_default()
            });
            let more = r.with(|r| r.as_ref().map(|r| r.findings.len()).unwrap_or(0));
            untracked(|| {
                if findings.is_empty() {
                    return view().into_element();
                }
                view()
                    .gap_1()
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .child(caption("TOP FINDINGS").flex_1())
                            .child((more > 3).then(|| {
                                btn(
                                    format!("All {more} findings"),
                                    || Kind::Ghost,
                                    move || s.set_tab(Tab::Findings),
                                )
                            })),
                    )
                    .children(findings.into_iter().map(|f| finding_row(s, f, false)))
                    .into_element()
            })
        }))
        // Where the time goes.
        .child(
            view()
                .gap_2()
                .child(caption("WHERE OPERATION TIME GOES"))
                .child(dynamic(move || {
                    let parts = r.with(|r| r.as_ref().map(|r| r.stages.shares.parts().to_vec()));
                    untracked(|| match parts {
                        Some(p) => stage_bar(p).into_element(),
                        None => view().into_element(),
                    })
                })),
        )
        // Charts.
        .child(
            view()
                .flex_row()
                .gap_4()
                .child(view().flex_1().min_w(px(0.0)).child(chart(
                    "THROUGHPUT",
                    120.0,
                    move || series.with(|s| s.as_ref().and_then(|s| throughput_plot(s, None))),
                )))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(chart("LATENCY", 120.0, move || {
                            let lg = log.get();
                            series.with(|s| s.as_ref().and_then(|s| latency_plot(s, None, lg)))
                        })),
                ),
        )
        .child(
            view()
                .flex_row()
                .gap_4()
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(chart("BANDWIDTH", 90.0, move || {
                            series.with(|s| s.as_ref().and_then(bandwidth_plot))
                        })),
                )
                .child(view().flex_1().min_w(px(0.0)).child(chart(
                    "ERRORS & THROTTLING",
                    90.0,
                    move || series.with(|s| s.as_ref().and_then(errors_plot)),
                ))),
        )
        .child(heatmap(
            150.0,
            move || cur.with(|c| c.as_ref().map(|c| c.heatmap.clone())),
            s.brush,
            move |b| s.apply_brush(b),
        ))
}

fn live(s: Store) -> View {
    let l = s.live;
    let snap = computed(move || {
        l.with(|l| {
            l.as_ref()
                .and_then(|l| l.snapshot.as_ref().map(|s| s.summary.clone()))
        })
    });
    let series = computed(move || l.with(|l| l.as_ref().map(|l| l.series.clone())));
    let log = s.log_latency;
    let num = move |f: fn(&crate::trace::Summary) -> SharedString| {
        move || snap.with(|s| s.as_ref().map(f).unwrap_or_else(|| "—".into()))
    };
    view()
        .gap_4()
        .p_4()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(badge(
                    move || {
                        l.with(|l| match l.as_ref().map(|l| l.phase) {
                            Some(Phase::Running) => "Running",
                            Some(Phase::Stopping) => "Stopping",
                            Some(Phase::Done) => "Finished",
                            Some(Phase::Failed) => "Failed",
                            None => "",
                        })
                    },
                    move || {
                        l.with(|l| match l.as_ref().map(|l| l.phase) {
                            Some(Phase::Failed) => Tone::Crit,
                            Some(Phase::Done) => Tone::Ok,
                            _ => Tone::Info,
                        })
                    },
                ))
                .child(title(move || {
                    l.with(|l| {
                        l.as_ref()
                            .map(|l| match l.phase {
                                Phase::Stopping => "Stopping — cleaning up test data…".to_owned(),
                                Phase::Failed => "The run failed".to_owned(),
                                _ => format!("Measuring {}", l.title),
                            })
                            .unwrap_or_default()
                    })
                })),
        )
        .child(dynamic(move || {
            let failure = l.with(|l| l.as_ref().and_then(|l| l.failure.clone()));
            untracked(|| match failure {
                Some(f) => view()
                    .gap_1()
                    .p_3()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(move || Tone::Crit.color())
                    .child(body(f.error.clone()).selectable(true))
                    .child(f.hint.map(|(h, steps)| {
                        view()
                            .gap_1()
                            .child(strong(format!("Hint: {h}")))
                            .children(steps.into_iter().map(|st| small(format!("• {st}"))))
                    }))
                    .into_element(),
                None => view().into_element(),
            })
        }))
        .child(
            view()
                .flex_row()
                .rounded(px(6.0))
                .border_1()
                .border_color(move || theme().border)
                .overflow_hidden()
                .child(kpi(
                    "OPS/S",
                    num(|s| fmt::compact(s.ops_per_s).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "MiB/S",
                    num(|s| fmt::compact(s.mib_per_s).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "P50",
                    num(|s| fmt::dur_ms(s.latency.p50).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "P99",
                    num(|s| fmt::dur_ms(s.latency.p99).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "ERRORS",
                    num(|s| fmt::ratio(s.error_rate).into()),
                    || Tone::Neutral,
                ))
                .child(kpi(
                    "OPS",
                    num(|s| fmt::compact(s.ops as f64).into()),
                    || Tone::Neutral,
                )),
        )
        .child(
            view()
                .flex_row()
                .gap_4()
                .child(view().flex_1().min_w(px(0.0)).child(chart(
                    "THROUGHPUT",
                    120.0,
                    move || series.with(|s| s.as_ref().and_then(|s| throughput_plot(s, None))),
                )))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(chart("LATENCY", 120.0, move || {
                            let lg = log.get();
                            series.with(|s| s.as_ref().and_then(|s| latency_plot(s, None, lg)))
                        })),
                ),
        )
        .child(chart("ERRORS & THROTTLING", 80.0, move || {
            series.with(|s| s.as_ref().and_then(errors_plot))
        }))
        .child(
            view()
                .gap_0p5()
                .child(caption("LOG"))
                .child(dynamic(move || {
                    let lines: Vec<String> = l.with(|l| {
                        l.as_ref()
                            .map(|l| l.log.iter().rev().take(6).rev().cloned().collect())
                            .unwrap_or_default()
                    });
                    untracked(|| {
                        view()
                            .children(lines.into_iter().map(|m| small(m).number_of_lines(1)))
                            .into_element()
                    })
                })),
        )
}

pub fn overview() -> impl IntoElement {
    let s = store();
    scroll().flex_1().child(dynamic(move || {
        let is_live = s.sel.get() == Sel::Live;
        let has = s.current.with(|c| c.is_some());
        untracked(|| {
            if is_live {
                live(s).into_element()
            } else if has {
                finished(s).into_element()
            } else {
                empty("Loading…", "").into_element()
            }
        })
    }))
}
