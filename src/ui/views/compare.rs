//! Compare two runs: overlaid charts (A solid, B dashed), the regression gate's checks with
//! their significance, configuration differences and which findings came and went.

use nsh_base::theme;
use nsh_core::prelude::*;

use crate::report::diff::Status;
use crate::ui::chart::chart;
use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, num, picker, small, strong, title,
};
use crate::ui::model::compare::FindingChange;
use crate::ui::model::fmt;
use crate::ui::store::store;
use crate::ui::views::overview::{latency_plot, throughput_plot};

fn status_tone(s: Status) -> Tone {
    match s {
        Status::Pass => Tone::Neutral,
        Status::Improved => Tone::Ok,
        Status::Fail => Tone::Crit,
        Status::Skipped => Tone::Neutral,
    }
}

fn status_label(s: Status) -> &'static str {
    match s {
        Status::Pass => "same",
        Status::Improved => "better",
        Status::Fail => "worse",
        Status::Skipped => "skipped",
    }
}

pub fn compare_view() -> impl IntoElement {
    let s = store();
    let a = signal(None::<SharedString>);
    let b = signal(None::<SharedString>);
    // Follow what is compared.
    effect(move || {
        if let Some((va, vb, _)) = s.comparison.get() {
            a.set(Some(va.id().to_owned().into()));
            b.set(Some(vb.id().to_owned().into()));
        }
    });
    let items = move || {
        s.runs
            .get()
            .into_iter()
            .map(|r| {
                (
                    SharedString::from(r.id.clone()),
                    SharedString::from(format!(
                        "{} · {}",
                        r.id,
                        r.label.clone().unwrap_or(r.target_display.clone())
                    )),
                )
            })
            .collect::<Vec<_>>()
    };
    let cmp = s.comparison;
    view()
        .flex_1()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(move || theme().border)
                .child(strong("A"))
                .child(dynamic(move || {
                    let it = items();
                    untracked(|| {
                        view()
                            .w(px(260.0))
                            .child(picker("Baseline run", it, a))
                            .into_element()
                    })
                }))
                .child(strong("B"))
                .child(dynamic(move || {
                    let it = items();
                    untracked(|| {
                        view()
                            .w(px(260.0))
                            .child(picker("Candidate run", it, b))
                            .into_element()
                    })
                }))
                .child(btn(
                    "Compare",
                    || Kind::Primary,
                    move || {
                        if let (Some(x), Some(y)) = (a.get_untracked(), b.get_untracked()) {
                            s.compare(&x, &y);
                        }
                    },
                ))
                .child(btn(
                    "Swap",
                    || Kind::Plain,
                    move || {
                        if let (Some(x), Some(y)) = (a.get_untracked(), b.get_untracked()) {
                            s.compare(&y, &x);
                        }
                    },
                )),
        )
        .child(scroll().flex_1().child(dynamic(move || {
            let c = cmp.get();
            let log = s.log_latency.get();
            untracked(|| {
                let Some((va, vb, c)) = c else {
                    return empty(
                        "Comparing…",
                        "Pick two runs: A is the baseline, B the candidate.",
                    )
                    .into_element();
                };
                let d = c.diff.clone();
                let (sa, sb) = (va.series.clone(), vb.series.clone());
                let (sa2, sb2) = (sa.clone(), sb.clone());
                view()
                    .p_4()
                    .gap_4()
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(badge(
                                if d.passed {
                                    "no regression"
                                } else {
                                    "regression"
                                },
                                move || if d.passed { Tone::Ok } else { Tone::Crit },
                            ))
                            .child(title(format!("{} → {}", va.id(), vb.id()))),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_4()
                            .child(view().flex_1().min_w(px(0.0)).child(chart(
                                "THROUGHPUT · A solid, B dashed",
                                120.0,
                                move || throughput_plot(&sa, Some(&sb)),
                            )))
                            .child(view().flex_1().min_w(px(0.0)).child(chart(
                                "LATENCY · A solid, B dashed",
                                120.0,
                                move || latency_plot(&sa2, Some(&sb2), log),
                            ))),
                    )
                    .child(caption("CHANGES (regression gate, default thresholds)"))
                    .child(
                        view()
                            .gap_0p5()
                            .child(
                                view()
                                    .flex_row()
                                    .gap_3()
                                    .child(small("Metric").w(px(200.0)))
                                    .child(small("A").w(px(90.0)))
                                    .child(small("B").w(px(90.0)))
                                    .child(small("Change").w(px(80.0)))
                                    .child(small("Limit").flex_1()),
                            )
                            .children(c.diff.checks.iter().map(|ch| {
                                let tone = status_tone(ch.status);
                                let st = ch.status;
                                view()
                                    .flex_row()
                                    .gap_3()
                                    .items_center()
                                    .py(px(2.0))
                                    .child(body(ch.name.clone()).w(px(200.0)).number_of_lines(1))
                                    .child(num(fmt::sig3(ch.baseline)).w(px(90.0)))
                                    .child(num(fmt::sig3(ch.candidate)).w(px(90.0)))
                                    .child(
                                        num(fmt::delta_pct(ch.change_pct)).w(px(80.0)).text_color(
                                            move || match tone {
                                                Tone::Neutral => theme().foreground,
                                                t => t.color(),
                                            },
                                        ),
                                    )
                                    .child(
                                        view()
                                            .flex_row()
                                            .gap_2()
                                            .flex_1()
                                            .child(badge(status_label(st), move || tone))
                                            .child(small(ch.limit.clone())),
                                    )
                            })),
                    )
                    .child(caption("CONFIGURATION DIFFERENCES"))
                    .child(if c.diff.config_changes.is_empty() {
                        small("Same target, workload and client settings.").into_element()
                    } else {
                        view()
                            .gap_0p5()
                            .children(c.diff.config_changes.iter().map(|ch| {
                                view()
                                    .flex_row()
                                    .gap_3()
                                    .child(small(ch.kind.label()).w(px(70.0)))
                                    .child(body(ch.field.clone()).w(px(220.0)))
                                    .child(
                                        body(format!("{} → {}", ch.baseline, ch.candidate))
                                            .flex_1(),
                                    )
                            }))
                            .into_element()
                    })
                    .child(caption("FINDINGS"))
                    .child(if c.findings.is_empty() {
                        small("Neither run has findings.").into_element()
                    } else {
                        view()
                            .gap_1()
                            .children(c.findings.iter().map(|(ch, f)| {
                                let tone = match ch {
                                    FindingChange::New => Tone::Crit,
                                    FindingChange::Resolved => Tone::Ok,
                                    FindingChange::Persisting => Tone::Neutral,
                                };
                                view()
                                    .flex_row()
                                    .gap_2()
                                    .items_center()
                                    .child(badge(ch.label(), move || tone))
                                    .child(body(f.cause.clone()).flex_1())
                            }))
                            .into_element()
                    })
                    .child(
                        view()
                            .gap_0p5()
                            .children(c.diff.warnings.iter().map(|w| small(format!("⚠ {w}")))),
                    )
                    .into_element()
            })
        })))
}
