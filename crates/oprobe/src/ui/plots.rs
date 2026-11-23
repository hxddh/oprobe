//! The plots the views draw, built from the app's models: the timeline (goodput with the
//! attempted rate behind it and the intervals with throttled responses or failures shaded, the
//! latency quantiles muted where an interval has too few operations, requests and retries),
//! the latency distribution, the in-flight levels behind a concurrency ceiling, each finding's
//! evidence, Prove's convergence and the overlays of Compare. Plain functions of plain values,
//! so each is tested without a window.

use crate::analysis::capacity::Confidence;
use crate::app::ProveObjective;
use crate::app::prove::{Converged, convergence};
use crate::report::view::{ConcurrencyView, EvidenceChart, LatencyCdf, TimelineView};
use crate::ui::chart::{Band, Line, Plot, Rule, Unit};
use crate::ui::data::{Heat, ProfileModel};
use crate::ui::fmt::Scale;
use crate::ui::i18n::{tr, trf};

/// Series colours in order, for one line per run.
pub const SERIES: [&str; 5] = ["series-a", "series-b", "chart-3", "chart-2", "chart-5"];

/// The end of the timeline's last interval.
fn window(t: &TimelineView) -> f64 {
    match (t.t_s.last(), t.len_s.last()) {
        (Some(s), Some(l)) => s + l,
        _ => 1.0,
    }
}

/// `(t, v)` of a series the view carries (empty when it does not).
fn series(t: &TimelineView, v: &[f64]) -> Vec<(f64, f64)> {
    t.t_s.iter().copied().zip(v.iter().copied()).collect()
}

/// A per-interval count as a rate.
fn per_s(t: &TimelineView, c: &[u64]) -> Vec<f64> {
    c.iter()
        .zip(&t.len_s)
        .map(|(n, l)| *n as f64 / l.max(1e-3))
        .collect()
}

/// A step band over the full height of `y` for the intervals where `on` holds.
fn shade(t: &TimelineView, y: &Scale, on: impl Fn(usize) -> bool) -> Vec<(f64, f64, f64)> {
    t.t_s
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, y.min, if on(i) { y.max } else { y.min }))
        .collect()
}

fn time_plot(t: &TimelineView, y: Scale, unit: Unit, lines: Vec<Line>) -> Plot {
    let mut p = Plot::over_time(window(t), y, unit, lines);
    p.step_w = t.len_s.last().copied().unwrap_or(1.0);
    p
}

/// Goodput over the run (successful operations), the attempted rate (failures included) as a
/// faint line behind it, and the intervals with throttled responses or failed operations
/// shaded.
pub fn goodput(t: &TimelineView) -> Option<Plot> {
    if t.is_empty() {
        return None;
    }
    let top = t
        .ops_per_s
        .iter()
        .chain(&t.goodput_ops_per_s)
        .copied()
        .fold(0.0, f64::max);
    let y = Scale::zero_based(top);
    let mut lines = vec![Line::step(
        tr("metric.goodput"),
        "series-a",
        series(t, &t.goodput_ops_per_s),
    )];
    // The attempted rate only when it differs (failures happened).
    if t.ops_per_s
        .iter()
        .zip(&t.goodput_ops_per_s)
        .any(|(a, g)| (a - g).abs() > 1e-9)
    {
        lines.push(Line::step(tr("plot.attempted"), "axis", series(t, &t.ops_per_s)).faint());
    }
    let mut plot = time_plot(t, y, Unit::Rate, lines);
    if t.throttled_count.iter().any(|n| *n > 0) {
        plot.bands.push(Band {
            label: tr("plot.throttled_responses").to_owned(),
            token: "warn",
            pts: shade(t, &y, |i| t.throttled_count[i] > 0),
            step: true,
            alpha: 0.14,
        });
    }
    if t.errors_count.iter().any(|n| *n > 0) {
        plot.bands.push(Band {
            label: tr("plot.error_secs").to_owned(),
            token: "crit",
            pts: shade(t, &y, |i| t.errors_count[i] > 0),
            step: true,
            alpha: 0.14,
        });
    }
    Some(plot)
}

/// p50, p99 and p99.9 over the run on a log axis. An interval with too few operations for a
/// quantile ([`TimelineView::reliable`]) has that quantile drawn faint and dashed, and the
/// readout says so.
pub fn latency(t: &TimelineView) -> Option<Plot> {
    if t.is_empty() {
        return None;
    }
    let lo = t
        .p50_ms
        .iter()
        .copied()
        .filter(|v| *v > 0.0)
        .fold(f64::INFINITY, f64::min);
    let hi = t
        .p999_ms
        .iter()
        .chain(&t.p99_ms)
        .copied()
        .fold(0.1, f64::max);
    let qs = [
        ("p50", 0.5, "series-a", &t.p50_ms),
        ("p99", 0.99, "stage-server", &t.p99_ms),
        ("p99.9", 0.999, "stage-stall", &t.p999_ms),
    ];
    let lines = qs
        .iter()
        .map(|(label, q, tok, v)| {
            let keep: Vec<usize> = (0..v.len()).filter(|i| v[*i] > 0.0).collect();
            let reliable = t.reliable(*q);
            Line::step(
                *label,
                tok,
                keep.iter().map(|i| (t.t_s[*i], v[*i])).collect(),
            )
            .weak(
                keep.iter()
                    .map(|i| !reliable.get(*i).copied().unwrap_or(true))
                    .collect(),
            )
        })
        .collect();
    let mut plot = time_plot(
        t,
        Scale::log(if lo.is_finite() { lo } else { 0.1 }, hi),
        Unit::Ms,
        lines,
    );
    plot.notes = too_few(t);
    Some(plot)
}

/// Per interval with too few operations for a quantile: `n=12, too few for p99`.
pub fn too_few(t: &TimelineView) -> Vec<(f64, f64, String)> {
    let names = [(0.5, "p50"), (0.99, "p99"), (0.999, "p99.9")];
    (0..t.len())
        .filter_map(|i| {
            let n = t.ops_count.get(i).copied()?;
            let first = names
                .iter()
                .find(|(q, _)| n < TimelineView::min_ops_for(*q))?;
            Some((
                t.t_s[i],
                t.t_s[i] + t.len_s[i],
                trf("plot.too_few", &[&n, &first.1]),
            ))
        })
        .collect()
}

/// One per-interval series over the run.
fn per_interval(
    t: &TimelineView,
    label: &str,
    token: &'static str,
    unit: Unit,
    v: Vec<f64>,
) -> Option<Plot> {
    if t.is_empty() || v.len() != t.len() {
        return None;
    }
    let y = Scale::zero_based(v.iter().copied().fold(0.0, f64::max));
    Some(time_plot(
        t,
        y,
        unit,
        vec![Line::step(label, token, series(t, &v))],
    ))
}

/// The most requests in flight at once, interval by interval (where a concurrency limit
/// shows).
pub fn inflight(t: &TimelineView) -> Option<Plot> {
    if t.inflight_peak_count.iter().all(|n| *n == 0) {
        return None;
    }
    per_interval(
        t,
        tr("plot.peak"),
        "series-a",
        Unit::Count,
        t.inflight_peak_count.iter().map(|n| *n as f64).collect(),
    )
}

/// Requests sent per second (retries included) and the retries per second: where a request
/// rate limit shows.
pub fn requests(t: &TimelineView) -> Option<Plot> {
    if t.requests_per_s.len() != t.len() || t.requests_per_s.iter().all(|v| *v <= 0.0) {
        return None;
    }
    let retries = per_s(t, &t.retries_count);
    let y = Scale::zero_based(t.requests_per_s.iter().copied().fold(0.0, f64::max));
    let mut lines = vec![Line::step(
        tr("plot.requests"),
        "series-a",
        series(t, &t.requests_per_s),
    )];
    if retries.iter().any(|v| *v > 0.0) {
        lines.push(Line::step(
            tr("plot.retries"),
            "stage-retry",
            series(t, &retries),
        ));
    }
    Some(time_plot(t, y, Unit::Rate, lines))
}

/// Throttled responses per second (each in the interval it arrived).
pub fn throttled_rate(t: &TimelineView) -> Option<Plot> {
    per_interval(
        t,
        tr("plot.throttled_responses"),
        "warn",
        Unit::Rate,
        per_s(t, &t.throttled_count),
    )
}

/// Failed operations per second.
pub fn errors_rate(t: &TimelineView) -> Option<Plot> {
    per_interval(
        t,
        tr("metric.errors"),
        "crit",
        Unit::Rate,
        per_s(t, &t.errors_count),
    )
}

/// Bandwidth over the run.
pub fn bandwidth(t: &TimelineView) -> Option<Plot> {
    per_interval(
        t,
        tr("metric.bandwidth"),
        "series-a",
        Unit::MibRate,
        t.mib_per_s.clone(),
    )
}

/// The heatmap's colour key: the fewest operations in a non-empty cell and the most.
pub fn heat_legend(h: &Heat) -> (u64, u64) {
    let lo = h
        .counts
        .iter()
        .flatten()
        .copied()
        .filter(|n| *n > 0)
        .min()
        .unwrap_or(0);
    (lo, h.max.max(lo))
}

/// Latency distributions on one log axis: one line per `(label, cdf)`; `marks` puts the p50
/// and p99 of a single distribution on it as rules.
pub fn cdf(series: &[(String, &LatencyCdf)], marks: Option<(f64, f64)>) -> Option<Plot> {
    let all = series.iter().flat_map(|(_, c)| c.pts.iter());
    let (lo, hi) = all.fold((f64::INFINITY, 0.0_f64), |(lo, hi), p| {
        (if p.0 > 0.0 { lo.min(p.0) } else { lo }, hi.max(p.0))
    });
    if hi <= 0.0 {
        return None;
    }
    let x = Scale::log(if lo.is_finite() { lo } else { hi / 1e3 }, hi);
    let mut plot = Plot {
        x,
        y: Scale {
            min: 0.0,
            max: 1.0,
            log: false,
        },
        step_w: 0.0,
        x_fmt: Unit::Ms,
        y_fmt: Unit::Share,
        lines: series
            .iter()
            .zip(SERIES.iter().cycle())
            .filter(|((_, c), _)| !c.pts.is_empty())
            .map(|((label, c), tok)| {
                // Start on the floor at the first latency, so the curve rises from zero.
                let mut pts = vec![(c.pts[0].0, 0.0)];
                pts.extend(c.pts.iter().copied());
                Line::joined(label.clone(), tok, pts)
            })
            .collect(),
        bands: Vec::new(),
        rules: Vec::new(),
        notes: Vec::new(),
    };
    if let Some((p50, p99)) = marks {
        if p50 > 0.0 {
            plot.rules.push(Rule::at_x(p50, "axis", "p50"));
        }
        if p99 > 0.0 {
            plot.rules.push(Rule::at_x(p99, "stage-server", "p99"));
        }
    }
    Some(plot)
}

/// The run's own latency distribution with its p50 and p99 marked.
pub fn own_cdf(m: &ProfileModel) -> Option<Plot> {
    cdf(
        &[(tr("plot.ops").to_owned(), &m.cdf)],
        Some((m.p50_ms, m.p99_ms)),
    )
}

/// The share throttled at each in-flight level, the fitted ceiling's interval shaded and its
/// estimate ruled.
pub fn concurrency(c: &ConcurrencyView) -> Option<Plot> {
    if c.levels.is_empty() {
        return None;
    }
    let top = c.levels.iter().map(|l| l.inflight_count).max().unwrap_or(1) as f64;
    let top = c.ceiling.map(|(_, _, hi)| hi.max(top)).unwrap_or(top) + 1.0;
    let y = Scale {
        min: 0.0,
        max: 1.0,
        log: false,
    };
    let mut plot = Plot {
        x: Scale::zero_based(top),
        y,
        step_w: 1.0,
        x_fmt: Unit::Count,
        y_fmt: Unit::Share,
        lines: vec![Line::dots(
            tr("plot.throttled_share"),
            "warn",
            c.levels
                .iter()
                .map(|l| (l.inflight_count as f64, l.throttled_ratio))
                .collect(),
        )],
        bands: Vec::new(),
        rules: Vec::new(),
        notes: Vec::new(),
    };
    if let Some((est, lo, hi)) = c.ceiling {
        plot.bands.push(Band {
            label: tr("plot.limit_interval").to_owned(),
            token: if c.confidence == Confidence::High {
                "series-a"
            } else {
                "inconclusive"
            },
            pts: vec![(lo, y.min, y.max), (hi.max(lo), y.min, y.max)],
            step: false,
            alpha: 0.14,
        });
        plot.rules.push(Rule::at_x(
            est,
            "series-a",
            trf("plot.limit", &[&est.round()]),
        ));
    }
    Some(plot)
}

/// A finding's evidence chart: its heading and plot (`None` when the run lacks the data).
pub fn evidence(m: &ProfileModel, kind: EvidenceChart) -> Option<(&'static str, Plot)> {
    match kind {
        EvidenceChart::Concurrency => m
            .concurrency
            .as_ref()
            .and_then(concurrency)
            .map(|p| (tr("plot.by_inflight"), p)),
        EvidenceChart::ThrottledOverTime => {
            throttled_rate(&m.timeline).map(|p| (tr("plot.throttled_rate"), p))
        }
        EvidenceChart::Bandwidth => bandwidth(&m.timeline).map(|p| (tr("metric.bandwidth"), p)),
        EvidenceChart::Latency => own_cdf(m).map(|p| {
            (
                if m.cdf.exact {
                    tr("plot.cdf")
                } else {
                    tr("plot.cdf_buckets")
                },
                p,
            )
        }),
        EvidenceChart::ErrorsOverTime => {
            errors_rate(&m.timeline).map(|p| (tr("plot.errors_rate"), p))
        }
        // Tables, not charts: the view draws them.
        EvidenceChart::HotKeys | EvidenceChart::Amplification => None,
    }
}

/// `e^±floor − 1`: the noise floor as a relative change either way.
fn noise_band(floor: f64) -> (f64, f64) {
    ((-floor).exp() - 1.0, floor.exp() - 1.0)
}

/// Prove's convergence: each pair's change (candidate / baseline − 1) as a dot, the running
/// estimate with its 95 % interval narrowing as pairs come in, the A/A noise floor around no
/// change, and no change ruled. `ab` holds the objective of each pair and the fewest operations
/// its trials counted (the noise floor is never below that resolution, as in the verdict).
pub fn prove(ab: &[(f64, f64, u64)], p99: bool) -> Option<Plot> {
    if ab.is_empty() {
        return None;
    }
    let obj = if p99 {
        ProveObjective::P99
    } else {
        ProveObjective::Goodput
    };
    let (a, b): (Vec<f64>, Vec<f64>) = ab.iter().map(|p| (p.0, p.1)).unzip();
    let ops: Vec<u64> = ab
        .iter()
        .scan(u64::MAX, |m, p| {
            *m = (*m).min(p.2);
            Some(*m)
        })
        .collect();
    let conv: Vec<Converged> = convergence(obj, &a, &b, &ops);
    let n = ab.len() as f64;
    let d: Vec<(f64, f64)> = ab
        .iter()
        .enumerate()
        .map(|(i, (a, b, _))| ((i + 1) as f64, if *a > 0.0 { b / a - 1.0 } else { 0.0 }))
        .collect();
    let floor = conv.last().map(|c| c.noise_floor_ratio).unwrap_or(0.0);
    let (nlo, nhi) = noise_band(floor);
    // The first interval (one pair) is very wide: the axis follows the dots and the later
    // intervals, so the narrowing stays readable (the clip cuts the first).
    let later = conv.iter().skip(if conv.len() > 2 { 1 } else { 0 });
    let lo = d
        .iter()
        .map(|p| p.1)
        .chain(later.clone().map(|c| c.interval.low))
        .fold(nlo, f64::min);
    let hi = d
        .iter()
        .map(|p| p.1)
        .chain(later.map(|c| c.interval.high))
        .fold(nhi, f64::max);
    let x = Scale {
        min: 0.5,
        max: n + 0.5,
        log: false,
    };
    Some(Plot {
        x,
        y: Scale::span(lo, hi),
        step_w: 1.0,
        x_fmt: Unit::Count,
        y_fmt: Unit::Change,
        lines: vec![
            Line::joined(
                tr("plot.estimate"),
                "series-a",
                conv.iter()
                    .enumerate()
                    .map(|(i, c)| ((i + 1) as f64, c.interval.estimate))
                    .collect(),
            ),
            Line::dots(tr("plot.pair_delta"), "series-b", d),
        ],
        bands: vec![
            Band {
                label: tr("plot.noise").to_owned(),
                token: "inconclusive",
                pts: vec![(x.min, nlo, nhi), (x.max, nlo, nhi)],
                step: false,
                alpha: 0.12,
            },
            Band {
                label: tr("plot.interval").to_owned(),
                token: "series-a",
                pts: conv
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ((i + 1) as f64, c.interval.low, c.interval.high))
                    .collect(),
                step: false,
                alpha: 0.18,
            },
        ],
        rules: vec![Rule::at_y(0.0, "axis", "")],
        notes: Vec::new(),
    })
}

/// Several runs' goodput over time from their own start, one line each.
pub fn goodput_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    let w = ms.iter().map(|m| window(&m.timeline)).fold(0.0, f64::max);
    let top = ms
        .iter()
        .flat_map(|m| m.timeline.goodput_ops_per_s.iter().copied())
        .fold(0.0, f64::max);
    if w <= 0.0 || ms.iter().all(|m| m.timeline.is_empty()) {
        return None;
    }
    Some(Plot::over_time(
        w,
        Scale::zero_based(top),
        Unit::Rate,
        ms.iter()
            .zip(SERIES.iter().cycle())
            .map(|(m, tok)| {
                Line::step(
                    m.id.clone(),
                    tok,
                    series(&m.timeline, &m.timeline.goodput_ops_per_s),
                )
            })
            .collect(),
    ))
}

/// Several runs' latency distributions, one line each.
pub fn cdf_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    let series: Vec<(String, &LatencyCdf)> = ms.iter().map(|m| (m.id.clone(), &m.cdf)).collect();
    cdf(&series, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chart::Mark;

    /// Three 1 s intervals: `(attempted, goodput, throttled responses, failures, operations)`.
    fn tl(rows: &[(f64, f64, u64, u64, u64)]) -> TimelineView {
        let n = rows.len();
        TimelineView {
            t_s: (0..n).map(|i| i as f64).collect(),
            len_s: vec![1.0; n],
            ops_per_s: rows.iter().map(|r| r.0).collect(),
            goodput_ops_per_s: rows.iter().map(|r| r.1).collect(),
            mib_per_s: rows.iter().map(|r| r.0 / 10.0).collect(),
            p50_ms: vec![5.0; n],
            p90_ms: vec![30.0; n],
            p99_ms: vec![50.0; n],
            p999_ms: vec![80.0; n],
            max_ms: vec![120.0; n],
            ops_count: rows.iter().map(|r| r.4).collect(),
            throttled_count: rows.iter().map(|r| r.2).collect(),
            errors_count: rows.iter().map(|r| r.3).collect(),
            requests_per_s: rows.iter().map(|r| r.0 * 1.1).collect(),
            retries_count: rows.iter().map(|r| r.2).collect(),
            inflight_peak_count: vec![4; n],
            new_connections_count: vec![0; n],
            verb: None,
        }
    }

    #[test]
    fn the_timeline_shades_only_the_intervals_with_throttled_responses() {
        let t = tl(&[
            (100.0, 100.0, 0, 0, 100),
            (80.0, 80.0, 4, 0, 80),
            (90.0, 90.0, 0, 0, 90),
        ]);
        let p = goodput(&t).unwrap();
        assert_eq!(p.lines.len(), 1, "no failures: no attempted line");
        assert_eq!(p.bands.len(), 1, "no errors, no error band");
        let b = &p.bands[0];
        assert_eq!(b.label, tr("plot.throttled_responses"));
        assert!(b.step);
        assert_eq!(b.pts[0].1, b.pts[0].2, "a clean interval draws nothing");
        assert_eq!((b.pts[1].1, b.pts[1].2), (p.y.min, p.y.max));
        assert!(goodput(&TimelineView::default()).is_none());
        assert_eq!(inflight(&t).unwrap().lines[0].pts[0], (0.0, 4.0));
        // Throttled responses, per second.
        let th = throttled_rate(&t).unwrap();
        assert_eq!(th.lines[0].pts[1], (1.0, 4.0));
        // Requests per second with the retries beside them.
        let rq = requests(&t).unwrap();
        assert_eq!(rq.lines.len(), 2);
        assert!((rq.lines[0].pts[0].1 - 110.0).abs() < 1e-9);
        assert_eq!(rq.lines[1].pts[1], (1.0, 4.0));
        assert!(rq.summary("Requests").contains("Requests"));
    }

    #[test]
    fn goodput_is_drawn_over_the_faint_attempted_rate() {
        let t = tl(&[(100.0, 100.0, 0, 0, 100), (100.0, 60.0, 0, 40, 100)]);
        let p = goodput(&t).unwrap();
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.lines[0].label, tr("metric.goodput"));
        assert!(!p.lines[0].faint);
        assert_eq!(p.lines[0].pts[1], (1.0, 60.0));
        assert_eq!(p.lines[1].label, tr("plot.attempted"));
        assert!(p.lines[1].faint);
        assert_eq!(p.lines[1].pts[1], (1.0, 100.0));
        assert_eq!(p.bands.len(), 1, "the failures are shaded");
        // The screen reader hears the goodput's range, not the attempted rate's.
        let s = p.summary("Goodput");
        assert!(s.starts_with("Goodput"), "{s}");
        assert!(s.contains("60") && s.contains("100/s"), "{s}");
    }

    #[test]
    fn quantiles_of_intervals_with_too_few_operations_are_muted() {
        // 2 000 operations: p99 holds (≥ 1 000), p99.9 does not (< 10 000); 25: only p50
        // (≥ 20).
        let t = tl(&[(2000.0, 2000.0, 0, 0, 2000), (25.0, 25.0, 0, 0, 25)]);
        let p = latency(&t).unwrap();
        let by = |l: &str| p.lines.iter().find(|x| x.label == l).unwrap();
        assert_eq!(by("p50").weak, vec![false, false]);
        assert_eq!(by("p99").weak, vec![false, true]);
        assert_eq!(by("p99.9").weak, vec![true, true]);
        assert_eq!(
            p.note_at(1.5),
            Some(trf("plot.too_few", &[&25, &"p99"]).as_str())
        );
        assert_eq!(
            p.note_at(0.5),
            Some(trf("plot.too_few", &[&2000, &"p99.9"]).as_str())
        );
        let r = crate::ui::chart::readout_at(&p, 1.5);
        assert!(r.contains("n=25, too few for p99"), "{r}");
    }

    #[test]
    fn the_heatmap_key_spans_the_fewest_to_the_most_in_a_cell() {
        let h = Heat {
            bucket_s: 1.0,
            bounds_ms: vec![0.0, 1.0, 10.0],
            counts: vec![vec![0, 3, 0], vec![0, 7, 40]],
            max: 40,
            span_s: 2.0,
            fine: true,
        };
        assert_eq!(heat_legend(&h), (3, 40));
        assert_eq!(heat_legend(&Heat::default()), (0, 0));
    }

    #[test]
    fn a_distribution_rises_from_zero_with_its_quantiles_ruled() {
        let c = LatencyCdf {
            pts: vec![(1.0, 0.5), (10.0, 0.99), (40.0, 1.0)],
            exact: true,
        };
        let p = cdf(&[("a".into(), &c)], Some((1.0, 10.0))).unwrap();
        assert!(p.x.log);
        assert_eq!(p.lines[0].pts[0], (1.0, 0.0));
        assert_eq!(p.lines[0].mark, Mark::Line);
        assert_eq!(p.rules.len(), 2);
        assert!(cdf(&[("a".into(), &LatencyCdf::default())], None).is_none());
    }

    #[test]
    fn the_concurrency_evidence_shades_the_limit_interval() {
        use crate::report::view::LevelPoint;
        let c = ConcurrencyView {
            levels: (1..=40)
                .map(|i| LevelPoint {
                    inflight_count: i,
                    requests_count: 100,
                    throttled_ratio: if i > 24 { 0.4 } else { 0.0 },
                })
                .collect(),
            ceiling: Some((24.0, 22.0, 26.0)),
            confidence: Confidence::High,
        };
        let p = concurrency(&c).unwrap();
        assert_eq!(p.lines[0].mark, Mark::Dots);
        assert_eq!(p.lines[0].pts.len(), 40);
        assert_eq!(p.bands[0].pts[0].0, 22.0);
        assert_eq!(p.bands[0].pts[1].0, 26.0);
        assert_eq!(p.rules[0].x, Some(24.0));
        assert!(p.x.max >= 41.0);
    }

    #[test]
    fn the_prove_chart_narrows_around_the_estimate() {
        let ab: Vec<(f64, f64, u64)> = (0..6)
            .map(|i| (100.0 + i as f64, 120.0 + (i % 3) as f64, 10_000))
            .collect();
        let p = prove(&ab, false).unwrap();
        let est = &p.lines[0].pts;
        let dots = &p.lines[1].pts;
        assert_eq!((est.len(), dots.len()), (6, 6));
        assert!((dots[0].1 - 0.2).abs() < 1e-9);
        let band = &p.bands[1].pts;
        let w = |i: usize| band[i].2 - band[i].1;
        assert!(w(5) < w(1));
        assert!(band[5].1 > 0.0, "six clear pairs exclude no change");
        assert_eq!(p.rules[0].y, Some(0.0));
        assert!(p.y.min <= 0.0 && p.y.max >= 0.2);
        // p99: lower is better; each dot is still candidate / baseline − 1.
        let q = prove(&[(50.0, 40.0, 10_000)], true).unwrap();
        assert!((q.lines[1].pts[0].1 + 0.2).abs() < 1e-9);
        assert!(prove(&[], false).is_none());
        // Few operations per trial: the noise floor is their resolution, as in the verdict.
        let few: Vec<(f64, f64, u64)> = [
            (37.0, 36.0),
            (37.0, 35.0),
            (37.0, 37.0),
            (38.0, 37.0),
            (37.0, 35.0),
        ]
        .iter()
        .map(|(a, b)| (*a, *b, 35))
        .collect();
        let f = prove(&few, false).unwrap();
        let last = f.bands[1].pts.last().unwrap();
        assert!(last.1 < 0.0 && last.2 > 0.0, "{last:?}");
    }
}
