//! The main screen (design v5 §9 "Main screen: target Health"): one target's health at a
//! glance — live tiles with the last runs as sparklines and the baseline as a band, the chosen
//! limit hypothesis in one sentence, one primary action with its predicted outcome, and the
//! run history as an annotated timeline. Plain Rust from the run summaries and the latest
//! run's report; the view only draws it.

use crate::analysis::hypothesis::{Hypothesis, HypothesisKind};
use crate::model::{Finding, Report};
use crate::store::RunSummary;
use crate::ui::model::fmt;
use crate::ui::model::i18n::{tr, trf};
use crate::ui::model::verdicts::{self, Level};

/// How many runs the sparklines and the history show.
pub const HISTORY: usize = 20;

/// One live tile.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    /// `ops`, `p99`, `throttled`, `cost`.
    pub key: &'static str,
    pub title: &'static str,
    pub value: String,
    /// Oldest first (the latest run last); empty when the runs do not carry the metric.
    pub series: Vec<f64>,
    /// The baseline runs' range of the metric.
    pub band: Option<(f64, f64)>,
    pub band_text: Option<String>,
    pub level: Level,
}

/// The primary action: the most severe verifiable finding's fix, with what it predicts.
#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub code: String,
    pub label: String,
    pub predicted: Option<String>,
}

/// One run in the history timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryRow {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub verdict: &'static str,
    pub level: Level,
    pub annotations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Health {
    /// The target's name (or display URL).
    pub target: String,
    pub latest: Option<String>,
    pub tiles: Vec<Tile>,
    pub hypothesis: String,
    /// Other hypotheses that were considered (kind and confidence), when the analysis fitted
    /// several.
    pub alternatives: Vec<String>,
    pub action: Option<Action>,
    pub history: Vec<HistoryRow>,
}

/// The key runs are grouped by in the sidebar and here: the target's name, else its URL.
pub fn target_key(r: &RunSummary) -> String {
    r.target_name
        .clone()
        .unwrap_or_else(|| r.target_display.clone())
}

fn band(values: &[f64]) -> Option<(f64, f64)> {
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
}

/// The selected hypothesis in one sentence (`in-flight limit: throttling starts between 40 and
/// 60 requests in flight`), or that no limit was found.
pub fn hypothesis_sentence(hs: &[Hypothesis]) -> String {
    match crate::analysis::hypothesis::selected(hs) {
        Some(h) if h.kind != HypothesisKind::None => {
            let ev = h.evidence.first().cloned().unwrap_or_default();
            let conf = format!("{:?}", h.confidence).to_lowercase();
            if ev.is_empty() {
                format!("{} ({conf} confidence)", h.kind.label())
            } else {
                format!("{}: {ev} ({conf} confidence)", h.kind.label())
            }
        }
        _ => tr("app.no_hypothesis").to_owned(),
    }
}

/// The primary action of a report: its most severe finding with a verification.
pub fn action(r: &Report) -> Option<Action> {
    let f: &Finding = r
        .findings
        .iter()
        .filter(|f| f.verification.is_some())
        .max_by_key(|f| f.severity)?;
    let v = f.verification.as_ref()?;
    let predicted = v.expect.first().map(|e| {
        let name = verdicts::metric_name(e.metric.trim_start_matches("summary."));
        let value = match e.unit.as_str() {
            "ratio" => fmt::ratio(e.predicted),
            "ms" => fmt::dur_ms(e.predicted),
            "ops_per_s" => format!("{} ops/s", fmt::compact(e.predicted)),
            "mib_per_s" => format!("{} MiB/s", fmt::compact(e.predicted)),
            u => format!("{} {u}", fmt::sig3(e.predicted)),
        };
        trf("app.predicted", &[("what", &format!("{name} → {value}"))])
    });
    Some(Action {
        code: f.code.clone(),
        label: format!("{}: {}", tr("app.primary_fix"), f.advice),
        predicted,
    })
}

/// What changed from run `prev` (older) to `r`, in words, plus verify and tuning marks.
pub fn annotations(r: &RunSummary, prev: Option<&RunSummary>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = prev {
        let mut what = Vec::new();
        if p.target_id != r.target_id {
            what.push("target");
        }
        if p.workload_hash != r.workload_hash {
            what.push(match (&p.preset, &r.preset) {
                (Some(a), Some(b)) if a != b => "preset",
                _ => "workload",
            });
        }
        if !what.is_empty() {
            out.push(trf("app.annotation.config", &[("what", &what.join(", "))]));
        }
    }
    let label = r.label.as_deref().unwrap_or("").to_lowercase();
    if label.starts_with("verify") {
        if let Some(p) = prev {
            out.push(trf("app.annotation.verify", &[("run", &p.id)]));
        }
    } else if label.starts_with("tuned") {
        out.push(tr("app.annotation.tuned").to_owned());
    }
    out
}

/// A run's descriptive row (U10): its date and time, preset and verdict, and its numbers —
/// the id only once, where it is needed.
pub fn row(r: &RunSummary, today: &str) -> (String, String) {
    let when = if fmt::day_of(&r.started_at) == today {
        format!("Today {}", fmt::short_time(&r.started_at, today))
    } else {
        fmt::short_time(&r.started_at, today)
    };
    let (verdict, _) = verdicts::run_level(r.verdict_level);
    let title = match &r.label {
        Some(l) => format!("{l} · {when}"),
        None => when,
    };
    let mut detail = vec![];
    if let Some(p) = &r.preset {
        detail.push(p.clone());
    }
    detail.push(verdict.to_owned());
    detail.push(crate::ui::model::runs::row_numbers(r));
    (title, detail.join(" · "))
}

/// The health of target `key` from the runs (newest first), the baselines and the latest run's
/// report (when loaded).
pub fn health(
    key: &str,
    runs: &[RunSummary],
    baselines: &[(String, Vec<String>)],
    latest: Option<&Report>,
    today: &str,
) -> Health {
    let mine: Vec<&RunSummary> = runs
        .iter()
        .filter(|r| target_key(r) == key)
        .take(HISTORY)
        .collect();
    let base_ids: Vec<&String> = baselines.iter().flat_map(|(_, ids)| ids).collect();
    let base_runs: Vec<&RunSummary> = runs
        .iter()
        .filter(|r| target_key(r) == key && base_ids.contains(&&r.id))
        .collect();
    let series = |f: &dyn Fn(&RunSummary) -> Option<f64>| -> Vec<f64> {
        let v: Vec<f64> = mine.iter().rev().filter_map(|r| f(r)).collect();
        if v.len() >= 2 { v } else { Vec::new() }
    };
    let bands = |f: &dyn Fn(&RunSummary) -> Option<f64>| -> Option<(f64, f64)> {
        band(&base_runs.iter().filter_map(|r| f(r)).collect::<Vec<_>>())
    };
    let first = mine.first().copied();
    let ops =
        |r: &RunSummary| (!crate::ui::model::runs::stopped_in_warmup(r)).then_some(r.ops_per_s);
    let p99 = |r: &RunSummary| (!crate::ui::model::runs::stopped_in_warmup(r)).then_some(r.p99_ms);
    let thr = |r: &RunSummary| r.throttled_request_ratio;
    let cost = |r: &RunSummary| r.cost_per_month_usd;
    let level_of = |metric: &str| {
        latest
            .map(|rep| verdicts::metric_level(rep, metric))
            .unwrap_or(Level::Neutral)
    };
    let mk = |key: &'static str,
              title: &'static str,
              f: &dyn Fn(&RunSummary) -> Option<f64>,
              show: &dyn Fn(f64) -> String,
              level: Level| {
        let b = bands(f);
        Tile {
            key,
            title,
            value: first.and_then(f).map(show).unwrap_or_else(|| "—".into()),
            series: series(f),
            band: b,
            band_text: b
                .map(|(lo, hi)| trf("app.tile.baseline", &[("lo", &show(lo)), ("hi", &show(hi))])),
            level,
        }
    };
    let tiles = vec![
        mk(
            "ops",
            tr("app.tile.ops"),
            &ops,
            &|v| fmt::compact(v),
            Level::Neutral,
        ),
        mk(
            "p99",
            tr("app.tile.p99"),
            &p99,
            &|v| fmt::dur_ms(v),
            level_of("p99_ms"),
        ),
        mk(
            "throttled",
            tr("app.tile.throttled"),
            &thr,
            &|v| fmt::ratio(v),
            level_of("throttled_request_ratio"),
        ),
        mk(
            "cost",
            tr("app.tile.cost"),
            &cost,
            &|v| fmt::usd(v),
            Level::Neutral,
        ),
    ];
    let history = mine
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (title, detail) = row(r, today);
            let (verdict, level) = verdicts::run_level(r.verdict_level);
            HistoryRow {
                id: r.id.clone(),
                title,
                detail,
                verdict,
                level,
                annotations: annotations(r, mine.get(i + 1).copied()),
            }
        })
        .collect();
    let hs = latest
        .map(|r| r.capacity.hypotheses.as_slice())
        .unwrap_or(&[]);
    Health {
        target: key.to_owned(),
        latest: first.map(|r| r.id.clone()),
        tiles,
        hypothesis: hypothesis_sentence(hs),
        alternatives: hs
            .iter()
            .filter(|h| !h.selected && h.kind != HypothesisKind::None)
            .map(|h| {
                format!(
                    "{} ({:?}, explains {})",
                    h.kind.label(),
                    h.confidence,
                    fmt::ratio(h.explains_ratio)
                )
                .to_lowercase()
            })
            .collect(),
        action: latest.and_then(action),
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::runs::tests::run;

    fn runs() -> Vec<RunSummary> {
        let mut v = Vec::new();
        for i in 0..4 {
            let mut r = run(&format!("r{i}"), "s3://b", if i == 1 { "w2" } else { "w1" });
            r.target_name = Some("prod".into());
            r.started_at = format!("2026-11-2{i}T10:00:00Z");
            r.ops_per_s = 100.0 + i as f64;
            r.throttled_request_ratio = Some(0.01 * i as f64);
            r.preset = Some("parquet".into());
            v.push(r);
        }
        v.reverse(); // newest first
        v
    }

    #[test]
    fn tiles_carry_sparklines_and_the_baseline_band() {
        let rs = runs();
        let h = health(
            "prod",
            &rs,
            &[("main".into(), vec!["r0".into(), "r1".into()])],
            None,
            "2026-11-23",
        );
        assert_eq!(h.latest.as_deref(), Some("r3"));
        let ops = &h.tiles[0];
        assert_eq!(ops.value, "103");
        assert_eq!(ops.series, vec![100.0, 101.0, 102.0, 103.0]);
        assert_eq!(ops.band, Some((100.0, 101.0)));
        assert_eq!(h.tiles[2].series.len(), 4);
        // No cost in the summaries: no sparkline, a dash.
        assert!(h.tiles[3].series.is_empty());
        assert_eq!(h.tiles[3].value, "—");
        assert_eq!(h.history.len(), 4);
        assert_eq!(h.history[0].title, "Today 10:00");
        assert!(
            h.history[0].detail.starts_with("parquet · Healthy · "),
            "{}",
            h.history[0].detail
        );
        // r1 changed the workload against r0; r2 changed it back.
        assert_eq!(h.history[2].annotations, vec!["config changed: workload"]);
        assert_eq!(h.history[1].annotations, vec!["config changed: workload"]);
        assert!(h.history[3].annotations.is_empty());
        assert!(h.hypothesis.starts_with("No limit identified"));
    }

    #[test]
    fn the_report_gives_the_hypothesis_and_the_primary_action() {
        let mut rep = crate::report::fixture::report();
        rep.capacity.hypotheses = vec![
            Hypothesis {
                kind: HypothesisKind::RateLimit,
                evidence: vec!["throttling starts at 3 400 requests/s".into()],
                selected: true,
                ..Default::default()
            },
            Hypothesis {
                kind: HypothesisKind::InflightLimit,
                explains_ratio: 0.4,
                ..Default::default()
            },
        ];
        let h = health("prod", &runs(), &[], Some(&rep), "2026-11-23");
        assert!(
            h.hypothesis
                .starts_with("rate limit: throttling starts at 3 400"),
            "{}",
            h.hypothesis
        );
        assert_eq!(h.alternatives.len(), 1);
        assert!(
            h.alternatives[0].starts_with("in-flight limit"),
            "{:?}",
            h.alternatives
        );
        if rep.findings.iter().any(|f| f.verification.is_some()) {
            let a = h.action.unwrap();
            assert!(a.label.starts_with("Fix: "), "{}", a.label);
        }
    }

    #[test]
    fn verify_and_tuned_runs_are_annotated() {
        let mut a = run("a", "t", "w");
        let mut b = run("b", "t", "w");
        b.label = Some("verify throttling".into());
        assert_eq!(annotations(&b, Some(&a)), vec!["verify of a"]);
        a.label = Some("tuned settings".into());
        assert_eq!(annotations(&a, None), vec!["tuned settings"]);
    }
}

/// A sparkline of `values` in block characters (`▁▂▃▅▇`), oldest first: honest in a terminal
/// and a window alike (no braille grid — U4).
pub fn sparkline(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() || !hi.is_finite() {
        return String::new();
    }
    values
        .iter()
        .map(|v| {
            if hi - lo < 1e-12 {
                BARS[3]
            } else {
                BARS[(((v - lo) / (hi - lo)) * 7.0).round().clamp(0.0, 7.0) as usize]
            }
        })
        .collect()
}

#[cfg(test)]
mod spark_tests {
    #[test]
    fn sparklines_span_their_range() {
        assert_eq!(super::sparkline(&[1.0, 2.0, 3.0]), "▁▅█");
        assert_eq!(super::sparkline(&[5.0, 5.0]), "▄▄");
        assert_eq!(super::sparkline(&[]), "");
    }
}
