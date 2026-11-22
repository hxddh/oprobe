//! Repeat-aware run comparison (`oprobe compare`, `run --baseline`, `verify`): the gate of
//! [`crate::report::diff`] on representative runs, made statistical (design v4 §7).
//!
//! - **One run per side:** a block bootstrap over per-second windows (each second's operations
//!   move together, so autocorrelation within a second does not inflate the confidence),
//!   always reported as `confidence: low`.
//! - **Several runs per side:** run-level variance — a Welch t-interval over the per-run
//!   values (medians, p99s, good-op rates).
//! - **Noise floor:** a change is significant only when its 95 % interval excludes zero *and*
//!   it is larger than the metric's noise floor, learned from repeated identical runs of the
//!   target (`<data>/noise.toml`, [`crate::store::baselines::NoiseFloors`]) or a default.
//! - A latency or throughput check fails only when the change exceeds its limit and is
//!   significant; otherwise it passes with a "not significant" note.
//! - Runs of different targets or workloads get no verdict (`comparable: false`).
//! - Findings are diffed: new, resolved and persisting codes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::capacity::Confidence;
use super::stats::{self, Interval};
use crate::model::{Report, Run};
use crate::report::diff::{DiffReport, GateVerdict, Status, Thresholds, diff, good_ops_per_s};
use crate::store::baselines::{NoiseEntry, noise_ratio};

/// One side of a comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Side {
    /// What the user named (RUNREF, baseline name, `repeat group`).
    pub label: String,
    pub run_ids: Vec<String>,
    /// The run whose report the per-check table compares (the median-p99 run).
    pub representative: String,
}

/// How a [`MetricStat`] interval was computed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// One run per side: block bootstrap over per-second windows.
    BlockBootstrap,
    /// Several runs per side: a t-interval over the per-run values.
    RunsT,
    /// Too few samples: point estimates only, never significant.
    #[default]
    Unavailable,
}

/// A headline metric with confidence intervals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricStat {
    /// `p50_ms`, `p99_ms`, `ops_per_s`.
    pub metric: String,
    /// Unit of `baseline` and `candidate`.
    pub unit: String,
    pub baseline: Interval,
    pub candidate: Interval,
    /// Relative change of the candidate (ratio; +0.1 = 10 % higher), with its 95 % interval.
    pub change: Interval,
    /// Changes smaller than this are noise for this target and workload.
    pub noise_floor_ratio: f64,
    /// The 95 % interval excludes zero and the change exceeds the noise floor.
    pub significant: bool,
    pub method: Method,
    pub confidence: Confidence,
}

/// A finding code on one side only, or on both.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindingRef {
    pub code: String,
    pub severity: super::diagnosis::Severity,
    pub cause: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FindingsDiff {
    pub new: Vec<FindingRef>,
    pub resolved: Vec<FindingRef>,
    pub persisting: Vec<FindingRef>,
}

/// Result of `oprobe compare`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Comparison {
    /// `passed`, `regression` or `not_comparable` (no verdict).
    pub verdict: GateVerdict,
    /// The verdict is `passed`.
    pub passed: bool,
    /// Same target identity and workload hash (design v4 §7).
    pub comparable: bool,
    pub baseline: Side,
    pub candidate: Side,
    pub stats: Vec<MetricStat>,
    pub findings: FindingsDiff,
    /// p99 (ms) of every baseline run then every candidate run, in time order.
    pub p99_trend_ms: Vec<f64>,
    /// The two-sided net verdict (design v5 §4; filled by stream ANA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<super::verdict::NetAssessment>,
    /// The per-check gate (per-operation latency, errors, throttling, amplification, cost),
    /// with significance applied to latency and throughput checks.
    pub gate: DiffReport,
}

fn median_run(runs: &[Run]) -> &Run {
    let mut idx: Vec<usize> = (0..runs.len()).collect();
    idx.sort_by(|a, b| {
        runs[*a]
            .report
            .summary
            .latency
            .p99
            .total_cmp(&runs[*b].report.summary.latency.p99)
    });
    &runs[idx[idx.len() / 2]]
}

/// Most samples per window kept for the latency bootstrap.
const MAX_PER_BLOCK: usize = 400;

/// Width of the bootstrap's time windows (µs): one second, or a tenth of the measurement
/// window for runs shorter than 10 s (at least 100 ms), so short runs still have blocks.
fn block_us(run: &Run) -> i64 {
    let w = run.report.run.window_s.max(0.0);
    ((w / 10.0).clamp(0.1, 1.0) * 1e6) as i64
}

/// Successful-op latencies (ms) of `verb` (all when `None`) by time window of the
/// measurement ([`block_us`]).
fn latency_seconds(run: &Run, verb: Option<&str>) -> Vec<Vec<f64>> {
    let width = block_us(run);
    let w_us = (run.report.run.window_s.max(0.0) * 1e6) as i64;
    let n = ((w_us + width - 1) / width).max(1) as usize;
    let mut b: Vec<Vec<f64>> = vec![Vec::new(); n];
    for o in &run.ops {
        if !o.ok || o.t_us < 0 || verb.is_some_and(|v| o.verb != v) {
            continue;
        }
        let s = (o.t_us / width) as usize;
        if s < n {
            b[s].push(o.latency_us as f64 / 1000.0);
        }
    }
    for v in &mut b {
        if v.len() > MAX_PER_BLOCK {
            let step = v.len() as f64 / MAX_PER_BLOCK as f64;
            *v = (0..MAX_PER_BLOCK)
                .map(|i| v[(i as f64 * step) as usize])
                .collect();
        }
    }
    b.retain(|v| !v.is_empty());
    b
}

/// Successful operations per second in each whole time window ([`block_us`]).
fn ops_seconds(run: &Run) -> Vec<f64> {
    let width = block_us(run);
    let w_us = (run.report.run.window_s.max(0.0) * 1e6) as i64;
    let full = (w_us / width) as usize;
    if full == 0 {
        return Vec::new();
    }
    let mut b = vec![0f64; full];
    for o in &run.ops {
        if o.ok && o.t_us >= 0 {
            let s = (o.t_us / width) as usize;
            if s < full {
                b[s] += 1e6 / width as f64;
            }
        }
    }
    b
}

fn pooled(blocks: &[Vec<f64>], pick: &[usize]) -> Vec<f64> {
    let mut v = Vec::new();
    for &i in pick {
        v.extend_from_slice(&blocks[i]);
    }
    v
}

/// Indices of a moving-block resample of `n` items with blocks of `len`.
fn block_indices(n: usize, len: usize, rng: &mut stats::Rng, out: &mut Vec<usize>) {
    out.clear();
    let len = len.clamp(1, n);
    while out.len() < n {
        let start = rng.index(n);
        for k in 0..len {
            if out.len() == n {
                break;
            }
            out.push((start + k) % n);
        }
    }
}

const ITERS: usize = 1000;

/// `(baseline, candidate, change)` for one run per side: a block bootstrap of `stat` over
/// per-second blocks of samples.
fn block_bootstrap(
    a: &[Vec<f64>],
    b: &[Vec<f64>],
    block_len: usize,
    stat: &dyn Fn(&mut [f64]) -> f64,
    seed: u64,
) -> (Interval, Interval, Interval) {
    let full = |x: &[Vec<f64>]| {
        let mut v: Vec<f64> = x.iter().flatten().copied().collect();
        stat(&mut v)
    };
    let (ea, eb) = (full(a), full(b));
    let rel = |x: f64, y: f64| if x.abs() > 0.0 { y / x - 1.0 } else { 0.0 };
    let mut rng = stats::Rng::new(seed);
    let (mut ia, mut ib) = (Vec::new(), Vec::new());
    let (mut sa, mut sb, mut sc) = (
        Vec::with_capacity(ITERS),
        Vec::with_capacity(ITERS),
        Vec::with_capacity(ITERS),
    );
    for _ in 0..ITERS {
        block_indices(a.len(), block_len, &mut rng, &mut ia);
        block_indices(b.len(), block_len, &mut rng, &mut ib);
        let x = stat(&mut pooled(a, &ia));
        let y = stat(&mut pooled(b, &ib));
        sa.push(x);
        sb.push(y);
        sc.push(rel(x, y));
    }
    let iv = |e: f64, s: &mut Vec<f64>| Interval {
        estimate: e,
        low: stats::quantile_mut(s, 0.025),
        high: stats::quantile_mut(s, 0.975),
    };
    (iv(ea, &mut sa), iv(eb, &mut sb), iv(rel(ea, eb), &mut sc))
}

/// Two-sided 97.5 % quantile of Student's t with `df` degrees of freedom.
pub fn t975(df: f64) -> f64 {
    const T: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    if !df.is_finite() || df < 1.0 {
        return T[0];
    }
    let i = df.floor() as usize;
    if i > 30 { 1.96 } else { T[i - 1] }
}

fn var(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 2 {
        return 0.0;
    }
    let m = stats::mean(v);
    v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1) as f64
}

/// `(baseline, candidate, change)` over per-run values: means with t-intervals, and the
/// relative change with a Welch interval. A side with one run borrows the other side's
/// variance.
pub fn runs_t(a: &[f64], b: &[f64]) -> (Interval, Interval, Interval) {
    let (ma, mb) = (stats::mean(a), stats::mean(b));
    let (mut va, mut vb) = (var(a), var(b));
    if a.len() < 2 {
        va = vb;
    }
    if b.len() < 2 {
        vb = va;
    }
    let (na, nb) = (a.len() as f64, b.len() as f64);
    let side = |m: f64, v: f64, n: f64| {
        let h = t975(n - 1.0) * (v / n).sqrt();
        Interval {
            estimate: m,
            low: m - h,
            high: m + h,
        }
    };
    let se2 = va / na + vb / nb;
    let df = if se2 > 0.0 {
        let num = se2 * se2;
        let den = (va / na).powi(2) / (na - 1.0).max(1.0) + (vb / nb).powi(2) / (nb - 1.0).max(1.0);
        if den > 0.0 { num / den } else { na + nb - 2.0 }
    } else {
        na + nb - 2.0
    };
    let h = t975(df) * se2.sqrt();
    let change = if ma.abs() > 0.0 {
        Interval {
            estimate: (mb - ma) / ma,
            low: (mb - ma - h) / ma,
            high: (mb - ma + h) / ma,
        }
    } else {
        Interval::default()
    };
    (side(ma, va, na), side(mb, vb, nb), change)
}

fn verb_q(r: &Report, verb: &str, p99: bool) -> Option<f64> {
    r.verbs
        .iter()
        .find(|v| v.verb == verb)
        .map(|v| if p99 { v.latency.p99 } else { v.latency.p50 })
}

type Change = (Interval, Interval, Interval, Method, Confidence);

fn runs_confidence(na: usize, nb: usize) -> Confidence {
    match na.min(nb) {
        n if n >= 5 => Confidence::High,
        n if n >= 3 => Confidence::Medium,
        _ => Confidence::Low,
    }
}

/// Change of a latency quantile `q` of `verb` (all operations when `None`). With three or
/// more runs on each side, a Welch interval on the log ratio of the per-run values (never
/// below −100 %); with fewer, a block bootstrap over the time windows of every run of a side
/// pooled (a t-interval on two runs is meaningless: A12).
fn latency_change(base: &[Run], cand: &[Run], verb: Option<&str>, q: f64) -> Option<Change> {
    let seed = (q * 1000.0) as u64 ^ verb.map_or(0, |v| crate::model::ids::fnv1a(v.as_bytes()));
    if base.len() >= 3 && cand.len() >= 3 {
        let get = |r: &Run| match verb {
            Some(v) => verb_q(&r.report, v, q > 0.9),
            None => Some(if q > 0.9 {
                r.report.summary.latency.p99
            } else {
                r.report.summary.latency.p50
            }),
        };
        let a: Option<Vec<f64>> = base.iter().map(get).collect();
        let b: Option<Vec<f64>> = cand.iter().map(get).collect();
        let (a, b) = (a?, b?);
        let (x, y, _) = runs_t(&a, &b);
        let c = super::verdict::log_ratio(&a, &b, false);
        return Some((x, y, c, Method::RunsT, runs_confidence(a.len(), b.len())));
    }
    let blocks = |runs: &[Run]| -> Vec<Vec<f64>> {
        runs.iter().flat_map(|r| latency_seconds(r, verb)).collect()
    };
    let (a, b) = (blocks(base), blocks(cand));
    let count = |x: &[Vec<f64>]| x.iter().map(Vec::len).sum::<usize>();
    if a.len() < 3 || b.len() < 3 || count(&a) < 10 || count(&b) < 10 {
        return None;
    }
    let (x, y, c) = block_bootstrap(&a, &b, 1, &stats::p(q), seed);
    Some((x, y, c, Method::BlockBootstrap, Confidence::Low))
}

/// Change of the good-op rate (as [`latency_change`]).
fn throughput_change(base: &[Run], cand: &[Run]) -> Option<Change> {
    if base.len() >= 3 && cand.len() >= 3 {
        let a: Vec<f64> = base.iter().map(|r| good_ops_per_s(&r.report)).collect();
        let b: Vec<f64> = cand.iter().map(|r| good_ops_per_s(&r.report)).collect();
        let (x, y, _) = runs_t(&a, &b);
        let c = super::verdict::log_ratio(&a, &b, false);
        return Some((x, y, c, Method::RunsT, runs_confidence(a.len(), b.len())));
    }
    let secs = |runs: &[Run]| -> Vec<f64> { runs.iter().flat_map(ops_seconds).collect() };
    let (a, b) = (secs(base), secs(cand));
    if a.len() < 3 || b.len() < 3 {
        return None;
    }
    let block = ((a.len().min(b.len()) as f64).cbrt().ceil() as usize).max(1);
    let wrap = |v: &[f64]| v.iter().map(|x| vec![*x]).collect::<Vec<_>>();
    let mean = |v: &mut [f64]| stats::mean(v);
    let (x, y, c) = block_bootstrap(&wrap(&a), &wrap(&b), block, &mean, 11);
    Some((x, y, c, Method::BlockBootstrap, Confidence::Low))
}

/// The change is significant: its interval excludes zero and it exceeds the noise floor.
pub fn significant(change: &Interval, floor: f64) -> bool {
    change.excludes_zero() && change.estimate.abs() > floor
}

fn majority_codes(runs: &[Run]) -> BTreeMap<String, FindingRef> {
    let mut n: BTreeMap<String, (usize, FindingRef)> = BTreeMap::new();
    for r in runs {
        for f in &r.report.findings {
            let e = n.entry(f.code.clone()).or_insert_with(|| {
                (
                    0,
                    FindingRef {
                        code: f.code.clone(),
                        severity: f.severity,
                        cause: f.cause.clone(),
                    },
                )
            });
            e.0 += 1;
        }
    }
    n.into_iter()
        .filter(|(_, (k, _))| *k * 2 > runs.len())
        .map(|(c, (_, f))| (c, f))
        .collect()
}

fn findings_diff(base: &[Run], cand: &[Run]) -> FindingsDiff {
    let (b, c) = (majority_codes(base), majority_codes(cand));
    FindingsDiff {
        new: c
            .iter()
            .filter(|(k, _)| !b.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
        resolved: b
            .iter()
            .filter(|(k, _)| !c.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
        persisting: c
            .iter()
            .filter(|(k, _)| b.contains_key(*k))
            .map(|(_, f)| f.clone())
            .collect(),
    }
}

fn fmt_ci(i: &Interval) -> String {
    format!(
        "{:+.1}% [{:+.1}%, {:+.1}%]",
        i.estimate * 100.0,
        i.low * 100.0,
        i.high * 100.0
    )
}

fn method_label(m: Method, c: Confidence) -> String {
    let conf = match c {
        Confidence::Low => "low confidence",
        Confidence::Medium => "medium confidence",
        Confidence::High => "high confidence",
    };
    match m {
        Method::BlockBootstrap => format!("per-second block bootstrap, {conf}"),
        Method::RunsT => format!("t-interval over runs, {conf}"),
        Method::Unavailable => "too few samples".into(),
    }
}

/// Compare `cand` runs against `base` runs (each non-empty) with the default noise floors.
pub fn compare(
    base: &[Run],
    cand: &[Run],
    base_label: &str,
    cand_label: &str,
    t: &Thresholds,
) -> Comparison {
    compare_with_noise(base, cand, base_label, cand_label, t, None)
}

/// [`compare`] with the noise floors learned for the target and workload.
pub fn compare_with_noise(
    base: &[Run],
    cand: &[Run],
    base_label: &str,
    cand_label: &str,
    t: &Thresholds,
    noise: Option<&NoiseEntry>,
) -> Comparison {
    assert!(
        !base.is_empty() && !cand.is_empty(),
        "compare needs runs on both sides"
    );
    let (rb, rc) = (median_run(base), median_run(cand));
    let mut gate = diff(&rb.report, &rc.report, t);
    // Every run of a side must be comparable with the other side's representative.
    if gate.comparable {
        for r in base.iter().chain(cand.iter()) {
            let other = if base.iter().any(|b| b.id() == r.id()) {
                &rc.report
            } else {
                &rb.report
            };
            let id = crate::report::diff::identity(&r.report, other, &[], t);
            if !id.comparable {
                gate.comparable = false;
                gate.identity.comparable = false;
                gate.identity.reasons.push(format!(
                    "run {} of the comparison: {}",
                    r.id(),
                    id.reasons.join("; ")
                ));
            }
        }
    }
    let repeated = base.len() > 1 || cand.len() > 1;
    // Significance for latency and throughput checks.
    for chk in gate.checks.iter_mut() {
        let (iv, metric) = if let Some(verb) = chk.id.strip_prefix("latency_p99.") {
            (latency_change(base, cand, Some(verb), 0.99), "p99_ms")
        } else if let Some(verb) = chk.id.strip_prefix("latency_p50.") {
            (latency_change(base, cand, Some(verb), 0.50), "p50_ms")
        } else if chk.id == "throughput" {
            (throughput_change(base, cand), "ops_per_s")
        } else {
            continue;
        };
        let Some((b, c, change, method, conf)) = iv else {
            continue;
        };
        if chk.verdict == Status::Skipped {
            continue;
        }
        if repeated {
            chk.baseline = b.estimate;
            chk.candidate = c.estimate;
            chk.delta_ratio = Some(change.estimate);
        }
        let floor = noise_ratio(noise, metric);
        let ci = fmt_ci(&change);
        let sig = significant(&change, floor);
        let how = method_label(method, conf);
        // Three tiers (design v5 §4, A13): worse and significant fails over the limit and
        // warns under it; not significant passes only when the interval rules out a change
        // past the limit, else it is inconclusive (never "same").
        let higher_worse = metric != "ops_per_s";
        let worse_end = if higher_worse {
            change.high
        } else {
            -change.low
        };
        let worse = if higher_worse {
            change.estimate > 0.0
        } else {
            change.estimate < 0.0
        };
        let limit = chk.limit_ratio.unwrap_or(f64::INFINITY);
        // Latency changes below the absolute floor are never regressions.
        let abs_ok =
            metric == "ops_per_s" || (chk.candidate - chk.baseline).abs() > t.min_latency_delta_ms;
        chk.verdict = if sig && worse && abs_ok && chk.verdict == Status::Fail {
            Status::Fail
        } else if sig && worse && abs_ok {
            Status::Warn
        } else if sig && !worse {
            Status::Improved
        } else if worse_end > limit && abs_ok {
            Status::Inconclusive
        } else {
            Status::Pass
        };
        chk.note = match chk.verdict {
            Status::Fail => format!("95% CI {ci} ({how})"),
            Status::Warn => format!(
                "significantly worse but within the limit: 95% CI {ci}, noise floor ±{:.1}% ({how})",
                floor * 100.0
            ),
            Status::Improved => format!("95% CI {ci} ({how})"),
            Status::Inconclusive => format!(
                "not significant, but a change past the limit is not ruled out: 95% CI {ci}, noise floor ±{:.1}% ({how}); repeat the runs",
                floor * 100.0
            ),
            _ => format!(
                "not significant: 95% CI of the change {ci}, noise floor ±{:.1}% ({how})",
                floor * 100.0
            ),
        };
    }
    gate.settle();
    let mut stats_out = Vec::new();
    for (metric, unit, iv) in [
        ("p50_ms", "ms", latency_change(base, cand, None, 0.50)),
        ("p99_ms", "ms", latency_change(base, cand, None, 0.99)),
        ("ops_per_s", "ops_per_s", throughput_change(base, cand)),
    ] {
        let floor = noise_ratio(noise, metric);
        match iv {
            Some((b, c, change, method, confidence)) => stats_out.push(MetricStat {
                metric: metric.into(),
                unit: unit.into(),
                baseline: b,
                candidate: c,
                // Latency changes below the absolute floor (`min_latency_delta_ms`) are
                // never significant, however tight the interval.
                significant: significant(&change, floor)
                    && (unit != "ms" || (c.estimate - b.estimate).abs() > t.min_latency_delta_ms),
                change,
                noise_floor_ratio: floor,
                method,
                confidence,
            }),
            None => {
                let v = |r: &Report| match metric {
                    "p50_ms" => r.summary.latency.p50,
                    "p99_ms" => r.summary.latency.p99,
                    _ => good_ops_per_s(r),
                };
                let (x, y) = (v(&rb.report), v(&rc.report));
                stats_out.push(MetricStat {
                    metric: metric.into(),
                    unit: unit.into(),
                    baseline: Interval::point(x),
                    candidate: Interval::point(y),
                    change: Interval::point(if x.abs() > 0.0 { y / x - 1.0 } else { 0.0 }),
                    noise_floor_ratio: floor,
                    significant: false,
                    method: Method::Unavailable,
                    confidence: Confidence::Low,
                });
            }
        }
    }
    let mut trend: Vec<(&str, f64)> = base
        .iter()
        .chain(cand.iter())
        .map(|r| {
            (
                r.report.run.started_at.as_str(),
                r.report.summary.latency.p99,
            )
        })
        .collect();
    trend.sort_by(|a, b| a.0.cmp(b.0));
    let side = |label: &str, runs: &[Run], rep: &Run| Side {
        label: label.to_owned(),
        run_ids: runs.iter().map(|r| r.id().to_owned()).collect(),
        representative: rep.id().to_owned(),
    };
    let net = net_of(&stats_out, base, cand);
    Comparison {
        verdict: gate.verdict,
        passed: gate.passed,
        comparable: gate.comparable,
        baseline: side(base_label, base, rb),
        candidate: side(cand_label, cand, rc),
        stats: stats_out,
        findings: findings_diff(base, cand),
        p99_trend_ms: trend.into_iter().map(|x| x.1).collect(),
        net: Some(net),
        gate,
    }
}

/// The net verdict of a comparison: its headline statistics plus throttling and errors over
/// pooled counts (the same rule as verify, [`super::verdict::assess`]).
fn net_of(stats_out: &[MetricStat], base: &[Run], cand: &[Run]) -> super::verdict::NetAssessment {
    use super::verdict::{MetricChange, ratio_difference};
    let mut changes: Vec<MetricChange> = stats_out
        .iter()
        .filter(|s| s.method != Method::Unavailable)
        .map(|s| MetricChange {
            metric: s.metric.clone(),
            change: s.change,
            lower_is_better: s.unit == "ms",
            floor: s.noise_floor_ratio,
        })
        .collect();
    let pooled = |rs: &[Run], f: &dyn Fn(&Report) -> (u64, u64)| {
        rs.iter().fold((0.0, 0.0), |(x, y), r| {
            let (a, b) = f(&r.report);
            (x + a as f64, y + b as f64)
        })
    };
    let thr = |r: &Report| (r.resilience.throttled_responses, r.connections.requests);
    let err = |r: &Report| (r.summary.failed_ops, r.summary.ops);
    for (metric, f) in [
        (
            "throttled_request_ratio",
            &thr as &dyn Fn(&Report) -> (u64, u64),
        ),
        ("error_ratio", &err),
    ] {
        let ((ka, na), (kb, nb)) = (pooled(base, f), pooled(cand, f));
        if na > 0.0 && nb > 0.0 && (ka > 0.0 || kb > 0.0) {
            changes.push(MetricChange {
                metric: metric.into(),
                change: ratio_difference(ka, na, kb, nb),
                lower_is_better: true,
                floor: 0.001,
            });
        }
    }
    super::verdict::assess(&changes)
}

impl Comparison {
    pub fn stat(&self, metric: &str) -> Option<&MetricStat> {
        self.stats.iter().find(|s| s.metric == metric)
    }

    /// The exit code (3 on a regression, 2 for non-comparable runs under `--strict`).
    pub fn exit_code(&self) -> u8 {
        self.gate.exit_code()
    }

    fn stat_rows(&self) -> Vec<Vec<String>> {
        self.stats
            .iter()
            .map(|s| {
                let v = |i: &Interval| {
                    if (i.high - i.low).abs() < 1e-9 {
                        format!("{:.2}", i.estimate)
                    } else {
                        format!("{:.2} [{:.2}–{:.2}]", i.estimate, i.low, i.high)
                    }
                };
                vec![
                    s.metric.clone(),
                    v(&s.baseline),
                    v(&s.candidate),
                    fmt_ci(&s.change),
                    format!("±{:.1}%", s.noise_floor_ratio * 100.0),
                    if s.significant {
                        "yes".into()
                    } else {
                        "no".into()
                    },
                    method_label(s.method, s.confidence),
                ]
            })
            .collect()
    }

    pub fn render_terminal(&self) -> String {
        use crate::report::style::*;
        let mut o = self.gate.render_terminal();
        let _ = writeln!(
            o,
            "\n{} baseline {} run(s) · candidate {} run(s) · p99 trend {}",
            bold(&cyan("STATISTICS")),
            self.baseline.run_ids.len(),
            self.candidate.run_ids.len(),
            stats::sparkline(&self.p99_trend_ms, 24)
        );
        o += &table(
            &[
                "metric",
                "baseline",
                "candidate",
                "change (95% CI)",
                "noise",
                "significant",
                "method",
            ],
            &self.stat_rows(),
            2,
        );
        let f = &self.findings;
        for (label, list) in [("new", &f.new), ("resolved", &f.resolved)] {
            for x in list {
                let _ = writeln!(
                    o,
                    "  {} {} — {}",
                    if label == "new" {
                        red("new finding")
                    } else {
                        green("resolved")
                    },
                    x.code,
                    x.cause
                );
            }
        }
        if !self.comparable {
            let _ = writeln!(
                o,
                "\n  {} {}",
                yellow("not comparable:"),
                self.gate.identity.reasons.join("; ")
            );
        }
        o
    }

    /// Markdown for a sticky PR comment (starts with a stable HTML marker to find and update).
    pub fn render_markdown(&self) -> String {
        let mut md = String::from("<!-- oprobe-compare -->\n");
        md += &self.gate.render_markdown();
        let _ = writeln!(
            md,
            "\n**Statistics** — baseline {} run(s), candidate {} run(s) · p99 trend `{}`\n",
            self.baseline.run_ids.len(),
            self.candidate.run_ids.len(),
            stats::sparkline(&self.p99_trend_ms, 24)
        );
        let _ = writeln!(
            md,
            "| metric | baseline | candidate | change (95% CI) | noise | significant | method |\n|---|---:|---:|---:|---:|---|---|"
        );
        for r in self.stat_rows() {
            let _ = writeln!(md, "| {} |", r.join(" | "));
        }
        let f = &self.findings;
        if !f.new.is_empty() || !f.resolved.is_empty() {
            md += "\n**Findings**\n\n";
            for x in &f.new {
                let _ = writeln!(md, "- 🆕 `{}` ({}) {}", x.code, x.severity.label(), x.cause);
            }
            for x in &f.resolved {
                let _ = writeln!(md, "- ✅ resolved `{}` {}", x.code, x.cause);
            }
        }
        md
    }

    pub fn render_junit(&self) -> String {
        let mut x = self.gate.render_junit();
        // One test case per new finding of warning severity or worse (informational ones pass).
        let extra: String = self
            .findings
            .new
            .iter()
            .map(|f| {
                let esc = f.cause.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
                if f.severity >= super::diagnosis::Severity::Warning && self.comparable {
                    format!("  <testcase classname=\"oprobe.findings\" name=\"new finding {}\"><failure message=\"{esc}\"/></testcase>\n", f.code)
                } else {
                    format!("  <testcase classname=\"oprobe.findings\" name=\"new finding {}\"/>\n", f.code)
                }
            })
            .collect();
        if let Some(i) = x.rfind("</testsuite>") {
            x.insert_str(i, &extra);
        }
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;
    use crate::trace::{Quantiles, VerbStats};

    fn run(id: &str, scale: f64, ops_per_s: f64) -> Run {
        let mut r = straggler_run();
        r.report.run.id = id.into();
        r.report.run.started_at = format!("2026-11-20T00:00:{id}Z");
        r.report.run.window_s = 5.0;
        let n = r.ops.len() as i64;
        for (i, o) in r.ops.iter_mut().enumerate() {
            o.latency_us = (o.latency_us as f64 * scale) as u64;
            o.verb = "read.range".into();
            o.ok = true;
            o.t_us = i as i64 * 5_000_000 / n.max(1);
        }
        let mut lat: Vec<f64> = r.ops.iter().map(|o| o.latency_us as f64 / 1000.0).collect();
        let q = Quantiles {
            count: 1000,
            p50: stats::quantile_mut(&mut lat, 0.5),
            p99: stats::quantile_mut(&mut lat, 0.99),
            ..Default::default()
        };
        r.report.summary.latency = q;
        r.report.summary.ops_per_s = ops_per_s;
        r.report.summary.ops = (ops_per_s * 5.0) as u64;
        r.report.summary.failed_ops = 0;
        r.report.verbs = vec![VerbStats {
            verb: "read.range".into(),
            count: 1000,
            latency: q,
            ops_per_s,
            ..Default::default()
        }];
        r
    }

    #[test]
    fn a_real_regression_fails_and_noise_passes() {
        let t = Thresholds::default();
        let base = vec![run("01", 1.0, 200.0)];
        let worse = vec![run("02", 1.6, 200.0)];
        let c = compare(&base, &worse, "a", "b", &t);
        assert!(!c.passed, "{:#?}", c.gate.checks);
        assert_eq!(c.verdict, GateVerdict::Regression);
        let p50 = c.stat("p50_ms").unwrap();
        assert!(p50.significant && p50.change.estimate > 0.5, "{p50:?}");
        assert_eq!(p50.method, Method::BlockBootstrap);
        assert_eq!(p50.confidence, Confidence::Low);
        // Repeats: per-run values; a 25 % p99 change in one noisy run of five is not significant.
        let bs: Vec<Run> = (0..5)
            .map(|i| run(&format!("1{i}"), 1.0 + i as f64 * 0.05, 200.0))
            .collect();
        let cs: Vec<Run> = (0..5)
            .map(|i| {
                run(
                    &format!("2{i}"),
                    if i == 0 { 1.25 } else { 1.0 + i as f64 * 0.05 },
                    200.0,
                )
            })
            .collect();
        let c = compare(&bs, &cs, "base", "cand", &t);
        assert!(c.passed, "{:#?}", c.gate.checks);
        assert_eq!(c.stats[0].method, Method::RunsT);
        assert_eq!(c.stats[0].confidence, Confidence::High);
        assert_eq!(c.p99_trend_ms.len(), 10);
        let md = c.render_markdown();
        assert!(md.starts_with("<!-- oprobe-compare -->"));
        assert!(md.contains("p99 trend"));
        assert!(c.render_junit().contains("</testsuite>"));
    }

    #[test]
    fn identical_runs_are_not_significant() {
        let a = run("01", 1.0, 200.0);
        let mut b = run("02", 1.0, 200.0);
        // A 1 % shift (the AN-10 case: +0.9 % with a tight interval) is below the noise floor.
        for o in &mut b.ops {
            o.latency_us = (o.latency_us as f64 * 1.01) as u64;
        }
        let c = compare(&[a], &[b], "a", "b", &Thresholds::default());
        for s in &c.stats {
            assert!(!s.significant, "{s:?}");
        }
        assert!(c.passed);
    }

    #[test]
    fn t_interval_over_runs() {
        let (a, b, c) = runs_t(&[100.0, 102.0, 98.0], &[110.0, 112.0, 108.0]);
        assert!((a.estimate - 100.0).abs() < 1e-9 && (b.estimate - 110.0).abs() < 1e-9);
        assert!(c.low > 0.0 && (c.estimate - 0.1).abs() < 1e-9, "{c:?}");
        let (_, _, c) = runs_t(&[100.0, 120.0, 80.0], &[101.0, 121.0, 81.0]);
        assert!(!c.excludes_zero(), "{c:?}");
        assert!((t975(1.0) - 12.706).abs() < 1e-9);
        assert_eq!(t975(100.0), 1.96);
        assert!(significant(
            &Interval {
                estimate: 0.2,
                low: 0.1,
                high: 0.3
            },
            0.05
        ));
        assert!(!significant(
            &Interval {
                estimate: 0.02,
                low: 0.01,
                high: 0.03
            },
            0.05
        ));
    }

    #[test]
    fn findings_are_diffed() {
        let mut a = run("01", 1.0, 200.0);
        let mut b = run("02", 1.0, 200.0);
        a.report.findings = vec![crate::model::Finding {
            code: "throttling".into(),
            ..Default::default()
        }];
        b.report.findings = vec![crate::model::Finding {
            code: "cold_connections".into(),
            severity: crate::analysis::diagnosis::Severity::Warning,
            ..Default::default()
        }];
        let c = compare(&[a], &[b], "a", "b", &Thresholds::default());
        assert_eq!(c.findings.new[0].code, "cold_connections");
        assert_eq!(c.findings.resolved[0].code, "throttling");
        assert!(c.render_junit().contains("new finding cold_connections"));
    }
}
