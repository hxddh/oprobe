//! The tuner: throttle-aware search for the best client settings.

use crate::util::fmt_ms;
use nsh_base::theme;
use nsh_core::prelude::*;

use super::workbench::{failure_banner, run_form};
use crate::ui::store::store;
use crate::ui::widgets::*;

const COLS: [(&str, f32); 10] = [
    ("stage", 120.0),
    ("conc", 50.0),
    ("io", 40.0),
    ("chunk", 64.0),
    ("gap", 64.0),
    ("cache", 50.0),
    ("ops/s", 76.0),
    ("MiB/s", 76.0),
    ("p99", 70.0),
    ("result", 150.0),
];

fn trials() -> impl IntoElement {
    let s = store();
    card("Trials").child(dynamic(move || {
        let t = s.trials.get();
        if t.is_empty() {
            return muted("Each trial runs the workload against the target with one configuration. Throttled trials (429/503) mark the ceiling.")
                .into_element();
        }
        let mut v = view().child(table_head(&COLS));
        for tr in t {
            let k = &tr.knobs;
            let (verdict, tone) = if tr.throttled() {
                (format!("throttled ×{}", tr.throttled_responses), Tone::Crit)
            } else if tr.feasible {
                ("ok".to_owned(), Tone::Ok)
            } else {
                (tr.note.clone(), Tone::Warn)
            };
            v = v.child(table_row(
                &COLS,
                vec![
                    cell(format!("#{} {}", tr.id, tr.stage)),
                    cell(k.concurrency.to_string()),
                    cell(k.io_concurrency.to_string()),
                    cell(k.chunk_size.to_string()),
                    cell(k.range_gap.to_string()),
                    cell(if k.cache { "on" } else { "off" }),
                    cell(format!("{:.1}", tr.ops_per_s)),
                    cell(format!("{:.1}", tr.mib_per_s)),
                    cell(fmt_ms(tr.p99_ms)),
                    cell_tone(verdict, tone),
                ],
            ));
        }
        v.into_element()
    }))
}

fn result() -> impl IntoElement {
    let s = store();
    dynamic(move || {
        let Some(t) = s.tuned.get() else {
            return view().into_element();
        };
        let k = &t.knobs;
        let sm = &t.summary;
        let mut c = card("Recommended configuration").child(
            row()
                .child(badge(
                    format!("{:+.0}% {}", sm.improvement * 100.0, sm.objective),
                    Tone::Ok,
                ))
                .child(text(k.summary()).text_sm().flex_1()),
        );
        if let Some(ce) = sm.throttle_ceiling {
            c = c.child(
                text(format!(
                    "Throttling ceiling: concurrency {ce} (recommendations stay at or below {}).",
                    crate::tune::safe_concurrency(ce)
                ))
                .text_sm()
                .text_color(token("warn")),
            );
        }
        for n in &sm.notes {
            c = c.child(muted(format!("· {n}")));
        }
        c = c
            .child(
                row()
                    .child(btn("Apply to workbench", Kind::Primary, move || {
                        s.apply_tuned()
                    }))
                    .child(btn("Save profile…", Kind::Secondary, move || {
                        crate::ui::files::save_tuned_profile(s)
                    }))
                    .child(btn("Save Rust code…", Kind::Secondary, move || {
                        crate::ui::files::save_tuned_rust(s)
                    }))
                    .child(muted(move || s.notice.get().unwrap_or_default())),
            )
            .child(
                view()
                    .p_3()
                    .rounded_md()
                    .bg(move || theme().muted)
                    .child(mono(
                        t.rust
                            .lines()
                            .skip_while(|l| !l.starts_with("pub fn build_operator"))
                            .take(12)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )),
            );
        c.into_element()
    })
}

pub fn tune_page() -> impl IntoElement {
    let s = store();
    scroll().flex_1().child(
        view()
            .p_6()
            .gap_4()
            .child(title("Tune"))
            .child(muted(
                "Real trials against your target: a concurrency ladder that stops at the 429/503 ceiling, then the knobs that matter for this workload, then a confirmation run.",
            ))
            .child(run_form("Start tuning", move || store().tune()))
            .child(
                card("Search")
                    .child(
                        row()
                            .items_end()
                            .child(picker("Objective", &[("throughput", "Max throughput"), ("latency", "Min p99 latency"), ("cost", "Min cost per op")], s.objective).w(px(200.0)))
                            .child(field("Trial length", "e.g. 5s", s.trial_secs).w(px(130.0)))
                            .child(field("Max trials", "e.g. 24", s.max_trials).w(px(110.0))),
                    ),
            )
            .child(failure_banner())
            .child(result())
            .child(trials()),
    )
}
