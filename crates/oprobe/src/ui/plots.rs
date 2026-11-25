//! The plots the views draw, built from the app's models: the timeline (goodput with the
//! attempted rate behind it and the intervals with throttled responses or failures shaded, the
//! latency quantiles muted where an interval has too few operations, throughput, requests, and
//! the problems per second on their own axis), the latency distribution as a tail plot, the
//! in-flight levels behind a concurrency ceiling, each finding's
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
    let hi = t.max_ms.iter().copied().fold(hi, f64::max);
    let qs = [
        ("p50", 0.5, "series-a", &t.p50_ms),
        ("p99", 0.99, "stage-server", &t.p99_ms),
        ("p99.9", 0.999, "stage-stall", &t.p999_ms),
    ];
    let mut lines: Vec<Line> = qs
        .iter()
        .map(|(label, q, tok, v)| {
            let keep: Vec<usize> = (0..v.len()).filter(|i| v[*i] > 0.0).collect();
            let reliable = t.reliable(*q);
            let l = Line::step(
                *label,
                tok,
                keep.iter().map(|i| (t.t_s[*i], v[*i])).collect(),
            )
            .weak(
                keep.iter()
                    .map(|i| !reliable.get(*i).copied().unwrap_or(true))
                    .collect(),
            );
            // p99.9 often runs just above p99: dashed, so neither hides the other.
            if *label == "p99.9" { l.dashed() } else { l }
        })
        .collect();
    // The slowest operation of each interval, faint behind the quantiles.
    if t.max_ms.len() == t.len() && t.max_ms.iter().any(|v| *v > 0.0) {
        let keep: Vec<usize> = (0..t.len()).filter(|i| t.max_ms[*i] > 0.0).collect();
        lines.push(
            Line::step(
                tr("plot.max"),
                "axis",
                keep.iter().map(|i| (t.t_s[*i], t.max_ms[*i])).collect(),
            )
            .faint(),
        );
    }
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
/// shows), and the connections dialed in each interval (churn behind a latency spike), both
/// counts on one axis. The client's CPU has a chart of its own ([`cpu`]).
pub fn inflight(t: &TimelineView) -> Option<Plot> {
    if t.inflight_peak_count.len() != t.len() || t.inflight_peak_count.iter().all(|n| *n == 0) {
        return None;
    }
    let peak: Vec<f64> = t.inflight_peak_count.iter().map(|n| *n as f64).collect();
    let conns: Vec<f64> = t.new_connections_count.iter().map(|n| *n as f64).collect();
    let top = peak.iter().chain(&conns).copied().fold(0.0, f64::max);
    let mut lines = vec![Line::step(tr("plot.peak"), "series-a", series(t, &peak))];
    if conns.len() == t.len() && conns.iter().any(|v| *v > 0.0) {
        lines.push(Line::step(
            tr("plot.new_connections"),
            "stage-connect",
            series(t, &conns),
        ));
    }
    Some(time_plot(t, Scale::zero_based(top), Unit::Count, lines))
}

/// The client's CPU in cores busy, interval by interval (drawn when the client was saturated:
/// [`crate::report::view::TimelineLayout::cpu`]).
pub fn cpu(t: &TimelineView) -> Option<Plot> {
    let cpu = t.cpu_cores().filter(|c| c.len() == t.len())?;
    let top = cpu.iter().copied().fold(0.0, f64::max);
    Some(time_plot(
        t,
        Scale::zero_based(top.max(1.0)),
        Unit::Count,
        vec![Line::step(
            tr("plot.cpu_cores"),
            "series-a",
            series(t, &cpu),
        )],
    ))
}

/// A horizontal rule at `y` (a ceiling, a target), the axis grown to show it.
pub fn with_rule(mut p: Plot, y: f64, token: &'static str, label: String) -> Plot {
    if y > p.y.max && !p.y.log {
        p.y = Scale::zero_based(y);
    }
    let mut r = Rule::at_y(y, token, label);
    r.dashed = true;
    p.rules.push(r);
    p
}

/// Goodput read as throughput on a right axis: every operation moved `mib_per_op` MiB.
pub fn with_throughput_axis(mut p: Plot, mib_per_op: f64) -> Plot {
    p.y2 = Some((
        mib_per_op,
        Unit::MibRate,
        tr("plot.read_written").to_owned(),
    ));
    p
}

/// Requests sent per second, retries included (the problems have their own chart: on this
/// axis a few retries are flat at zero).
pub fn requests(t: &TimelineView) -> Option<Plot> {
    if t.requests_per_s.len() != t.len() || t.requests_per_s.iter().all(|v| *v <= 0.0) {
        return None;
    }
    let y = Scale::zero_based(t.requests_per_s.iter().copied().fold(0.0, f64::max));
    Some(time_plot(
        t,
        y,
        Unit::Rate,
        vec![Line::step(
            tr("plot.requests"),
            "series-a",
            series(t, &t.requests_per_s),
        )],
    ))
}

/// Problems per second on their own axis: retries, throttled responses and failed operations
/// (each in the interval it happened) — only those that happened; `None` when none did.
pub fn problems(t: &TimelineView) -> Option<Plot> {
    if t.is_empty() {
        return None;
    }
    let lines: Vec<Line> = [
        (tr("plot.retries"), "stage-retry", &t.retries_count),
        (tr("plot.throttled_responses"), "warn", &t.throttled_count),
        (tr("plot.failed_ops"), "crit", &t.errors_count),
        (tr("plot.missed"), "inconclusive", &t.missed_arrivals_count),
    ]
    .into_iter()
    .filter(|(_, _, c)| c.len() == t.len() && c.iter().any(|n| *n > 0))
    .map(|(label, token, c)| Line::step(label, token, series(t, &per_s(t, c))))
    .collect();
    if lines.is_empty() {
        return None;
    }
    let top = lines
        .iter()
        .flat_map(|l| l.pts.iter().map(|p| p.1))
        .fold(0.0, f64::max);
    Some(time_plot(t, Scale::zero_based(top), Unit::Rate, lines))
}

/// Bytes moved per second (read and written, as the operations asked for them), interval by
/// interval: the bandwidth tile over time. `None` when nothing moved.
pub fn throughput(t: &TimelineView) -> Option<Plot> {
    if t.mib_per_s.len() != t.len() || t.mib_per_s.iter().all(|v| *v <= 0.0) {
        return None;
    }
    per_interval(
        t,
        tr("plot.read_written"),
        "series-a",
        Unit::MibRate,
        t.mib_per_s.clone(),
    )
}

/// The hottest keys as horizontal bars, at most five, largest first: `(key, share)` of all
/// throttling when any key drew some (`true`), else of all requests (`false`: a key read
/// over and over).
pub fn hot_key_bars(m: &ProfileModel) -> (bool, Vec<(String, f64)>) {
    let keys = &m.spread.hot_keys;
    let throttle = keys
        .iter()
        .any(|k| k.throttle_share_ratio.is_some_and(|r| r > 0.0));
    let mut v: Vec<(String, f64)> = keys
        .iter()
        .map(|k| {
            let r = if throttle {
                k.throttle_share_ratio.unwrap_or(0.0)
            } else {
                k.share_ratio
            };
            (k.key.clone(), r)
        })
        .filter(|(_, r)| *r > 0.0)
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v.truncate(5);
    (throttle, v)
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

/// The quantiles a tail plot marks: p50, p99 and p99.9 (0: not marked).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Marks {
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
}

impl Marks {
    /// The marks of a distribution, read off it.
    pub fn of(c: &LatencyCdf) -> Self {
        Self {
            p50_ms: c.quantile(0.5).unwrap_or(0.0),
            p99_ms: c.quantile(0.99).unwrap_or(0.0),
            p999_ms: c.quantile(0.999).unwrap_or(0.0),
        }
    }
}

/// The lowest share a tail plot shows: one operation in 10 000.
pub const TAIL_FLOOR: f64 = 1e-4;

/// Latency distributions as tail plots: the share of operations at or slower than each latency,
/// on log axes (latency across, 100 % down to 0.01 % up the side), one line per `(label, cdf)`;
/// `marks` rules a single distribution's p50, p99 and p99.9. The tail, where every finding
/// lives, gets as much room as the body.
pub fn tail(series: &[(String, &LatencyCdf)], marks: Option<Marks>) -> Option<Plot> {
    let tails: Vec<(String, Vec<(f64, f64)>)> =
        series.iter().map(|(l, c)| (l.clone(), c.tail())).collect();
    let all = tails.iter().flat_map(|(_, t)| t.iter());
    let (lo, hi) = all.fold((f64::INFINITY, 0.0_f64), |(lo, hi), p| {
        (lo.min(p.0), hi.max(p.0))
    });
    if hi <= 0.0 {
        return None;
    }
    let mut plot = Plot {
        x: Scale::log(if lo.is_finite() { lo } else { hi / 1e3 }, hi),
        y: Scale {
            min: TAIL_FLOOR,
            max: 1.0,
            log: true,
        },
        step_w: 0.0,
        x_fmt: Unit::Ms,
        y_fmt: Unit::Tail,
        lines: tails
            .into_iter()
            .zip(SERIES.iter().cycle())
            .filter(|((_, t), _)| !t.is_empty())
            .map(|((label, t), tok)| Line::joined(label, tok, t))
            .collect(),
        bands: Vec::new(),
        rules: Vec::new(),
        notes: Vec::new(),
        y2: None,
    };
    if let Some(m) = marks {
        for (v, tok, label) in [
            (m.p50_ms, "axis", "p50"),
            (m.p99_ms, "stage-server", "p99"),
            (m.p999_ms, "stage-stall", "p99.9"),
        ] {
            if v > 0.0 {
                plot.rules.push(Rule::at_x(v, tok, label));
            }
        }
    }
    Some(plot)
}

/// The run's own tail plot with its p50, p99 and p99.9 marked.
pub fn own_tail(m: &ProfileModel) -> Option<Plot> {
    tail(
        &[(tr("plot.ops").to_owned(), &m.cdf)],
        Some(Marks {
            p50_ms: m.p50_ms,
            p99_ms: m.p99_ms,
            p999_ms: m.p999_ms,
        }),
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
        y2: None,
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
        EvidenceChart::Latency => own_tail(m).map(|p| {
            (
                if m.cdf.exact {
                    tr("plot.tail")
                } else {
                    tr("plot.tail_buckets")
                },
                p,
            )
        }),
        EvidenceChart::ErrorsOverTime => {
            errors_rate(&m.timeline).map(|p| (tr("plot.errors_rate"), p))
        }
        // Bars ([`hot_key_bars`]) and numbers, not a plot: the view draws them.
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
        y2: None,
    })
}

/// Prove's sizing sweep: the objective at each value tried, in the order tried, the values
/// that kept the run feasible joined and the others marked apart, the chosen knee ruled with
/// its value. The readout names each trial's value.
pub fn sizing(trials: &[crate::report::view::SizingView], p99: bool) -> Option<Plot> {
    if trials.is_empty() {
        return None;
    }
    let pts = |feasible: bool| -> Vec<(f64, f64)> {
        trials
            .iter()
            .enumerate()
            .filter(|(_, t)| t.feasible == feasible)
            .map(|(i, t)| ((i + 1) as f64, t.objective))
            .collect()
    };
    let top = trials.iter().map(|t| t.objective).fold(0.0, f64::max);
    let n = trials.len() as f64;
    let mut lines = vec![Line::joined(
        tr("plot.sizing_feasible"),
        "series-a",
        pts(true),
    )];
    let infeasible = pts(false);
    if !infeasible.is_empty() {
        lines.push(Line::dots(tr("plot.sizing_infeasible"), "warn", infeasible));
    }
    let mut plot = Plot {
        x: Scale {
            min: 0.5,
            max: n + 0.5,
            log: false,
        },
        y: Scale::zero_based(top),
        step_w: 1.0,
        x_fmt: Unit::Count,
        y_fmt: if p99 { Unit::Ms } else { Unit::Rate },
        lines,
        bands: Vec::new(),
        rules: Vec::new(),
        notes: trials
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let x = (i + 1) as f64;
                (x - 0.5, x + 0.5, trf("plot.sizing_value", &[&t.value]))
            })
            .collect(),
        y2: None,
    };
    if let Some((i, t)) = trials.iter().enumerate().find(|(_, t)| t.chosen) {
        plot.rules
            .push(Rule::at_x((i + 1) as f64, "ok", t.value.clone()));
    }
    Some(plot)
}

fn compact_run_labels(ms: &[&ProfileModel]) -> Vec<String> {
    if ms.len() <= 2 {
        return ms.iter().map(|m| m.id.clone()).collect();
    }
    let shorts: Vec<&str> = ms
        .iter()
        .map(|m| m.id.rsplit('-').next().unwrap_or(&m.id))
        .collect();
    let unique = (0..shorts.len()).all(|i| shorts[i + 1..].iter().all(|s| *s != shorts[i]));
    if unique {
        shorts.into_iter().map(str::to_owned).collect()
    } else {
        ms.iter().map(|m| m.id.clone()).collect()
    }
}

/// Several runs' series over time from their own start, one line each (`f` picks the
/// series; runs without it are left out).
fn overlay(
    ms: &[&ProfileModel],
    y_fmt: Unit,
    log: bool,
    f: impl Fn(&TimelineView) -> &[f64],
) -> Option<Plot> {
    let labels = compact_run_labels(ms);
    let with: Vec<(String, Vec<(f64, f64)>)> = ms
        .iter()
        .zip(labels)
        .map(|(m, label)| {
            let v = f(&m.timeline);
            let pts: Vec<(f64, f64)> = if v.len() == m.timeline.len() {
                series(&m.timeline, v)
                    .into_iter()
                    .filter(|p| !log || p.1 > 0.0)
                    .collect()
            } else {
                Vec::new()
            };
            (label, pts)
        })
        .collect();
    if with.iter().all(|(_, p)| p.is_empty()) {
        return None;
    }
    let w = ms.iter().map(|m| window(&m.timeline)).fold(0.0, f64::max);
    let ys = || with.iter().flat_map(|(_, p)| p.iter().map(|p| p.1));
    let top = ys().fold(0.0, f64::max);
    let y = if log {
        Scale::log(ys().fold(f64::INFINITY, f64::min), top)
    } else {
        Scale::zero_based(top)
    };
    Some(Plot::over_time(
        w,
        y,
        y_fmt,
        with.into_iter()
            .zip(SERIES.iter().cycle())
            .map(|((label, pts), tok)| Line::step(label, tok, pts))
            .collect(),
    ))
}

/// Several runs' goodput over time from their own start, one line each.
pub fn goodput_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    overlay(ms, Unit::Rate, false, |t| &t.goodput_ops_per_s)
}

/// Several runs' p99 over time on a log axis, one line each.
pub fn p99_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    overlay(ms, Unit::Ms, true, |t| &t.p99_ms)
}

/// Several runs' requests per second over time, one line each.
pub fn requests_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    overlay(ms, Unit::Rate, false, |t| &t.requests_per_s)
}

/// Several runs' latency distributions as tail plots, one line each.
pub fn tail_overlay(ms: &[&ProfileModel]) -> Option<Plot> {
    let labels = compact_run_labels(ms);
    let series: Vec<(String, &LatencyCdf)> = labels
        .into_iter()
        .zip(ms.iter())
        .map(|(l, m)| (l, &m.cdf))
        .collect();
    tail(&series, None)
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
            cpu_pct: Vec::new(),
            missed_arrivals_count: vec![0; n],
            verb: None,
        }
    }

    #[test]
    fn problems_have_their_own_axis_and_the_requests_chart_draws_requests_alone() {
        let t = tl(&[
            (100.0, 100.0, 0, 0, 100),
            (100.0, 60.0, 1, 0, 100),
            (100.0, 60.0, 900, 40, 100),
        ]);
        let rq = requests(&t).unwrap();
        assert_eq!(
            rq.lines.len(),
            1,
            "requests alone: problems are flat on its axis"
        );
        let pr = problems(&t).unwrap();
        let by = |l: &str| pr.lines.iter().find(|x| x.label == l).unwrap();
        // One 429 and nine hundred are not the same line.
        let th = by(tr("plot.throttled_responses"));
        assert_eq!(th.mark, crate::ui::chart::Mark::Step);
        assert_eq!(th.token, "warn");
        assert_eq!(th.pts[1], (1.0, 1.0));
        assert_eq!(th.pts[2], (2.0, 900.0));
        let failed = by(tr("plot.failed_ops"));
        assert_eq!(failed.token, "crit");
        assert_eq!(failed.pts[2], (2.0, 40.0));
        assert_eq!(by(tr("plot.retries")).token, "stage-retry");
        // Its own axis: the most problems per second, not the requests.
        assert!(pr.y.max >= 900.0 && pr.y.max < 1100.0, "{:?}", pr.y);
        // A quiet run draws no problems chart, and only what happened otherwise.
        let quiet = tl(&[(10.0, 10.0, 0, 0, 10), (10.0, 10.0, 0, 0, 10)]);
        assert!(problems(&quiet).is_none());
        let mut failing = quiet.clone();
        failing.errors_count = vec![0, 3];
        let p = problems(&failing).unwrap();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.lines[0].label, tr("plot.failed_ops"));
    }

    #[test]
    fn throughput_is_mib_per_second_per_interval() {
        let t = tl(&[(100.0, 100.0, 0, 0, 100), (50.0, 50.0, 0, 0, 50)]);
        let p = throughput(&t).unwrap();
        assert_eq!(p.y_fmt, Unit::MibRate);
        assert_eq!(p.lines[0].pts, vec![(0.0, 10.0), (1.0, 5.0)]);
        let mut none = t.clone();
        none.mib_per_s = vec![0.0, 0.0];
        assert!(throughput(&none).is_none());
    }

    #[test]
    fn hot_keys_are_bars_of_their_share_of_the_throttling() {
        let mut run = crate::report::fixture::run();
        let hk = |key: &str, share: f64, throttle: f64| crate::model::HotKey {
            key: key.into(),
            requests_count: 10,
            share_ratio: share,
            throttle_share_ratio: throttle,
        };
        run.report.dimensions.hot_keys = vec![hk("a", 0.5, 0.1), hk("b", 0.2, 0.8)];
        let m = crate::ui::data::of_run(&run);
        let (throttle, bars) = hot_key_bars(&m);
        assert!(throttle);
        assert_eq!(bars, vec![("b".to_owned(), 0.8), ("a".to_owned(), 0.1)]);
        // Nothing throttled (repeated reads): the share of the requests instead.
        run.report.dimensions.hot_keys = vec![hk("a", 0.5, 0.0), hk("b", 0.2, 0.0)];
        let (throttle, bars) = hot_key_bars(&crate::ui::data::of_run(&run));
        assert!(!throttle);
        assert_eq!(bars[0], ("a".to_owned(), 0.5));
    }

    #[test]
    fn the_inflight_chart_draws_new_connections_and_the_cpu_has_its_own_axis() {
        let mut t = tl(&[(10.0, 10.0, 0, 0, 10), (10.0, 10.0, 0, 0, 10)]);
        assert_eq!(inflight(&t).unwrap().lines.len(), 1, "nothing dialed");
        assert!(cpu(&t).is_none());
        t.new_connections_count = vec![16, 0];
        t.cpu_pct = vec![Some(250.0), None];
        let p = inflight(&t).unwrap();
        assert_eq!(p.lines.len(), 2, "CPU is not drawn on the in-flight axis");
        let conns = &p.lines[1];
        assert_eq!(conns.label, tr("plot.new_connections"));
        assert_eq!(conns.mark, crate::ui::chart::Mark::Step);
        assert_eq!(conns.pts[0], (0.0, 16.0));
        assert!(p.y.max >= 16.0);
        assert!(p.summary("In flight").contains('4'), "{}", p.summary("x"));
        let c = cpu(&t).unwrap();
        assert_eq!(c.lines[0].pts[0], (0.0, 2.5));
        assert!(c.y.max < 16.0, "its own axis: {:?}", c.y);
        // A ceiling ruled on it, the axis grown to show it.
        let r = with_rule(p, 40.0, "warn", "limit 40".into());
        assert_eq!(r.rules[0].y, Some(40.0));
        assert!(r.y.max >= 40.0);
    }

    #[test]
    fn p999_is_dashed_and_the_max_drawn_faint() {
        let t = tl(&[(2000.0, 2000.0, 0, 0, 2000), (2000.0, 2000.0, 0, 0, 2000)]);
        let p = latency(&t).unwrap();
        let by = |l: &str| p.lines.iter().find(|x| x.label == l).unwrap();
        assert!(by("p99.9").dashed && !by("p99").dashed);
        let max = by(tr("plot.max"));
        assert!(max.faint);
        assert_eq!(max.pts[0], (0.0, 120.0));
        assert!(p.y.max >= 120.0);
        // Missed arrivals are a problem per second of their own.
        let mut t = t;
        t.missed_arrivals_count = vec![0, 7];
        let pr = problems(&t).unwrap();
        assert!(
            pr.lines
                .iter()
                .any(|l| l.label == tr("plot.missed") && l.pts[1].1 == 7.0)
        );
        // Goodput read as throughput on the right.
        let g = with_throughput_axis(goodput(&t).unwrap(), 0.5);
        assert!(
            crate::ui::chart::readout_at(&g, 0.5).contains("1000"),
            "{}",
            crate::ui::chart::readout_at(&g, 0.5)
        );
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
        // Requests per second; the retries and the throttled responses on their own axis.
        let rq = requests(&t).unwrap();
        assert_eq!(rq.lines.len(), 1);
        assert!((rq.lines[0].pts[0].1 - 110.0).abs() < 1e-9);
        assert!(rq.summary("Requests").contains("Requests"));
        let pr = problems(&t).unwrap();
        assert_eq!(pr.lines.len(), 2);
        assert_eq!(pr.lines[0].pts[1], (1.0, 4.0));
        assert_eq!(pr.lines[1].label, tr("plot.throttled_responses"));
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
    fn a_distribution_is_a_tail_plot_on_log_axes_with_its_quantiles_ruled() {
        let c = LatencyCdf {
            pts: vec![(1.0, 0.5), (10.0, 0.99), (40.0, 0.999), (260.0, 1.0)],
            exact: true,
        };
        let p = tail(
            &[("a".into(), &c)],
            Some(Marks {
                p50_ms: 1.0,
                p99_ms: 10.0,
                p999_ms: 40.0,
            }),
        )
        .unwrap();
        assert!(p.x.log && p.y.log);
        assert_eq!((p.y.min, p.y.max), (TAIL_FLOOR, 1.0));
        assert_eq!(p.y_fmt, Unit::Tail);
        // Every operation is at least as slow as the fastest; the slowest 0.1 % reach 260 ms.
        let pts = &p.lines[0].pts;
        assert_eq!(pts[0], (1.0, 1.0));
        assert!((pts[1].1 - 0.5).abs() < 1e-9);
        let last = pts.last().unwrap();
        assert_eq!(last.0, 260.0);
        assert!((last.1 - 0.001).abs() < 1e-9);
        assert_eq!(p.lines[0].mark, Mark::Line);
        let labels: Vec<&str> = p.rules.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["p50", "p99", "p99.9"]);
        assert!(tail(&[("a".into(), &LatencyCdf::default())], None).is_none());
        assert_eq!(Marks::of(&c).p999_ms, 40.0);
        // The axis reads in plain percentages.
        assert_eq!(crate::ui::chart::tail_share(1e-4), "0.01 %");
        assert_eq!(crate::ui::chart::tail_share(1.0), "100 %");
        assert_eq!(crate::ui::chart::tail_share(0.0347), "3.47 %");
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

    #[test]
    fn the_sizing_sweep_marks_the_knee_and_the_infeasible_values() {
        use crate::report::view::SizingView;
        let t = |v: &str, o: f64, feasible: bool, chosen: bool| SizingView {
            value: v.into(),
            objective: o,
            throttled_ratio: 0.0,
            feasible,
            chosen,
        };
        let p = sizing(
            &[
                t("8", 100.0, true, false),
                t("16", 180.0, true, true),
                t("32", 90.0, false, false),
            ],
            false,
        )
        .unwrap();
        assert_eq!(p.lines[0].pts, vec![(1.0, 100.0), (2.0, 180.0)]);
        assert_eq!(p.lines[1].pts, vec![(3.0, 90.0)]);
        assert_eq!(p.rules[0].x, Some(2.0));
        assert_eq!(p.rules[0].label, "16");
        assert!(p.note_at(3.0).unwrap().contains("32"));
        assert!(sizing(&[], false).is_none());
    }
}
