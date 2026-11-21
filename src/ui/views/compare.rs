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
use crate::ui::model::verify::Verdict;
use crate::ui::model::{fmt, runs};
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

/// The Verify verdict over its comparison: confirmed, refuted or inconclusive, with numbers.
fn verdict_banner(v: Verdict) -> View {
    use crate::analysis::verification::VerifyOutcome;
    let tone = match v.outcome {
        VerifyOutcome::Confirmed => Tone::Ok,
        VerifyOutcome::Refuted => Tone::Crit,
        VerifyOutcome::Inconclusive => Tone::Warn,
    };
    view()
        .gap_1()
        .p_3()
        .rounded(px(6.0))
        .border_1()
        .border_color(move || tone.color())
        .role(AccessibilityRole::Alert)
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(badge(v.word(), move || tone))
                .child(strong(format!("Verify {}: {}", v.code, v.word()))),
        )
        .children(
            v.lines
                .into_iter()
                .map(|l| small(l).tabular_nums().selectable(true)),
        )
}

fn status_label(s: Status) -> &'static str {
    match s {
        Status::Pass => "same",
        Status::Improved => "better",
        Status::Fail => "worse",
        Status::Skipped => "skipped",
    }
}

/// A run as a selector item: `id · label or target`, with a mark when it measured the same
/// target and workload as the other side.
fn item(r: &crate::store::RunSummary, same: bool) -> (SharedString, SharedString) {
    (
        SharedString::from(r.id.clone()),
        SharedString::from(format!(
            "{}{} · {}",
            if same { "= " } else { "" },
            r.id,
            r.label.clone().unwrap_or(r.target_display.clone())
        )),
    )
}

/// One side's selector: it shrinks (ellipsis) instead of wrapping or overlapping.
fn side(
    name: &'static str,
    label: &'static str,
    items: Memo<Vec<(SharedString, SharedString)>>,
    value: Signal<Option<SharedString>>,
) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w(px(0.0))
        .max_w(px(300.0))
        .child(strong(name).flex_shrink_0())
        .child(dynamic(move || {
            let it = items.get();
            untracked(|| {
                view()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(picker(label, it, value))
                    .into_element()
            })
        }))
}

pub fn compare_view() -> impl IntoElement {
    let s = store();
    let a = signal(None::<SharedString>);
    let b = signal(None::<SharedString>);
    // Follow what is compared (or the run Compare was opened on, as B).
    effect(move || {
        let (pa, pb) = s.compare_pick.get();
        a.set(pa.map(Into::into));
        b.set(pb.map(Into::into));
    });
    // Each side lists the other side's target and workload first; built once per change of
    // the runs or of the other side, not on every render.
    let choices = move |other: Signal<Option<SharedString>>| {
        computed(move || {
            let o = other.get();
            s.runs.with(|runs| {
                let other_run = o
                    .as_deref()
                    .and_then(|id| runs.iter().find(|r| r.id == id).cloned());
                runs::compare_choices(runs, o.as_deref())
                    .iter()
                    .map(|r| {
                        let same = other_run
                            .as_ref()
                            .is_some_and(|x| x.id != r.id && runs::same_workload(x, r));
                        item(r, same)
                    })
                    .collect::<Vec<_>>()
            })
        })
    };
    let items_a = choices(b);
    let items_b = choices(a);
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
                .min_w(px(0.0))
                .border_b_1()
                .border_color(move || theme().border)
                .child(side("A", "Baseline run", items_a, a))
                .child(side("B", "Candidate run", items_b, b))
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
            let verdict = s.verdict.get();
            let loading = s.comparing.get();
            untracked(|| {
                if c.is_none() && loading {
                    return empty(
                        "Comparing…",
                        "Loading both runs and running the regression gate.",
                    )
                    .into_element();
                }
                let Some((va, vb, c)) = c else {
                    return empty(
                        "Pick two runs",
                        "A is the baseline, B the candidate. Runs of the same target and \
                         workload (marked =) compare; others show why they do not.",
                    )
                    .into_element();
                };
                let d = c.diff.clone();
                let comparable = c.comparable();
                let (sa, sb) = (va.series.clone(), vb.series.clone());
                let (sa2, sb2) = (sa.clone(), sb.clone());
                view()
                    .p_4()
                    .gap_4()
                    .min_w(px(0.0))
                    .child(verdict.map(verdict_banner))
                    .child(
                        view()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.0))
                            .child(if comparable {
                                badge(
                                    if d.passed {
                                        "no regression"
                                    } else {
                                        "regression"
                                    },
                                    move || if d.passed { Tone::Ok } else { Tone::Crit },
                                )
                            } else {
                                badge("not comparable", || Tone::Warn)
                            })
                            .child(
                                title(format!("{} → {}", va.id(), vb.id()))
                                    .number_of_lines(1)
                                    .flex_1()
                                    .min_w(px(0.0)),
                            ),
                    )
                    .child((!comparable).then(|| {
                        view()
                            .gap_0p5()
                            .p_3()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(move || Tone::Warn.color())
                            .child(strong(
                                "These runs measured different things: no regression verdict.",
                            ))
                            .children(
                                c.not_comparable
                                    .iter()
                                    .map(|w| small(format!("• {w}")).selectable(true)),
                            )
                            .child(small(
                                "Compare runs of the same target and workload (replay one with \
                                 Run Again or `oprobe run --from RUN`).",
                            ))
                    }))
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
                    .child(caption(if comparable {
                        "CHANGES (regression gate, default thresholds)"
                    } else {
                        "CHANGES (for information only: not comparable)"
                    }))
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
