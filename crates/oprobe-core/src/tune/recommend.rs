//! The tuner's answer as a recommendation (design v4 §6): the winning knobs as a profile, a
//! code snippet and an override set, with its deltas against the baseline and against the best
//! measured trial (what the safety margin costs), and — once `verify` ran — its outcome.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Objective, Trial};
use crate::analysis::verification::VerifyOutcome;
use crate::config::{Knobs, Profile};
use crate::model::KnobValues;
use crate::model::spec::{Overrides, REDACTED, RunSpec};
use crate::report::TuneSummary;

/// One objective metric, recommended vs a reference trial.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Delta {
    /// `ops_per_s` / `mib_per_s` (goodput), `p99_ms` (latency) or
    /// `cost_per_million_ops_usd`.
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
    /// A profile with the recommended knobs (environment defaults such as `proxy` and
    /// redacted secrets omitted).
    pub profile_toml: String,
    /// Rust/OpenDAL code configuring the same client.
    pub rust_snippet: String,
    /// Against the baseline trial (`baseline_feasible: false` when it violated the SLA or
    /// the throttle budget).
    pub vs_baseline: Delta,
    /// Against the best feasible trial: the cost of the safety margin.
    pub vs_best: Delta,
    /// No trial met the SLA: this is the least-bad configuration, not a recommendation.
    pub no_feasible_trial: bool,
    /// What `oprobe verify` concluded about the recommendation (set by `oprobe fix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<VerifyOutcome>,
}

/// `(metric, unit, value)` of a trial for an objective.
fn metric(o: Objective, mib: bool, t: &Trial) -> (&'static str, &'static str, f64) {
    match o {
        Objective::Throughput | Objective::Balanced if mib => {
            ("mib_per_s", "mib_per_s", t.mib_per_s)
        }
        Objective::Throughput | Objective::Balanced => ("ops_per_s", "ops_per_s", t.ops_per_s),
        Objective::Latency => ("p99_ms", "ms", t.p99_ms),
    }
}

fn delta(o: Objective, mib: bool, reference: &Trial, winner: &Trial) -> Delta {
    let (m, unit, b) = metric(o, mib, reference);
    let (_, _, c) = metric(o, mib, winner);
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

/// The profile a recommendation writes: the spec's profile (the target as the user named it,
/// with only what was added on top), without environment defaults (`proxy = "none"`), redacted
/// secrets or the run label.
pub fn portable_profile(spec: &RunSpec) -> Profile {
    let mut p = spec.resolved_profile();
    p.name = None;
    p.target.options.retain(|_, v| v != REDACTED);
    if p.target.proxy.as_deref() == Some("none") {
        p.target.proxy = None;
    }
    p
}

impl Recommendation {
    /// Build the recommendation from a finished tuner search: `spec` is the tuned run's spec
    /// (before the winner was applied), `session` the profile the session ran (normalised
    /// target, for the code snippet).
    pub fn from_tune(
        spec: &RunSpec,
        session: &Profile,
        winner: &Knobs,
        summary: &TuneSummary,
        capability: &opendal::Capability,
    ) -> anyhow::Result<Self> {
        let o = Objective::parse(&summary.objective).unwrap_or_default();
        let mib = summary.metric_unit == "mib_per_s";
        let find = |id: usize| summary.trials.iter().find(|t| t.id == id);
        let win = find(summary.recommended_trial).or_else(|| find(summary.winner_trial));
        let base = find(summary.baseline_trial);
        let better = |a: &Trial, b: &Trial| -> bool {
            let (_, _, x) = metric(o, mib, a);
            let (_, _, y) = metric(o, mib, b);
            match o {
                Objective::Throughput | Objective::Balanced => x > y,
                _ => x < y,
            }
        };
        // The best measured trial, feasible first (any trial when none is).
        let best = summary.best_feasible_trial.and_then(find).or_else(|| {
            let any_feasible = summary.trials.iter().any(|t| t.feasible);
            summary
                .trials
                .iter()
                .filter(|t| t.stage != "confirm" && (t.feasible || !any_feasible))
                .fold(None::<&Trial>, |acc, t| match acc {
                    Some(a) if !better(t, a) => Some(a),
                    _ => Some(t),
                })
        });
        let (vs_baseline, vs_best) = match (win, base, best) {
            (Some(w), Some(b), Some(best)) => (delta(o, mib, b, w), delta(o, mib, best, w)),
            (Some(w), Some(b), None) => (delta(o, mib, b, w), delta(o, mib, w, w)),
            _ => Default::default(),
        };
        let header = format!(
            "oprobe tune · objective {} · {}",
            summary.objective, summary.headline
        );
        let tp = portable_profile(spec);
        let mut overrides = knob_overrides(&spec.profile.knobs, winner);
        // A paced winner (rate limits): the pace is part of the change.
        if let Some(r) = win.and_then(|w| w.rate_per_s)
            && Some(r) != spec.profile.workload.rate
        {
            let _ = overrides.set("rate", &format!("{r:.0}"));
        }
        Ok(Self {
            objective: summary.objective.clone(),
            knobs: winner.into(),
            overrides,
            profile_toml: super::codegen::profile_toml(&tp, winner, &header)?,
            rust_snippet: super::codegen::rust_snippet(&session.target, winner, capability),
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
            p50_ms: 1.0,
            p99_ms: 2.0,
            feasible,
            ..Default::default()
        }
    }

    #[test]
    fn deltas_against_baseline_and_best() {
        let mut p = Profile::from_target(crate::config::TargetSpec::from_url("memory://").unwrap());
        p.target.proxy = Some("none".into());
        p.target
            .options
            .insert("secret_access_key".into(), "hunter2".into());
        let spec = RunSpec::new(&p, "memory://");
        let s = TuneSummary {
            objective: "goodput".into(),
            metric_unit: "ops_per_s".into(),
            trials: vec![
                trial(0, 16, 100.0, false),
                trial(1, 8, 150.0, true),
                trial(2, 12, 180.0, true),
                trial(3, 24, 400.0, false),
            ],
            baseline_trial: 0,
            recommended_trial: 1,
            winner_trial: 1,
            best_feasible_trial: Some(2),
            baseline_feasible: false,
            improvement: None,
            headline: "baseline infeasible".into(),
            ..Default::default()
        };
        let winner = s.trials[1].knobs.clone();
        let r = Recommendation::from_tune(&spec, &p, &winner, &s, &opendal::Capability::default())
            .unwrap();
        assert_eq!(r.knobs.concurrency_count, 8);
        assert_eq!(r.overrides.knobs["concurrency"], "8");
        assert_eq!(r.overrides.knobs.len(), 1);
        assert!(!r.vs_baseline.baseline_feasible);
        assert!((r.vs_baseline.delta_ratio - 0.5).abs() < 1e-9);
        // The best feasible trial (12 workers, 180 ops/s), not the infeasible 400.
        assert_eq!(r.vs_best.baseline_trial, 2);
        assert!(r.vs_best.delta_ratio < 0.0);
        assert!(
            r.profile_toml.contains("concurrency = 8"),
            "{}",
            r.profile_toml
        );
        assert!(!r.profile_toml.contains("proxy"), "{}", r.profile_toml);
        assert!(!r.profile_toml.contains("redacted"), "{}", r.profile_toml);
        assert!(!r.profile_toml.contains("hunter2"), "{}", r.profile_toml);
        assert!(!r.rust_snippet.is_empty());
    }
}
