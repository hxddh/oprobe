//! The main screen (design v5 §9): a target's health — live tiles with sparklines of the last
//! runs and the baseline band, the chosen limit hypothesis in one sentence, one primary action
//! with its predicted outcome (opening the Fix menu), and the run history as an annotated
//! timeline. Everything it says comes from [`crate::ui::model::health`].

use nsh_base::theme;
use nsh_core::element::desktop::{MenuItem, popup_menu};
use nsh_core::prelude::*;

use crate::ui::kit::{
    Kind, Tone, badge, body, btn, caption, empty, kpi_detail, kpi_row, row_button, small, title,
};
use crate::ui::model::health::{self, Health, sparkline};
use crate::ui::model::i18n::{tr, trf};
use crate::ui::store::{Store, store};

/// The Fix menu (design v5 §9: the primary action opens the fixes on the Pareto front): verify
/// the finding's fix with its prediction, or tune for throughput, latency or cost.
pub fn fix_menu(s: Store) {
    let finding = s.top_verifiable();
    let predicted = s
        .current
        .with_untracked(|c| c.as_ref().and_then(|c| health::action(&c.run.report)))
        .and_then(|a| a.predicted);
    let busy = s.busy();
    let mut items = Vec::new();
    if let Some(f) = finding {
        let label = match &predicted {
            Some(p) => format!("Verify the fix: {} ({p})", f.advice),
            None => format!("Verify the fix: {}", f.advice),
        };
        items.push(
            MenuItem::new(label)
                .disabled(busy)
                .on_select(move || s.root.run(|| s.verify(&f))),
        );
        items.push(MenuItem::separator());
    }
    for (objective, label) in [
        ("throughput", "Tune for Throughput"),
        ("latency", "Tune for p99 Latency"),
        ("cost", "Tune for Cost"),
    ] {
        items.push(
            MenuItem::new(label)
                .disabled(busy)
                .on_select(move || s.root.run(|| s.tune_for(objective))),
        );
    }
    let _ = popup_menu(items);
}

fn tile(t: health::Tile) -> View {
    let tone = Tone::of_level(t.level);
    let spark = sparkline(&t.series);
    let detail = match (&t.band_text, spark.is_empty()) {
        (Some(b), false) => format!("{spark} · {b}"),
        (Some(b), true) => b.clone(),
        (None, false) => spark,
        (None, true) => String::new(),
    };
    let value: SharedString = t.value.into();
    let detail: SharedString = detail.into();
    let id: &'static str = match t.key {
        "ops" => "health-ops",
        "p99" => "health-p99",
        "throttled" => "health-throttled",
        _ => "health-cost",
    };
    kpi_detail(
        t.title,
        move || value.clone(),
        move || detail.clone(),
        move || tone,
    )
    .test_id(id)
}

fn history_row(s: Store, h: health::HistoryRow) -> View {
    let tone = Tone::of_level(h.level);
    let id = h.id.clone();
    let a11y = format!("{} · {} · {}", h.title, h.detail, h.annotations.join(" · "));
    view().child(
        row_button(a11y, || false, move || s.select_run(&id))
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                view()
                    .w(px(8.0))
                    .h(px(8.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(move || tone.color()),
            )
            .child(
                view()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(body(h.title).number_of_lines(1))
                    .child(small(h.detail).number_of_lines(1).tabular_nums()),
            )
            .children(
                h.annotations
                    .into_iter()
                    .map(|a| badge(a, || Tone::Info).flex_shrink_0()),
            ),
    )
}

fn page(s: Store, h: Health) -> View {
    let latest = h.latest.clone();
    let latest_level = h.history.first().map(|r| (r.verdict, r.level));
    view()
        .p_4()
        .gap_4()
        .min_w(px(0.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
                .child(latest_level.map(|(w, l)| badge(w, move || Tone::of_level(l))))
                .child(
                    title(trf("app.health_of", &[("target", &h.target)]))
                        .number_of_lines(1)
                        .flex_1()
                        .min_w(px(0.0)),
                )
                .child(latest.map(|id| {
                    btn(
                        tr("app.open_latest"),
                        || Kind::Plain,
                        move || s.select_run(&id),
                    )
                })),
        )
        .child(kpi_row().children(h.tiles.into_iter().map(tile)))
        .child(
            view()
                .gap_1()
                .child(caption(tr("app.hypothesis")))
                .child(
                    body(h.hypothesis)
                        .selectable(true)
                        .test_id("health-hypothesis"),
                )
                .children(
                    h.alternatives
                        .into_iter()
                        .map(|a| small(format!("also considered: {a}"))),
                ),
        )
        .child(h.action.map(|a| {
            view()
                .gap_1()
                .child(
                    view().flex_row().child(
                        btn(a.label.clone(), || Kind::Primary, move || fix_menu(s))
                            .accessibility_label(format!(
                                "{} {}",
                                a.label,
                                a.predicted.clone().unwrap_or_default()
                            ))
                            .test_id("health-fix"),
                    ),
                )
                .child(a.predicted.map(small))
        }))
        .child(
            view()
                .gap_1()
                .child(caption(tr("app.history")))
                .children(h.history.into_iter().map(move |r| history_row(s, r))),
        )
}

pub fn health_view() -> impl IntoElement {
    let s = store();
    scroll()
        .flex_1()
        .child(dynamic(move || {
            let key = s.health_target.get();
            let runs = s.runs.get();
            let baselines = s.baselines.get();
            let today = crate::util::now_rfc3339();
            let today = crate::ui::model::fmt::day_of(&today).to_owned();
            let h = s.current.with(|c| {
                let key = key.clone()?;
                // The report of the target's newest run, once it has loaded.
                let newest = runs.iter().find(|r| health::target_key(r) == key)?;
                let rep = c
                    .as_ref()
                    .filter(|c| c.id() == newest.id)
                    .map(|c| &c.run.report);
                Some(health::health(&key, &runs, &baselines, rep, &today))
            });
            untracked(|| match h {
                Some(h) if !h.history.is_empty() => page(s, h).into_element(),
                _ => empty(tr("app.no_runs_for_target"), "").into_element(),
            })
        }))
        .bg(move || theme().background)
}
