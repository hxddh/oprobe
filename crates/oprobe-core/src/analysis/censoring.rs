//! A stalled open loop: scheduled arrivals that never completed, and what they leave known of
//! the p99.
//!
//! An open-loop arrival that never completed (it never got a slot before the window ended, or
//! was dropped behind schedule) has no latency: all that is known is a lower bound, the time
//! it had already waited. The recorder keeps every arrival in one distribution, a censored one
//! at its lower bound (design v6, T3: no coordinated omission), so a quantile read off that
//! distribution is bounded from below only.
//!
//! From above, a censored arrival may have been slower than everything measured: the p99 of
//! `N` arrivals with `m` censored then sits `m` ranks higher. When that rank does not exist —
//! `⌈0.99·N⌉ + m > N`, more than 1 % of the arrivals never completed — nothing bounds the p99
//! from above, whatever the operations that did complete measured. Such a run is stalled, and
//! it is judged so with no limit to compare against.

use crate::trace::OpenLoopStats;

/// An open-loop run whose p99 has no upper bound: its rank falls among the arrivals that
/// never completed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StalledOpenLoop {
    /// Scheduled arrivals that never completed.
    pub censored_count: u64,
    /// Every scheduled arrival of the window.
    pub arrivals_count: u64,
    /// The p99 over every arrival, each censored one at the time it had waited: the least
    /// the p99 can be. It is the run's own p99 (`summary.latency.p99_ms`), the same estimator.
    pub p99_at_least_ms: f64,
}

impl StalledOpenLoop {
    /// The stall of an open-loop run with the p99 `p99_ms` over all its arrivals; `None` for
    /// a closed loop, and while the p99 is still bounded from above (at most 1 % censored).
    pub fn of(open_loop: Option<&OpenLoopStats>, p99_ms: f64) -> Option<Self> {
        let o = open_loop?;
        let censored = o.censored_count.max(o.missed);
        // A report from before the schedule counted its arrivals cannot say what share.
        let arrivals = o.intended_count;
        // The rank of the p99 with every censored arrival taken as slower than any value.
        let rank = (arrivals as f64 * 0.99).ceil() as u64;
        (arrivals > 0 && rank.saturating_add(censored) > arrivals).then_some(Self {
            censored_count: censored,
            arrivals_count: arrivals,
            p99_at_least_ms: p99_ms,
        })
    }

    /// Share of the scheduled arrivals that never completed (0–1).
    pub fn censored_ratio(&self) -> f64 {
        self.censored_count as f64 / self.arrivals_count.max(1) as f64
    }

    /// The stall in one sentence, with its arithmetic.
    pub fn describe(&self) -> String {
        format!(
            "{} of {} scheduled arrivals never completed ({:.1}%): the p99 is at least {} and has no upper bound",
            self.censored_count,
            self.arrivals_count,
            self.censored_ratio() * 100.0,
            crate::util::fmt_ms(self.p99_at_least_ms)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_loop(censored: u64, arrivals: u64) -> OpenLoopStats {
        OpenLoopStats {
            missed: censored,
            censored_count: censored,
            intended_count: arrivals,
            ..Default::default()
        }
    }

    /// The p99 of 1 000 arrivals is the 990th: with 10 censored the 1 000th still bounds it,
    /// with 11 nothing does.
    #[test]
    fn a_run_is_stalled_when_the_p99_rank_falls_among_the_censored() {
        assert_eq!(StalledOpenLoop::of(None, 5.0), None);
        assert_eq!(StalledOpenLoop::of(Some(&open_loop(0, 1000)), 5.0), None);
        assert_eq!(StalledOpenLoop::of(Some(&open_loop(10, 1000)), 5.0), None);
        let s = StalledOpenLoop::of(Some(&open_loop(11, 1000)), 5.0).unwrap();
        assert_eq!((s.censored_count, s.arrivals_count), (11, 1000));
        let s = StalledOpenLoop::of(Some(&open_loop(800, 1000)), 7_900.0).unwrap();
        assert!((s.censored_ratio() - 0.8).abs() < 1e-12);
        assert_eq!(
            s.describe(),
            "800 of 1000 scheduled arrivals never completed (80.0%): the p99 is at least 7.90s and has no upper bound"
        );
    }

    /// W2 (9.2): the bound was read off the lower edge of a histogram bin, so the check said
    /// "at least 7.08 s" beside a summary p99 of 7.9 s. The bound is the summary's p99 itself.
    #[test]
    fn the_lower_bound_is_the_runs_own_p99() {
        let s = StalledOpenLoop::of(Some(&open_loop(800, 1000)), 7_912.3).unwrap();
        assert_eq!(s.p99_at_least_ms, 7_912.3);
    }

    /// A report written before the schedule counted its arrivals cannot say what share of
    /// them was missed: it is not judged.
    #[test]
    fn a_report_without_an_intended_count_is_not_judged() {
        let o = OpenLoopStats {
            missed: 40,
            ..Default::default()
        };
        assert_eq!(StalledOpenLoop::of(Some(&o), 100.0), None);
    }
}
