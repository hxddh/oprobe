//! Time series for the charts, built in Rust before anything is drawn (downsampled to at most
//! a few hundred points). The last bucket of a window is usually partial: its rates are scaled
//! by the share of the bucket the window covers, and a bucket covering under a quarter is
//! dropped, so no chart ends in a spike or a dip that is only an artefact of the cut.

use crate::model::OpRow;
use crate::trace::TimelinePoint;

/// One point of the throughput / latency charts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Bucket start, seconds from the window start.
    pub t_s: f64,
    pub ops_per_s: f64,
    pub mib_per_s: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub errors_per_s: f64,
    pub throttled_per_s: f64,
}

/// A series of evenly spaced buckets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    pub bucket_s: f64,
    pub points: Vec<Point>,
    /// The measurement window (s): the time axis spans `[0, window_s]`.
    pub window_s: f64,
}

/// Most buckets a chart gets.
pub const MAX_BUCKETS: usize = 240;

/// A nice bucket width (s) for `window_s` with at most [`MAX_BUCKETS`] buckets.
pub fn bucket_width(window_s: f64) -> f64 {
    // About 60 buckets: enough shape, enough samples per bucket for a p99.
    let raw = (window_s / 60.0).max(window_s / MAX_BUCKETS as f64);
    [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ]
    .into_iter()
    .find(|w| *w >= raw)
    .unwrap_or(600.0)
}

impl Series {
    /// From the operation rows of a finished run.
    pub fn from_ops(ops: &[OpRow], window_s: f64) -> Self {
        let window_s = window_s.max(0.0);
        let w = bucket_width(window_s.max(0.1));
        let n = ((window_s / w).ceil() as usize).max(1);
        let mut lat: Vec<Vec<f64>> = vec![Vec::new(); n];
        let mut bytes = vec![0u64; n];
        let mut errors = vec![0u64; n];
        let mut throttled = vec![0u64; n];
        for o in ops {
            if o.t_us < 0 {
                continue;
            }
            let i = (o.t_us as f64 / (w * 1e6) + 1e-9).floor() as usize;
            if i >= n {
                continue;
            }
            lat[i].push(o.latency_us as f64 / 1000.0);
            bytes[i] += o.bytes;
            if !o.ok {
                errors[i] += 1;
            }
            if o.throttled {
                throttled[i] += 1;
            }
        }
        let mut points = Vec::with_capacity(n);
        for i in 0..n {
            let start = i as f64 * w;
            let covered = ((window_s - start) / w).clamp(0.0, 1.0);
            if covered < 0.25 && i > 0 {
                break;
            }
            let span = w * covered.max(1e-9);
            let l = &mut lat[i];
            let (p50, p99) = quantiles(l);
            points.push(Point {
                t_s: start,
                ops_per_s: l.len() as f64 / span,
                mib_per_s: bytes[i] as f64 / 1_048_576.0 / span,
                p50_ms: p50,
                p99_ms: p99,
                errors_per_s: errors[i] as f64 / span,
                throttled_per_s: throttled[i] as f64 / span,
            });
        }
        Self {
            bucket_s: w,
            points,
            window_s,
        }
    }

    /// From the live engine's per-second timeline (its last bucket is already scaled by the
    /// covered share; one that started under 250 ms ago is dropped).
    pub fn from_timeline(tl: &[TimelinePoint], elapsed_s: f64) -> Self {
        let mut points: Vec<Point> = tl
            .iter()
            .map(|p| Point {
                t_s: p.t_s,
                ops_per_s: p.ops_per_s,
                mib_per_s: p.mib_per_s,
                p50_ms: p.p50_ms,
                p99_ms: p.p99_ms,
                errors_per_s: p.errors as f64,
                throttled_per_s: p.throttled as f64,
            })
            .collect();
        if points.len() > 1 && points.last().is_some_and(|p| elapsed_s - p.t_s < 0.25) {
            points.pop();
        }
        // Long live runs: merge neighbours so the chart keeps at most MAX_BUCKETS points.
        let factor = points.len().div_ceil(MAX_BUCKETS).max(1);
        if factor > 1 {
            points = points
                .chunks(factor)
                .map(|c| {
                    let k = c.len() as f64;
                    Point {
                        t_s: c[0].t_s,
                        ops_per_s: c.iter().map(|p| p.ops_per_s).sum::<f64>() / k,
                        mib_per_s: c.iter().map(|p| p.mib_per_s).sum::<f64>() / k,
                        p50_ms: c.iter().map(|p| p.p50_ms).fold(0.0, f64::max),
                        p99_ms: c.iter().map(|p| p.p99_ms).fold(0.0, f64::max),
                        errors_per_s: c.iter().map(|p| p.errors_per_s).sum::<f64>() / k,
                        throttled_per_s: c.iter().map(|p| p.throttled_per_s).sum::<f64>() / k,
                    }
                })
                .collect();
        }
        Self {
            bucket_s: factor as f64,
            points,
            window_s: elapsed_s.max(1.0),
        }
    }

    /// Follow a live timeline: append the seconds that are new and refresh the last one
    /// (finished seconds never change), instead of rebuilding the series. Falls back to
    /// [`Self::from_timeline`] once long runs merge buckets.
    pub fn update_timeline(&mut self, tl: &[TimelinePoint], elapsed_s: f64) {
        if tl.len() > MAX_BUCKETS || (self.bucket_s != 1.0 && !self.points.is_empty()) {
            *self = Self::from_timeline(tl, elapsed_s);
            return;
        }
        let point = |p: &TimelinePoint| Point {
            t_s: p.t_s,
            ops_per_s: p.ops_per_s,
            mib_per_s: p.mib_per_s,
            p50_ms: p.p50_ms,
            p99_ms: p.p99_ms,
            errors_per_s: p.errors as f64,
            throttled_per_s: p.throttled as f64,
        };
        // Keep the points that are still the timeline's (all but the last, which may have
        // been partial); a timeline that does not start where ours did is rebuilt.
        let keep = self.points.len().saturating_sub(1).min(tl.len());
        let same_start = self
            .points
            .first()
            .zip(tl.first())
            .is_none_or(|(a, b)| a.t_s == b.t_s);
        if !same_start {
            *self = Self::from_timeline(tl, elapsed_s);
            return;
        }
        self.points.truncate(keep);
        self.points.extend(tl[keep..].iter().map(point));
        if self.points.len() > 1 && self.points.last().is_some_and(|p| elapsed_s - p.t_s < 0.25) {
            self.points.pop();
        }
        self.bucket_s = 1.0;
        self.window_s = elapsed_s.max(1.0);
    }

    /// The largest value of `f` over the points.
    pub fn max(&self, f: impl Fn(&Point) -> f64) -> f64 {
        self.points.iter().map(f).fold(0.0, f64::max)
    }

    /// The smallest positive value of `f` over the points.
    pub fn min_positive(&self, f: impl Fn(&Point) -> f64) -> f64 {
        self.points
            .iter()
            .map(f)
            .filter(|v| *v > 0.0)
            .fold(f64::INFINITY, f64::min)
    }

    /// The point whose bucket contains `t_s`.
    pub fn at(&self, t_s: f64) -> Option<&Point> {
        if self.bucket_s <= 0.0 {
            return None;
        }
        let i = (t_s / self.bucket_s).floor();
        if i < 0.0 {
            return None;
        }
        self.points.get(i as usize)
    }
}

/// (p50, p99) of `v` (sorted in place); zeros when empty.
pub fn quantiles(v: &mut [f64]) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable_by(f64::total_cmp);
    let q = |p: f64| v[((p * (v.len() - 1) as f64).round() as usize).min(v.len() - 1)];
    (q(0.5), q(0.99))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(t_ms: i64, lat_ms: u64) -> OpRow {
        OpRow {
            op_id: 0,
            verb: "read".into(),
            key: String::new(),
            t_us: t_ms * 1000,
            latency_us: lat_ms * 1000,
            bytes: 1_048_576,
            ok: true,
            error_code: None,
            error: None,
            throttled: false,
            attempts: 1,
            breakdown: Default::default(),
            requests: 1,
            await_tree: None,
        }
    }

    #[test]
    fn a_partial_last_bucket_is_scaled_not_a_dip() {
        // 10 ops per second, steady, for 2.5 s.
        let ops: Vec<OpRow> = (0..25).map(|i| op(i * 100, 5)).collect();
        let s = Series::from_ops(&ops, 2.5);
        assert!(s.bucket_s <= 0.5);
        for p in &s.points {
            assert!((p.ops_per_s - 10.0).abs() < 1e-6, "{p:?}");
        }
    }

    #[test]
    fn a_sliver_bucket_is_dropped() {
        let ops: Vec<OpRow> = (0..100).map(|i| op(i * 100, 5)).collect();
        let s = Series::from_ops(&ops, 10.02);
        // The bucket starting at 10.0 covers 2 % of its width: no point for it.
        assert!(s.points.last().unwrap().t_s < 10.0);
    }

    #[test]
    fn live_timelines_drop_a_bucket_just_started() {
        let tl: Vec<TimelinePoint> = (0..4)
            .map(|i| TimelinePoint {
                t_s: i as f64,
                ops_per_s: 10.0,
                ..Default::default()
            })
            .collect();
        assert_eq!(Series::from_timeline(&tl, 3.1).points.len(), 3);
        assert_eq!(Series::from_timeline(&tl, 3.6).points.len(), 4);
    }

    #[test]
    fn a_live_series_appends_what_is_new() {
        let tl = |n: usize, last: f64| -> Vec<TimelinePoint> {
            (0..n)
                .map(|i| TimelinePoint {
                    t_s: i as f64,
                    ops_per_s: if i + 1 == n { last } else { 10.0 },
                    ..Default::default()
                })
                .collect()
        };
        let mut s = Series::default();
        for n in 1..40 {
            let t = tl(n, 5.0 + n as f64);
            let elapsed = n as f64 - 0.4;
            s.update_timeline(&t, elapsed);
            assert_eq!(s, Series::from_timeline(&t, elapsed), "after {n} seconds");
        }
        // A new run (a timeline that starts elsewhere) is rebuilt.
        let mut other = tl(3, 1.0);
        for p in &mut other {
            p.t_s += 100.0;
        }
        s.update_timeline(&other, 102.6);
        assert_eq!(s, Series::from_timeline(&other, 102.6));
    }
}
