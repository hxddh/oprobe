//! Profile: one run (`App::show`): the verdict, the key numbers, where the time went, the
//! findings with their remedy path, the request waterfall of the slowest operations, the
//! timeline, the heatmap and the connections.

use std::rc::Rc;

use nsh_base::theme;
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::ui::chart::{Line, Plot, Unit, chart};
use crate::ui::data::{FindingRow, ProfileModel, Waterfall};
use crate::ui::fmt::{self, Scale};
use crate::ui::heatmap::heatmap;
use crate::ui::i18n::{finding_title, tr, tr_dyn};
use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, meter, mono, prop, section, small, stage_token,
    strong, swatch, tile, tile_row, title, token,
};
use crate::ui::store::{Page, Store, store};

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
        .child(tiles(&m))
        .child(time_went(&m))
        .child(findings(s, &m));
    if !m.slowest.is_empty() {
        v = v.child(slowest(&m.slowest));
    }
    v.child(timeline(&m))
        .child(
            m.heat
                .clone()
                .map(|h| section("").child(heatmap(tr("profile.heatmap"), 150.0, h))),
        )
        .child(conns(&m))
        .child((!m.warnings.is_empty()).then(|| {
            section(tr("profile.warnings"))
                .children(m.warnings.iter().map(|w| small(w.clone()).selectable(true)))
        }))
}

fn head(s: Store, m: &ProfileModel) -> View {
    view()
        .gap_1()
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(title(m.id.clone()))
                .child(verdict_badge(m.verdict))
                .child(view().flex_1())
                .child(btn(
                    tr("profile.to_operator"),
                    || Kind::Primary,
                    move || s.page.set(Page::Operator),
                ))
                .child({
                    let id = m.id.clone();
                    btn(
                        tr("profile.pick"),
                        || Kind::Plain,
                        move || {
                            if !s.picked.with_untracked(|p| p.contains(&id)) {
                                s.toggle_pick(&id);
                            }
                            s.page.set(Page::Compare);
                        },
                    )
                }),
        )
        .child(small(format!(
            "{} · {} · {}",
            m.operator_ref,
            m.preset,
            fmt::clock_s(m.window_s)
        )))
        .child(body(m.headline.clone()).selectable(true))
}

fn tiles(m: &ProfileModel) -> View {
    let t = |label: &'static str, v: String, tone: Tone| {
        tile(tr(label), move || v.clone().into(), move || tone)
    };
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
        .child(t(
            "metric.errors",
            fmt::ratio(m.error_ratio),
            if m.error_ratio > 0.0 {
                Tone::Crit
            } else {
                Tone::Neutral
            },
        ))
        .child(t(
            "metric.throttled",
            fmt::ratio(m.throttled_ratio),
            if m.throttled_ratio > 0.0 {
                Tone::Warn
            } else {
                Tone::Neutral
            },
        ))
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
    sec.children(m.findings.iter().map(|f| finding(s, f.clone())))
}

fn finding(s: Store, f: FindingRow) -> View {
    let remedy = f.remedy.clone();
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
                        move || s.apply_remedy(&remedy),
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
                    "queue", "connect", "send", "ttfb", "body", "stall", "client",
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
                "{} · {}",
                b.label,
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
                        .flex_1()
                        .h(px(10.0))
                        .min_w(px(0.0))
                        .child(view().w(relative((b.start_us as f64 / span).min(1.0) as f32)))
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
    let pts = m.timeline.clone();
    if pts.is_empty() {
        return view();
    }
    let window = pts.last().map(|p| p.t_s + 1.0).unwrap_or(1.0);
    let x = Scale {
        min: 0.0,
        max: window,
        log: false,
    };
    let ops = Plot {
        x,
        y: Scale::zero_based(pts.iter().map(|p| p.ops_per_s).fold(0.0, f64::max)),
        step_w: 1.0,
        y_fmt: Unit::Rate,
        lines: vec![Line {
            label: tr("metric.goodput"),
            token: "series-a",
            pts: pts.iter().map(|p| (p.t_s, p.ops_per_s)).collect(),
        }],
    };
    let lo = pts
        .iter()
        .map(|p| p.p50_ms)
        .filter(|v| *v > 0.0)
        .fold(f64::INFINITY, f64::min);
    let lat = Plot {
        x,
        y: Scale::log(
            if lo.is_finite() { lo } else { 0.1 },
            pts.iter().map(|p| p.p99_ms).fold(0.1, f64::max),
        ),
        step_w: 1.0,
        y_fmt: Unit::Ms,
        lines: vec![
            Line {
                label: "p50",
                token: "series-a",
                pts: pts.iter().map(|p| (p.t_s, p.p50_ms)).collect(),
            },
            Line {
                label: "p99",
                token: "stage-server",
                pts: pts.iter().map(|p| (p.t_s, p.p99_ms)).collect(),
            },
        ],
    };
    section(tr("profile.timeline"))
        .test_id("timeline")
        .child(chart(tr("metric.goodput"), 110.0, move || {
            Some(ops.clone())
        }))
        .child(chart(tr("profile.latency"), 110.0, move || {
            Some(lat.clone())
        }))
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
