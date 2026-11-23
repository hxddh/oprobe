//! The plots the views draw, built from the app's models: the timeline (with the seconds that
//! were throttled or failed shaded), the latency distribution, the in-flight levels behind a
//! concurrency ceiling, each finding's evidence, Prove's convergence and the overlays of
//! Compare. Plain functions of plain values, so each is tested without a window.

use crate::analysis::capacity::Confidence;
use crate::app::ProveObjective;
use crate::app::prove::{Converged, convergence};
use crate::report::view::{ConcurrencyView, EvidenceChart, LatencyCdf};
use crate::ui::chart::{Band, Line, Plot, Rule, Unit};
use crate::ui::data::{Point, ProfileModel};
use crate::ui::fmt::Scale;
use crate::ui::i18n::{tr, trf};

/// Series colours in order, for one line per run.
pub const SERIES: [&str; 5] = ["series-a", "series-b", "chart-3", "chart-2", "chart-5"];

fn window(pts: &[Point]) -> f64 {
    pts.last().map(|p| p.t_s + p.len_s).unwrap_or(1.0)
}

/// A step band over the full height of `y` for the slices where `on` holds.
fn shade(pts: &[Point], y: &Scale, on: impl Fn(&Point) -> bool) -> Vec<(f64, f64, f64)> {
    pts.iter()
        .map(|p| {
            let hi = if on(p) { y.max } else { y.min };
            (p.t_s, y.min, hi)
        })
        .collect()
}

/// Goodput over the run, the throttled and failed seconds shaded under it.
pub fn goodput(pts: &[Point]) -> Option<Plot> {
    if pts.is_empty() {
        return None;
    }
    let y = Scale::zero_based(pts.iter().map(|p| p.ops_per_s).fold(0.0, f64::max));
    let mut plot = Plot::over_time(
        window(pts),
        y,
        Unit::Rate,
        vec![Line::step(
            tr("metric.goodput"),
            "series-a",
            pts.iter().map(|p| (p.t_s, p.ops_per_s)).collect(),
        )],
    );
    if pts.iter().any(|p| p.throttled > 0) {
        plot.bands.push(Band {
            label: tr("plot.throttled_secs").to_owned(),
            token: "warn",
            pts: shade(pts, &y, |p| p.throttled > 0),
            step: true,
            alpha: 0.14,
        });
    }
    if pts.iter().any(|p| p.errors > 0) {
        plot.bands.push(Band {
            label: tr("plot.error_secs").to_owned(),
            token: "crit",
            pts: shade(pts, &y, |p| p.errors > 0),
            step: true,
            alpha: 0.14,
        });
    }
    Some(plot)
}

/// p50 and p99 over the run on a log axis.
pub fn latency(pts: &[Point]) -> Option<Plot> {
    if pts.is_empty() {
        return None;
    }
    let lo = pts
        .iter()
        .map(|p| p.p50_ms)
        .filter(|v| *v > 0.0)
        .fold(f64::INFINITY, f64::min);
    Some(Plot::over_time(
        window(pts),
        Scale::log(
            if lo.is_finite() { lo } else { 0.1 },
            pts.iter().map(|p| p.p99_ms).fold(0.1, f64::max),
        ),
        Unit::Ms,
        vec![
            Line::step(
                "p50",
                "series-a",
                pts.iter().map(|p| (p.t_s, p.p50_ms)).collect(),
            ),
            Line::step(
                "p99",
                "stage-server",
                pts.iter().map(|p| (p.t_s, p.p99_ms)).collect(),
            ),
        ],
    ))
}

/// One per-second series over the run.
fn per_second(
    pts: &[Point],
    label: &str,
    token: &'static str,
    unit: Unit,
    f: fn(&Point) -> f64,
) -> Option<Plot> {
    if pts.is_empty() {
        return None;
    }
    let y = Scale::zero_based(pts.iter().map(f).fold(0.0, f64::max));
    Some(Plot::over_time(
        window(pts),
        y,
        unit,
        vec![Line::step(
            label,
            token,
            pts.iter().map(|p| (p.t_s, f(p))).collect(),
        )],
    ))
}

/// Throttled responses per second.
pub fn throttled_rate(pts: &[Point]) -> Option<Plot> {
    per_second(
        pts,
        tr("plot.throttled_share"),
        "warn",
        Unit::Rate,
        Point::throttled_per_s,
    )
}

/// Failed operations per second.
pub fn errors_rate(pts: &[Point]) -> Option<Plot> {
    per_second(
        pts,
        tr("metric.errors"),
        "crit",
        Unit::Rate,
        Point::errors_per_s,
    )
}

/// Bandwidth over the run.
pub fn bandwidth(pts: &[Point]) -> Option<Plot> {
    per_second(
        pts,
        tr("metric.bandwidth"),
        "series-a",
        Unit::MibRate,
        |p| p.mib_per_s,
    )
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
    }
}

/// `e^±floor − 1`: the noise floor as a relative change either way.
fn noise_band(floor: f64) -> (f64, f64) {
    ((-floor).exp() - 1.0, floor.exp() - 1.0)
}

/// Prove's convergence: each pair's change (candidate / baseline − 1) as a dot, the running
/// estimate with its 95 % interval narrowing as pairs come in, the A/A noise floor around no
/// change, and no change ruled. `ab` holds the objective of each pair.
pub fn prove(ab: &[(f64, f64)], p99: bool) -> Option<Plot> {
    if ab.is_empty() {
        return None;
    }
    let obj = if p99 {
        ProveObjective::P99
    } else {
        ProveObjective::Goodput
    };
    let (a, b): (Vec<f64>, Vec<f64>) = ab.iter().copied().unzip();
    let conv: Vec<Converged> = convergence(obj, &a, &b);
    let n = ab.len() as f64;
    let d: Vec<(f64, f64)> = ab
        .iter()
        .enumerate()
        .map(|(i, (a, b))| ((i + 1) as f64, if *a > 0.0 { b / a - 1.0 } else { 0.0 }))
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
    })
}

/// Several runs' goodput over time from their own start, one line each.
pub fn goodput_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    let w = ms.iter().map(|m| window(&m.timeline)).fold(0.0, f64::max);
    let top = ms
        .iter()
        .flat_map(|m| m.timeline.iter().map(|p| p.ops_per_s))
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
                    m.timeline.iter().map(|p| (p.t_s, p.ops_per_s)).collect(),
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

    fn pt(t: f64, ops: f64, throttled: u64, errors: u64) -> Point {
        Point {
            t_s: t,
            len_s: 1.0,
            ops_per_s: ops,
            mib_per_s: ops / 10.0,
            p50_ms: 5.0,
            p99_ms: 50.0,
            errors,
            throttled,
        }
    }

    #[test]
    fn the_timeline_shades_only_the_seconds_that_were_throttled() {
        let pts = [
            pt(0.0, 100.0, 0, 0),
            pt(1.0, 80.0, 4, 0),
            pt(2.0, 90.0, 0, 0),
        ];
        let p = goodput(&pts).unwrap();
        assert_eq!(p.bands.len(), 1, "no errors, no error band");
        let b = &p.bands[0];
        assert!(b.step);
        assert_eq!(b.pts[0].1, b.pts[0].2, "a clean second draws nothing");
        assert_eq!((b.pts[1].1, b.pts[1].2), (p.y.min, p.y.max));
        assert!(goodput(&[]).is_none());
        let t = throttled_rate(&pts).unwrap();
        assert_eq!(t.lines[0].pts[1], (1.0, 4.0));
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
        let ab: Vec<(f64, f64)> = (0..6)
            .map(|i| (100.0 + i as f64, 120.0 + (i % 3) as f64))
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
        let q = prove(&[(50.0, 40.0)], true).unwrap();
        assert!((q.lines[1].pts[0].1 + 0.2).abs() < 1e-9);
        assert!(prove(&[], false).is_none());
    }
}
