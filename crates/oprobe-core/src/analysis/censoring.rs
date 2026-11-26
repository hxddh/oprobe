//! Latency quantiles of an open-loop run with right-censored arrivals.
//!
//! An open-loop arrival that never completed (it never got a slot before the window ended, or
//! was dropped behind schedule) has no latency: all that is known is a lower bound, the time
//! it had already waited. Its latency is *at least* that, and nothing says how much more: a
//! stalled target never answers at all.
//!
//! The recorder keeps every arrival in one distribution, a censored one at its lower bound
//! (design v6, T3: no coordinated omission). Reading a quantile off that distribution treats a
//! censored arrival as if it had completed at its bound, which only bounds the quantile from
//! below. This module gives both sides:
//!
//! - **from below**, each censored arrival at its bound (the least it can be);
//! - **from above**, each censored arrival slower than everything measured (the worst it can
//!   be): the quantile of `N` arrivals with `m` censored then sits `m` ranks higher among the
//!   values recorded, and when that rank does not exist (more than `1 − q` of the arrivals
//!   were censored) nothing bounds the quantile from above.
//!
//! A limit on a quantile is met only when the bound from above meets it.

use crate::trace::LatencyHistogram;

/// What is known of a latency quantile when some arrivals are right-censored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CensoredQuantile {
    /// The quantile is at most this (ms), however slow the censored arrivals would have been.
    AtMost(f64),
    /// The quantile's rank falls among the censored arrivals: it is at least this (ms), and
    /// nothing bounds it from above.
    AtLeast(f64),
}

impl CensoredQuantile {
    /// The quantile is known to be within `limit_ms`.
    pub fn meets(&self, limit_ms: f64) -> bool {
        matches!(self, Self::AtMost(v) if *v <= limit_ms)
    }

    /// The bound itself (ms).
    pub fn bound_ms(&self) -> f64 {
        match self {
            Self::AtMost(v) | Self::AtLeast(v) => *v,
        }
    }
}

/// The bin holding the `rank`-th smallest value (1-based) of `h`.
fn bin_of_rank(h: &LatencyHistogram, rank: u64) -> Option<usize> {
    let mut seen = 0u64;
    h.counts.iter().position(|n| {
        seen += n;
        seen >= rank
    })
}

/// Quantile `q` (0–1) of the arrivals in `all`, `censored` of which are recorded at their
/// lower bound instead of a latency. `None` for an empty distribution.
///
/// The values are a histogram's: a bound from above is the upper edge of its bin (never above
/// the slowest value), a bound from below the lower edge.
pub fn censored_quantile(
    all: &LatencyHistogram,
    censored: u64,
    q: f64,
) -> Option<CensoredQuantile> {
    let total = all.total();
    if total == 0 {
        return None;
    }
    let rank = ((total as f64 * q.clamp(0.0, 1.0)).ceil() as u64).clamp(1, total);
    // Censored arrivals taken as slower than every value: the quantile moves up by their number.
    match rank
        .checked_add(censored)
        .filter(|r| *r <= total)
        .and_then(|r| bin_of_rank(all, r))
    {
        Some(i) => Some(CensoredQuantile::AtMost(
            all.upper_ms[i].min(all.max_ms).max(all.lower_ms[i]),
        )),
        None => bin_of_rank(all, rank).map(|i| CensoredQuantile::AtLeast(all.lower_ms[i])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic open-loop run: one arrival every `gap_ms` over `window_ms`; the target
    /// answers in `service_ms` until it stalls at `stall_ms`, and every later arrival is
    /// censored at the window end. Returns the distribution as the recorder keeps it, and
    /// the number censored.
    fn stalled(
        window_ms: u64,
        gap_ms: u64,
        service_ms: u64,
        stall_ms: u64,
    ) -> (LatencyHistogram, u64) {
        let mut values = Vec::new();
        let mut censored = 0;
        for at in (0..window_ms).step_by(gap_ms as usize) {
            if at < stall_ms {
                values.push((service_ms * 1000, 1));
            } else {
                // Its latency is at least the time left in the window: recorded at that.
                values.push(((window_ms - at) * 1000, 1));
                censored += 1;
            }
        }
        (LatencyHistogram::from_counts(values), censored)
    }

    #[test]
    fn a_quantile_among_the_censored_is_only_bounded_from_below() {
        // 1 000 arrivals; the target stalls after 2 s of 10: 800 never complete.
        let (h, censored) = stalled(10_000, 10, 5, 2_000);
        assert_eq!((h.total(), censored), (1000, 800));
        let p99 = censored_quantile(&h, censored, 0.99).unwrap();
        // The 990th smallest of 200 × 5 ms and bounds of 10 ms to 8 s is the bound of the
        // arrival that had waited ≈ 7.9 s: the p99 is at least that, not the 5 ms measured.
        let CensoredQuantile::AtLeast(ms) = p99 else {
            panic!("{p99:?}");
        };
        assert!((7_000.0..=7_900.0).contains(&ms), "{ms}");
        assert!(
            !p99.meets(60_000.0),
            "no limit is met by an unbounded quantile"
        );
        // The median is among the censored too (80 % are); the p10 is not: 100 + 800 ≤ 1 000.
        assert!(matches!(
            censored_quantile(&h, censored, 0.5),
            Some(CensoredQuantile::AtLeast(_))
        ));
        let p10 = censored_quantile(&h, censored, 0.10).unwrap();
        assert!(matches!(p10, CensoredQuantile::AtMost(_)), "{p10:?}");
    }

    #[test]
    fn a_few_censored_arrivals_move_the_quantile_up_by_their_rank() {
        // 988 arrivals of 10 ms, 10 of 500 ms, 2 censored after waiting 1 ms.
        let h = LatencyHistogram::from_counts([(10_000, 988), (500_000, 10), (1_000, 2)]);
        // Read as if the two had completed in 1 ms, the p99 is 10 ms.
        let naive = h.quantile_ms(0.99).unwrap();
        assert!(naive < 12.0, "{naive}");
        // They may have been the slowest: the 990th of 1 000 is then one of the 500 ms.
        let p99 = censored_quantile(&h, 2, 0.99).unwrap();
        let CensoredQuantile::AtMost(ms) = p99 else {
            panic!("{p99:?}");
        };
        assert!((500.0..=562.0).contains(&ms), "{ms}");
        assert!(ms >= naive, "never below what the arrivals imply");
        assert!(p99.meets(600.0) && !p99.meets(400.0));
        // Nothing censored: the histogram's own quantile.
        assert_eq!(
            censored_quantile(&h, 0, 0.99),
            Some(CensoredQuantile::AtMost(naive))
        );
        assert_eq!(
            censored_quantile(&LatencyHistogram::default(), 0, 0.99),
            None
        );
    }
}
