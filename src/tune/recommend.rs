//! The tuner's answer as a recommendation (design v4 §6): the winning knobs as a profile, a
//! code snippet and an override set, with its deltas against the baseline and against the best
//! measured trial (what the safety margin costs), and — once `verify` ran — its outcome.
//!
//! Stream T owns the search; this module defines the result shape and builds it from the
//! tuner's summary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Objective, Trial};
use crate::analysis::verification::VerifyOutcome;
use crate::config::{Knobs, Profile};
use crate::model::KnobValues;
use crate::model::spec::Overrides;
use crate::report::TuneSummary;

/// One objective metric, recommended vs a reference trial.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Delta {
    /// `ops_per_s` (throughput, goodput), `p99_ms` (latency) or `cost_per_million_ops_usd`.
    pub metric: String,
    /// Unit of `baseline` and `candidate`.
    pub unit: String,
    /// The reference trial's value.
    pub baseline: f64,
    /// The recommended configuration's value.
    pub candidate: f64,
    /// `(candidate − baseline) / baseline` (0 when the baseline is 0).
    pub delta_ratio: f64,
    /// The reference met the SLA and the throttle budget; deltas against an infeasible
    /// reference are not improvements.
    pub baseline_feasible: bool,
    /// Trial id of the reference.
    pub baseline_trial: u64,
}

/// What `oprobe tune` / `oprobe fix` / MCP `recommend_config` recommend.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Recommendation {
    pub objective: String,
    /// The recommended knob values.
    pub knobs: KnobValues,
    /// The change from the baseline's knobs (`--set` vocabulary): apply with
    /// `oprobe run --from RUN …` or `RunSpec::with`.
    pub overrides: Overrides,
    /// A profile with the recommended knobs (environment defaults such as `proxy` omitted).
    pub profile_toml: String,
    /// Rust/OpenDAL code configuring the same client.
    pub rust_snippet: String,
    pub vs_baseline: Delta,
    /// Against the best measured trial: the cost of the safety margin.
    pub vs_best: Delta,
    /// No trial met the SLA: this is the least-bad configuration, not a recommendation.
    pub no_feasible_trial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<VerifyOutcome>,
}

fn objective_of(label: &str) -> Objective {
    match label {
        "latency" => Objective::Latency,
        "cost" => Objective::Cost,
        _ => Objective::Throughput,
    }
}

/// `(metric, unit, value)` of a trial for an objective.
fn metric(o: Objective, t: &Trial) -> (&'static str, &'static str, f64) {
    match o {
        Objective::Throughput => ("ops_per_s", "ops_per_s", t.ops_per_s),
        Objective::Latency => ("p99_ms", "ms", t.p99_ms),
        Objective::Cost => ("cost_per_million_ops_usd", "usd", t.cost_per_million_ops),
    }
}

fn delta(o: Objective, reference: &Trial, winner: &Trial) -> Delta {
    let (m, unit, b) = metric(o, reference);
    let (_, _, c) = metric(o, winner);
    Delta {
        metric: m.into(),
        unit: unit.into(),
        baseline: b,
        candidate: c,
        delta_ratio: if b.abs() > f64::EPSILON {
            (c - b) / b
        } else {
            0.0
        },
        baseline_feasible: reference.feasible,
        baseline_trial: reference.id as u64,
    }
}

/// The knob changes from `from` to `to` as `--set` overrides.
pub fn knob_overrides(from: &Knobs, to: &Knobs) -> Overrides {
    let mut o = Overrides::new();
    let mut set = |k: &str, a: String, b: String| {
        if a != b {
            let _ = o.set(k, &b);
        }
    };
    set(
        "concurrency",
        from.concurrency.to_string(),
        to.concurrency.to_string(),
    );
    set(
        "io_concurrency",
        from.io_concurrency.to_string(),
        to.io_concurrency.to_string(),
    );
    set(
        "chunk_size",
        from.chunk_size.to_string(),
        to.chunk_size.to_string(),
    );
    set(
        "range_gap",
        from.range_gap.to_string(),
        to.range_gap.to_string(),
    );
    set(
        "max_inflight_requests",
        from.max_inflight_requests.to_string(),
        to.max_inflight_requests.to_string(),
    );
    let http = |k: &Knobs| format!("{:?}", k.http_version).to_ascii_lowercase();
    set("http_version", http(from), http(to));
    set(
        "keep_alive",
        from.keep_alive.to_string(),
        to.keep_alive.to_string(),
    );
    let cache = |k: &Knobs| {
        if k.cache {
            k.cache_size.to_string()
        } else {
            "off".into()
        }
    };
    set("cache", cache(from), cache(to));
    set("retries", from.retries.to_string(), to.retries.to_string());
    set(
        "tail_cut",
        from.tail_cut.to_string(),
        to.tail_cut.to_string(),
    );
    o
}

impl Recommendation {
    /// Build the recommendation from a finished tuner search over `profile` (the profile the
    /// session ran, normalised target) whose winner is `winner`.
    pub fn from_tune(
        profile: &Profile,
        winner: &Knobs,
        summary: &TuneSummary,
        capability: &opendal::Capability,
    ) -> anyhow::Result<Self> {
        let o = objective_of(&summary.objective);
        let find = |id: usize| summary.trials.iter().find(|t| t.id == id);
        let win = find(summary.winner_trial);
        let base = find(summary.baseline_trial);
        let better = |a: &Trial, b: &Trial| -> bool {
            let (_, _, x) = metric(o, a);
            let (_, _, y) = metric(o, b);
            match o {
                Objective::Throughput => x > y,
                _ => x < y,
            }
        };
        // The best measured trial, feasible first (any trial when none is).
        let any_feasible = summary.trials.iter().any(|t| t.feasible);
        let best = summary
            .trials
            .iter()
            .filter(|t| t.feasible || !any_feasible)
            .fold(None::<&Trial>, |acc, t| match acc {
                Some(a) if !better(t, a) => Some(a),
                _ => Some(t),
            });
        let (vs_baseline, vs_best) = match (win, base, best) {
            (Some(w), Some(b), Some(best)) => (delta(o, b, w), delta(o, best, w)),
            _ => Default::default(),
        };
        let header = format!("oprobe tune · objective {}", summary.objective);
        let mut tp = profile.clone();
        // Environment defaults are not recommendations.
        if tp.target.proxy.as_deref() == Some("none") {
            tp.target.proxy = None;
        }
        Ok(Self {
            objective: summary.objective.clone(),
            knobs: winner.into(),
            overrides: knob_overrides(&profile.knobs, winner),
            profile_toml: super::codegen::profile_toml(&tp, winner, &header)?,
            rust_snippet: super::codegen::rust_snippet(&profile.target, winner, capability),
            vs_baseline,
            vs_best,
            no_feasible_trial: summary.no_feasible_trial,
            confirmed: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(id: usize, c: usize, ops: f64, feasible: bool) -> Trial {
        Trial {
            id,
            stage: "concurrency".into(),
            knobs: Knobs {
                concurrency: c,
                ..Knobs::default()
            },
            ops_per_s: ops,
            mib_per_s: 0.0,
            p50_ms: 1.0,
            p99_ms: 2.0,
            error_rate: 0.0,
            throttle_rate: 0.0,
            throttled_responses: 0,
            cost_per_million_ops: 0.4,
            feasible,
            note: String::new(),
        }
    }

    #[test]
    fn deltas_against_baseline_and_best() {
        let p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        let s = TuneSummary {
            objective: "throughput".into(),
            trials: vec![
                trial(1, 16, 100.0, false),
                trial(2, 8, 150.0, true),
                trial(3, 12, 180.0, true),
                trial(4, 24, 400.0, false),
            ],
            baseline_trial: 1,
            winner_trial: 2,
            improvement: 0.5,
            throttle_ceiling: None,
            no_feasible_trial: false,
            notes: vec![],
        };
        let winner = s.trials[1].knobs.clone();
        let r =
            Recommendation::from_tune(&p, &winner, &s, &opendal::Capability::default()).unwrap();
        assert_eq!(r.knobs.concurrency_count, 8);
        assert_eq!(r.overrides.knobs["concurrency"], "8");
        assert_eq!(r.overrides.knobs.len(), 1);
        assert!(!r.vs_baseline.baseline_feasible);
        assert!((r.vs_baseline.delta_ratio - 0.5).abs() < 1e-9);
        // The best feasible trial (12 workers, 180 ops/s), not the infeasible 400.
        assert_eq!(r.vs_best.baseline_trial, 3);
        assert!(r.vs_best.delta_ratio < 0.0);
        assert!(
            r.profile_toml.contains("concurrency = 8"),
            "{}",
            r.profile_toml
        );
        assert!(!r.rust_snippet.is_empty());
    }
}
