//! Runs: start a run (an operator URL, an `op.toml`, or a lab scenario for a first offline
//! run, with a preset), watch it stream in, and the history (`App::list`): open a run in
//! Profile, pick runs for Compare, delete them.

use nsh_base::theme;
use nsh_core::prelude::*;

use super::shell::{fill, page};
use crate::ui::chart::sparkline;
use crate::ui::data::RunRow;
use crate::ui::fmt;
use crate::ui::i18n::{tr, tr_dyn};
use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, field, meter, mono, row_button, section,
    segmented, small, strong, tile, tile_row,
};
use crate::ui::store::{LAB, PRESETS, Store, store};

pub fn runs() -> impl IntoElement {
    let s = store();
    page(
        view()
            .child(new_run(s))
            .child(dynamic(move || {
                let on = s
                    .run_state
                    .with(|st| st.as_ref().is_some_and(|st| st.outcome.is_none()));
                untracked(|| on.then(|| live(s)))
            }))
            .child(history(s)),
    )
}

fn new_run(s: Store) -> View {
    let f = s.form;
    let scenario = move || {
        f.operator
            .get()
            .strip_prefix("lab://")
            .map(str::to_owned)
            .filter(|n| !n.contains(char::is_whitespace))
    };
    section(tr("runs.new"))
        .child(
            view()
                .flex_row()
                .gap_2()
                .items_center()
                .child(
                    field(
                        "run-operator",
                        tr("runs.operator"),
                        tr("runs.operator_hint"),
                        move || f.operator.get(),
                        move |v| f.operator.set(v),
                    )
                    .flex_1()
                    .min_w(px(0.0)),
                )
                .child(
                    btn(
                        move || {
                            SharedString::from(if s.running() {
                                tr("runs.stop")
                            } else {
                                tr("runs.start")
                            })
                        },
                        move || {
                            if s.running() {
                                Kind::Danger
                            } else {
                                Kind::Primary
                            }
                        },
                        move || {
                            if s.running() {
                                s.stop_run()
                            } else {
                                s.start_run()
                            }
                        },
                    )
                    .test_id("run-start"),
                ),
        )
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(small(tr("runs.lab")).pr_1())
                .children(LAB.map(|name| {
                    let on = move || scenario().as_deref() == Some(name);
                    btn(
                        name,
                        move || if on() { Kind::Plain } else { Kind::Ghost },
                        move || f.operator.set(format!("lab://{name}")),
                    )
                    .accessibility_label(format!("lab://{name}"))
                })),
        )
        .child(dynamic(move || {
            let sc = scenario();
            untracked(|| sc.and_then(|n| tr_dyn(&format!("lab.{n}"))).map(small))
        }))
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_3()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(small(tr("runs.preset")))
                        .child(segmented(
                            PRESETS
                                .iter()
                                .map(|p| (*p, SharedString::from(*p)))
                                .collect(),
                            move || f.preset.get(),
                            move |p| f.preset.set(p),
                        )),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(small(tr("runs.duration")))
                        .child(
                            field(
                                "run-duration",
                                tr("runs.duration"),
                                "10s",
                                move || f.duration.get(),
                                move |v| f.duration.set(v),
                            )
                            .w(px(72.0)),
                        )
                        .child(small(tr("runs.concurrency")))
                        .child(
                            field(
                                "run-concurrency",
                                tr("runs.concurrency"),
                                "auto",
                                move || f.concurrency.get(),
                                move |v| f.concurrency.set(v),
                            )
                            .w(px(64.0)),
                        ),
                ),
        )
        .child(small(tr("runs.several")))
}

fn live(s: Store) -> View {
    let st = move || s.run_state.get().unwrap_or_default();
    section(tr("runs.live"))
        .test_id("live-run")
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    strong(move || {
                        let st = st();
                        let phase = if st.cleaning {
                            "phase.cleanup".to_owned()
                        } else {
                            format!(
                                "phase.{}",
                                st.phase.map(|p| p.label()).unwrap_or("preparing dataset")
                            )
                        };
                        SharedString::from(tr_dyn(&phase).unwrap_or(tr("phase.measuring")))
                    })
                    .flex_shrink_0(),
                )
                .child(dynamic(move || {
                    let st = st();
                    let frac = if st.total_s > 0.0 {
                        st.elapsed_s / st.total_s
                    } else {
                        0.0
                    };
                    untracked(|| meter(frac, "series-a").into_element())
                }))
                .child(
                    small(move || {
                        let st = st();
                        SharedString::from(format!(
                            "{} / {}",
                            fmt::clock_s(st.elapsed_s),
                            fmt::clock_s(st.total_s)
                        ))
                    })
                    .tabular_nums()
                    .flex_shrink_0(),
                ),
        )
        .child(sparkline(48.0, move || {
            s.run_state
                .with(|st| st.as_ref().map(|s| s.spark.clone()).unwrap_or_default())
        }))
        .child(
            tile_row()
                .child(tile(
                    tr("metric.goodput"),
                    move || format!("{}/s", fmt::compact(st().ops_per_s)).into(),
                    || Tone::Neutral,
                ))
                .child(tile(
                    tr("metric.p99"),
                    move || fmt::dur_ms(st().p99_ms).into(),
                    || Tone::Neutral,
                ))
                .child(tile(
                    tr("metric.errors"),
                    move || st().errors.to_string().into(),
                    move || {
                        if st().errors > 0 {
                            Tone::Crit
                        } else {
                            Tone::Neutral
                        }
                    },
                ))
                .child(tile(
                    tr("metric.throttled"),
                    move || st().throttled.to_string().into(),
                    move || {
                        if st().throttled > 0 {
                            Tone::Warn
                        } else {
                            Tone::Neutral
                        }
                    },
                )),
        )
}

fn verdict_badge(v: crate::model::VerdictLevel) -> View {
    badge(
        tr_dyn(&format!("verdict.{}", v.as_str())).unwrap_or(v.as_str()),
        move || Tone::of_verdict(v),
    )
}

fn history(s: Store) -> View {
    section(move || SharedString::from(fill("runs.history", &[&s.runs.with(|r| r.len())])))
        .child(dynamic(move || {
            let none = s.runs.with(|r| r.is_empty());
            untracked(|| {
                if none {
                    empty(tr("runs.empty"), tr("runs.empty_hint")).into_element()
                } else {
                    each(
                        move || s.runs.get(),
                        |r: &RunRow| r.id.clone(),
                        move |r, _| run_row(s, r.get_untracked()),
                    )
                    .into_element()
                }
            })
        }))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    btn(
                        move || {
                            SharedString::from(fill(
                                "runs.compare_picked",
                                &[&s.picked.with(|p| p.len())],
                            ))
                        },
                        || Kind::Plain,
                        move || {
                            let ids = s.picked.get_untracked();
                            if ids.len() >= 2 {
                                s.compare(ids)
                            }
                        },
                    )
                    .disabled(move || s.picked.with(|p| p.len() < 2)),
                )
                .child(small(tr("runs.pick_hint"))),
        )
}

fn run_row(s: Store, r: RunRow) -> View {
    let id = r.id.clone();
    let (open_id, pick_id, del_id, picked_id) = (id.clone(), id.clone(), id.clone(), id.clone());
    let picked = computed(move || s.picked.with(|p| p.contains(&picked_id)));
    let picked = move || picked.get();
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w(px(0.0))
        .border_b_1()
        .border_color(move || theme().border)
        .child(
            row_button(
                fill("runs.open", &[&id]),
                || false,
                move || s.open_run(&open_id),
            )
            .flex_1()
            .min_w(px(0.0))
            .flex_row()
            .items_center()
            .gap_2()
            .child(verdict_badge(r.verdict))
            .child(mono(r.id.clone()).flex_shrink_0())
            .child(
                body(r.operator.clone())
                    .number_of_lines(1)
                    .flex_1()
                    .min_w(px(0.0)),
            )
            .child(small(r.preset.clone()).number_of_lines(1).flex_shrink_0())
            .child(
                small(format!(
                    "{}/s · p99 {}",
                    fmt::compact(r.ops_per_s),
                    fmt::dur_ms(r.p99_ms)
                ))
                .tabular_nums()
                .number_of_lines(1)
                .flex_shrink_0(),
            )
            .child(small(fmt::short_time(&r.started_at)).flex_shrink_0())
            .child(r.partial.then(|| caption("*"))),
        )
        .child(
            btn(
                move || SharedString::from(if picked() { "✓" } else { "+" }),
                move || if picked() { Kind::Plain } else { Kind::Ghost },
                move || s.toggle_pick(&pick_id),
            )
            .accessibility_label(fill("runs.pick", &[&id])),
        )
        .child(
            btn("×", || Kind::Ghost, move || s.delete_run(&del_id))
                .accessibility_label(fill("runs.delete", &[&r.id])),
        )
}
