//! Time-segment analysis of a run: per-second series from the operation rows, stable
//! throughput segments (change-point detection on ops/s), and the first seconds compared
//! with the steady state — a slow start (cold connections, DNS, TLS, auto-scaling) skews a
//! short run's averages.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::stats::{mean, quantile_us_ms, stddev};
use crate::model::{OpRow, TimeWindow};

/// One second of the measurement window (from the operation rows).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SecondStats {
    pub t_s: f64,
    pub ops_count: u64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub errors_count: u64,
    pub throttled_count: u64,
}

/// A stretch of the run with stable throughput.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Segment {
    pub from_s: f64,
    pub to_s: f64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub errors_count: u64,
    pub throttled_count: u64,
}

impl Segment {
    pub fn window(&self) -> TimeWindow {
        TimeWindow {
            from_s: self.from_s,
            to_s: self.to_s,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Segments {
    pub per_second: Vec<SecondStats>,
    /// Stable-throughput segments in time order.
    pub segments: Vec<Segment>,
    /// The longest segment: what the run measures once settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steady: Option<Segment>,
    /// The first seconds of the window (start-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Segment>,
}

fn summarize(ops: &[&OpRow], from_s: f64, to_s: f64) -> Segment {
    let mut lat: Vec<u64> = ops.iter().map(|o| o.latency_us).collect();
    let secs = (to_s - from_s).max(1e-9);
    Segment {
        from_s,
        to_s,
        ops_per_s: ops.len() as f64 / secs,
        mib_per_s: ops.iter().map(|o| o.bytes).sum::<u64>() as f64 / 1_048_576.0 / secs,
        p50_ms: quantile_us_ms(&mut lat, 0.5),
        p99_ms: quantile_us_ms(&mut lat, 0.99),
        errors_count: ops.iter().filter(|o| !o.ok).count() as u64,
        throttled_count: ops.iter().filter(|o| o.throttled).count() as u64,
    }
}

fn ops_in(ops: &[OpRow], from_s: f64, to_s: f64) -> Vec<&OpRow> {
    let (a, b) = ((from_s * 1e6) as i64, (to_s * 1e6) as i64);
    ops.iter().filter(|o| o.t_us >= a && o.t_us < b).collect()
}

/// Binary segmentation on a per-second series: split where the mean shifts most (t-like
/// statistic above `min_t`, both sides at least `min_len` seconds), recursively.
fn change_points(x: &[f64], offset: usize, min_len: usize, min_t: f64, depth: u32) -> Vec<usize> {
    if depth == 0 || x.len() < 2 * min_len {
        return Vec::new();
    }
    let sd = stddev(x).max(mean(x).abs() * 0.02).max(1e-9);
    let mut best: Option<(usize, f64)> = None;
    for k in min_len..=x.len() - min_len {
        let (l, r) = x.split_at(k);
        let t =
            (mean(l) - mean(r)).abs() / sd * ((l.len() * r.len()) as f64 / x.len() as f64).sqrt();
        if best.is_none_or(|(_, bt)| t > bt) {
            best = Some((k, t));
        }
    }
    let Some((k, t)) = best else {
        return Vec::new();
    };
    if t < min_t {
        return Vec::new();
    }
    let mut out = change_points(&x[..k], offset, min_len, min_t, depth - 1);
    out.push(offset + k);
    out.extend(change_points(
        &x[k..],
        offset + k,
        min_len,
        min_t,
        depth - 1,
    ));
    out
}

/// Analyse `ops` over a window of `window_s` seconds.
pub fn segments(ops: &[OpRow], window_s: f64) -> Segments {
    let n = window_s.floor().max(0.0) as usize;
    if n == 0 || ops.is_empty() {
        return Segments::default();
    }
    let mut per: Vec<Vec<&OpRow>> = vec![Vec::new(); n];
    for o in ops.iter().filter(|o| o.t_us >= 0) {
        let i = (o.t_us / 1_000_000) as usize;
        if i < n {
            per[i].push(o);
        }
    }
    let per_second: Vec<SecondStats> = per
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let s = summarize(v, i as f64, i as f64 + 1.0);
            SecondStats {
                t_s: i as f64,
                ops_count: v.len() as u64,
                mib_per_s: s.mib_per_s,
                p50_ms: s.p50_ms,
                p99_ms: s.p99_ms,
                errors_count: s.errors_count,
                throttled_count: s.throttled_count,
            }
        })
        .collect();
    let series: Vec<f64> = per_second.iter().map(|s| s.ops_count as f64).collect();
    let mut cuts = vec![0];
    cuts.extend(change_points(&series, 0, 3, 4.0, 3));
    cuts.push(n);
    let segs: Vec<Segment> = cuts
        .windows(2)
        .map(|w| {
            let (a, b) = (w[0] as f64, w[1] as f64);
            summarize(&ops_in(ops, a, b), a, b)
        })
        .collect();
    let steady = segs
        .iter()
        .max_by(|a, b| (a.to_s - a.from_s).total_cmp(&(b.to_s - b.from_s)))
        .cloned();
    let start = (n >= 6).then(|| {
        let first = (n as f64 / 5.0).clamp(1.0, 3.0).floor();
        summarize(&ops_in(ops, 0.0, first), 0.0, first)
    });
    Segments {
        per_second,
        segments: segs,
        steady,
        start,
    }
}

/// A latency heatmap with buckets fitted to the data: 1-1.5-2-3-5-7 steps per decade from the
/// fastest to the slowest operation (finer than the report's fixed buckets). Operations of
/// the warmup (`t_us < 0`) are left out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FineHeatmap {
    pub t_buckets_s: Vec<f64>,
    pub bucket_width_s: f64,
    /// Lower bound of each latency bucket (ms); the last is open-ended.
    pub latency_buckets_ms: Vec<f64>,
    /// `counts[t][l]`.
    pub counts: Vec<Vec<u64>>,
}

pub fn fine_heatmap(ops: &[OpRow], window_s: f64, max_columns: usize) -> FineHeatmap {
    let measured = || ops.iter().filter(|o| o.t_us >= 0);
    if measured().next().is_none() {
        return FineHeatmap::default();
    }
    let min_ms = measured().map(|o| o.latency_us).min().unwrap_or(0) as f64 / 1000.0;
    let max_ms = measured().map(|o| o.latency_us).max().unwrap_or(0) as f64 / 1000.0;
    // The first row starts at the largest step at or below the fastest operation (rows below
    // it would be empty), then every step up to the slowest.
    let mut bounds = vec![0.0];
    let mut decade = 10f64.powf(min_ms.max(0.001).log10().floor());
    let mut first = decade;
    'outer: loop {
        for m in [1.0, 1.5, 2.0, 3.0, 5.0, 7.0] {
            let b = m * decade;
            if b > max_ms {
                break 'outer;
            }
            if b <= min_ms * 1.000_001 {
                first = b;
            } else {
                if bounds.len() == 1 {
                    bounds.push(first);
                }
                bounds.push(b);
            }
        }
        decade *= 10.0;
    }
    if bounds.len() == 1 {
        bounds.push(first);
    }
    let window = window_s.max(1e-3);
    let width = (window / max_columns.max(1) as f64).max(0.1);
    let n_t = ((window / width).ceil() as usize).max(1);
    let mut counts = vec![vec![0u64; bounds.len()]; n_t];
    for o in measured() {
        let t = ((o.t_us as f64 / 1e6) / width) as usize;
        let ms = o.latency_us as f64 / 1000.0;
        let l = bounds.partition_point(|b| *b <= ms).saturating_sub(1);
        counts[t.min(n_t - 1)][l] += 1;
    }
    FineHeatmap {
        t_buckets_s: (0..n_t).map(|i| i as f64 * width).collect(),
        bucket_width_s: width,
        latency_buckets_ms: bounds,
        counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::op;

    #[test]
    fn a_slow_start_is_its_own_segment() {
        let mut ops = Vec::new();
        let mut id = 0;
        for s in 0..20i64 {
            // 20 ops/s for 4 s, then 100 ops/s.
            let n = if s < 4 { 20 } else { 100 };
            for j in 0..n {
                id += 1;
                let lat = if s < 4 { 50_000 } else { 10_000 };
                ops.push(op(id, s * 1_000_000 + j * (1_000_000 / n), lat));
            }
        }
        let a = segments(&ops, 20.0);
        assert_eq!(a.per_second.len(), 20);
        assert_eq!(a.segments.len(), 2, "{:#?}", a.segments);
        assert_eq!(a.segments[0].to_s, 4.0);
        let steady = a.steady.unwrap();
        assert_eq!(steady.from_s, 4.0);
        assert!((steady.ops_per_s - 100.0).abs() < 1.0);
        let start = a.start.unwrap();
        assert!(start.p50_ms > 4.0 * steady.p50_ms);
        let h = fine_heatmap(&ops, 20.0, 10);
        assert_eq!(h.counts.len(), 10);
        assert!(h.latency_buckets_ms.contains(&10.0) && h.latency_buckets_ms.contains(&50.0));
        assert_eq!(h.counts.iter().flatten().sum::<u64>(), ops.len() as u64);
    }
}
