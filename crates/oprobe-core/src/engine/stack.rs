//! Models of OpenDAL layer behaviour the analysis replays (the write chunk OpenDAL applies,
//! `TailCutLayer`'s deadline rule, `RetryLayer`'s backoff). The stack itself is built from the
//! operator spec ([`crate::operator::build`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Knobs;
use crate::trace::EffectiveKnobs;

/// The chunk OpenDAL writes with when asked for `requested` bytes: it raises it to the
/// service's minimum multipart part size and lowers it to the maximum
/// (`opendal_core` `WriteContext::calculate_chunk_size`).
pub fn effective_write_chunk(cap: &opendal::Capability, requested: usize) -> usize {
    let mut size = requested;
    if let Some(max) = cap.write_multi_max_size {
        size = size.min(max);
    }
    if let Some(min) = cap.write_multi_min_size {
        size = size.max(min);
    }
    size
}

/// The effective knobs for `k` on a service with capability `cap`.
pub fn effective_knobs(cap: &opendal::Capability, k: &Knobs) -> EffectiveKnobs {
    EffectiveKnobs {
        requested_chunk_size: k.chunk_size.0,
        write_chunk_size: effective_write_chunk(cap, k.chunk_size.as_usize()) as u64,
        service_min_part_size: cap.write_multi_min_size.map(|v| v as u64),
        service_max_part_size: cap.write_multi_max_size.map(|v| v as u64),
    }
}

/// Longest per-I/O timeout (`TimeoutLayer::with_io_timeout`).
pub const MAX_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// `RetryLayer` parameters (exponential backoff, OpenDAL's default factor of 2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryParams {
    pub max_times: usize,
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub factor: f32,
    pub jitter: bool,
}

/// `TailCutLayer` parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TailCutParams {
    pub percentile: u8,
    pub safety_factor: f64,
    pub min_samples: usize,
    pub min_deadline: Duration,
    pub max_deadline: Duration,
    pub window: Duration,
}

impl TailCutParams {
    /// The parameters oprobe's client uses with a request `timeout` (the deadline's ceiling).
    pub fn for_timeout(timeout: Duration) -> Self {
        Self {
            percentile: 95,
            safety_factor: 1.5,
            min_samples: 32,
            min_deadline: Duration::from_millis(50),
            max_deadline: timeout,
            window: Duration::from_secs(60),
        }
    }

    /// The deadline `TailCutLayer` sets for the next call, given the latencies it recorded
    /// (`h`): none below `min_samples`, else the `percentile` of the histogram (its
    /// power-of-two bucket's upper bound) × `safety_factor`, clamped to
    /// `[min_deadline, max_deadline]`. This is the layer's own rule (`opendal-layer-tail-cut`
    /// 0.59 `calculate_deadline`); the simulator replays traces with it.
    pub fn deadline(&self, h: &TailCutHistogram) -> Option<Duration> {
        if (h.total() as usize) < self.min_samples {
            return None;
        }
        let pctl = h.quantile(f64::from(self.percentile) / 100.0)?;
        let d = Duration::from_secs_f64(pctl.as_secs_f64() * self.safety_factor);
        Some(d.clamp(self.min_deadline, self.max_deadline.max(self.min_deadline)))
    }
}

/// How long `TailCutLayer` cuts nothing after the client is built.
///
/// OpenDAL 0.59's windowed histogram only counts time slices whose start time is set, and the
/// first slice's never is: every latency recorded during the layer's first 10 s slice is
/// ignored, so the layer has no samples, and so no deadline, until 10 s (plus `min_samples`
/// calls) after the operator was built. A run shorter than that is never tail-cut; a
/// long-lived client is unaffected. (Measured on the lab: a 10 s run cut 8 of ~270
/// stragglers, all in its last half second.)
pub const TAIL_CUT_BLIND_START: Duration = Duration::from_secs(10);

/// Buckets of `TailCutLayer`'s latency histogram (power-of-two milliseconds, 1 ms to 64 s).
const TAIL_CUT_BUCKETS: usize = 17;

/// A replica of `TailCutLayer`'s latency histogram (`opendal-layer-tail-cut` 0.59
/// `WindowedHistogram`, one window): bucket `b` holds latencies in `[2^(b-1), 2^b)` ms and
/// reads back as `2^b` ms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailCutHistogram {
    counts: [u64; TAIL_CUT_BUCKETS],
}

impl TailCutHistogram {
    fn bucket(latency: Duration) -> usize {
        let ms = latency.as_millis() as u64;
        if ms == 0 {
            return 0;
        }
        ((64 - ms.leading_zeros()) as usize).min(TAIL_CUT_BUCKETS - 1)
    }

    fn bucket_latency(b: usize) -> Duration {
        match b {
            0 => Duration::from_millis(1),
            b if b >= TAIL_CUT_BUCKETS - 1 => Duration::from_secs(64),
            b => Duration::from_millis(1u64 << b),
        }
    }

    /// Record one successful call.
    pub fn record(&mut self, latency: Duration) {
        self.counts[Self::bucket(latency)] += 1;
    }

    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The layer's quantile: the upper bound of the bucket holding the `q`-th sample.
    pub fn quantile(&self, q: f64) -> Option<Duration> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let target = (total as f64 * q).ceil() as u64;
        let mut cum = 0;
        for (b, n) in self.counts.iter().enumerate() {
            cum += n;
            if cum >= target {
                return Some(Self::bucket_latency(b));
            }
        }
        Some(Self::bucket_latency(TAIL_CUT_BUCKETS - 1))
    }
}

impl RetryParams {
    /// The parameters of an operator's `layers.retry`.
    pub fn of(r: &crate::operator::Retry) -> Self {
        Self {
            max_times: r.max_times,
            min_delay: r.min_delay.get(),
            max_delay: r.max_delay.get(),
            factor: r.factor,
            jitter: r.jitter,
        }
    }

    /// Mean wait before the first retry: `min_delay`, plus on average half of it with jitter.
    pub fn first_backoff_mean(&self) -> Duration {
        if self.jitter {
            self.min_delay.mul_f64(1.5)
        } else {
            self.min_delay
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn tail_cut_deadline_follows_the_layer_rule() {
        let p = TailCutParams::for_timeout(Duration::from_secs(30));
        let mut h = TailCutHistogram::default();
        for _ in 0..31 {
            h.record(ms(9));
        }
        assert_eq!(p.deadline(&h), None, "fewer than 32 samples");
        h.record(ms(9));
        // 9 ms falls in [8, 16) and reads back as 16 ms; × 1.5 = 24 ms, floored at 50 ms.
        assert_eq!(h.quantile(0.95), Some(ms(16)));
        assert_eq!(p.deadline(&h), Some(ms(50)));
        // 5 % stragglers at 250 ms leave the p95 in the body of the distribution …
        let mut h = TailCutHistogram::default();
        for i in 0..1000 {
            h.record(if i % 40 == 0 { ms(258) } else { ms(9) });
        }
        assert_eq!(p.deadline(&h), Some(ms(50)));
        // … but with more than 5 % of them the p95 *is* a straggler: nothing gets cut.
        let mut h = TailCutHistogram::default();
        for i in 0..1000 {
            h.record(if i % 10 == 0 { ms(258) } else { ms(9) });
        }
        assert_eq!(p.deadline(&h), Some(ms(768)));
        // The deadline never exceeds the request timeout.
        let short = TailCutParams::for_timeout(ms(500));
        assert_eq!(short.deadline(&h), Some(ms(500)));
        assert_eq!(TailCutParams::for_timeout(ms(500)).min_samples, 32);
    }

    #[test]
    fn retry_backoff_mean() {
        let r = RetryParams::of(&crate::operator::Retry {
            min_delay: crate::util::HumanDuration(ms(50)),
            jitter: true,
            ..Default::default()
        });
        assert_eq!(r.first_backoff_mean(), ms(75));
    }
}
