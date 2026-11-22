//! The Fix 5 menu (design v5 §4, fixes C3): `oprobe fix` answers with 2–4 Pareto options —
//! goodput, p99, throttling, cost per month and verify status each — instead of one "go
//! slower". Example: "0 % throttling at −28 % goodput, or 0.4 % throttling at −3 %".
//!
//! - The options are the measured trials not dominated on (goodput ↑, throttling ↓, p99 ↓),
//!   picked for distinct trade-offs: the best goodput within the throttle budget (recommended),
//!   the best with no throttling at all, the best goodput regardless of throttling and the
//!   lowest p99.
//! - The recommended option is the best goodput within the risk (the budget); it is never one
//!   whose measured goodput is worse than the diagnosis' when the diagnosis itself met the
//!   budget — then "keep the current configuration" is recommended.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::verification::VerifyOutcome;
use crate::model::spec::Overrides;
use crate::tune::Trial;

/// Whether an option was verified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OptionVerify {
    Confirmed,
    Refuted,
    Inconclusive,
    /// Measured by the tuner, not replayed against the diagnosis.
    #[default]
    NotVerified,
}

impl From<VerifyOutcome> for OptionVerify {
    fn from(o: VerifyOutcome) -> Self {
        match o {
            VerifyOutcome::Confirmed => Self::Confirmed,
            VerifyOutcome::Refuted => Self::Refuted,
            VerifyOutcome::Inconclusive => Self::Inconclusive,
        }
    }
}

/// One entry of the fix menu.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FixOption {
    /// What the option trades, in words ("no throttling", "best goodput within the budget").
    pub label: String,
    /// The change from the diagnosis run (empty: keep the current configuration).
    pub overrides: Overrides,
    /// The tuner trial it was measured in.
    pub trial_id: u64,
    /// Goodput in `goodput_unit` (`ops_per_s` or `mib_per_s`).
    #[serde(rename = "goodput_per_s")]
    pub goodput: f64,
    pub goodput_unit: String,
    /// Goodput change against the diagnosis (ratio).
    pub goodput_delta_ratio: f64,
    pub p99_ms: f64,
    pub throttled_request_ratio: f64,
    /// Cost per month at the profile's monthly operations (USD; absent without a price table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_month_usd: Option<f64>,
    /// Monthly cost change against the diagnosis (USD; retries and amplification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_delta_per_month_usd: Option<f64>,
    /// Within the throttle budget and the SLA.
    pub feasible: bool,
    pub recommended: bool,
    pub verify: OptionVerify,
}

impl FixOption {
    /// "0.00% throttling at −28% goodput (p99 12 ms, +$3/month)".
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{:.2}% throttling at {:+.0}% goodput (p99 {})",
            self.throttled_request_ratio * 100.0,
            self.goodput_delta_ratio * 100.0,
            crate::util::fmt_ms(self.p99_ms)
        );
        if let Some(d) = self.cost_delta_per_month_usd
            && d.abs() >= 0.01
        {
            s += &format!(
                ", {}{}/month",
                if d >= 0.0 { "+" } else { "−" },
                crate::util::fmt_usd(d.abs())
            );
        }
        s
    }
}

/// The inputs of [`menu`].
pub struct MenuInputs<'a> {
    pub trials: &'a [Trial],
    /// The diagnosis as measured (goodput, p99, throttling): what deltas are taken against.
    pub reference: &'a Trial,
    /// Goodput in MiB/s instead of ops/s.
    pub use_bytes: bool,
    pub max_throttle_ratio: f64,
    /// Operations per month (cost) and whether a price table applies.
    pub monthly_ops: Option<f64>,
    /// The change of a trial from the diagnosis' configuration.
    pub overrides_of: &'a dyn Fn(&Trial) -> Overrides,
}

fn goodput(t: &Trial, bytes: bool) -> f64 {
    if bytes { t.mib_per_s } else { t.ops_per_s }
}

/// The fix menu: 2–4 Pareto options, the recommended one first.
pub fn menu(m: &MenuInputs<'_>) -> Vec<FixOption> {
    let g = |t: &Trial| goodput(t, m.use_bytes);
    let cand: Vec<&Trial> = m
        .trials
        .iter()
        .filter(|t| t.stage != "confirm" && t.error_rate <= m.reference.error_rate.max(0.01))
        .collect();
    if cand.is_empty() {
        return Vec::new();
    }
    let dominated = |b: &Trial| {
        cand.iter().any(|a| {
            g(a) >= g(b)
                && a.throttled_request_ratio <= b.throttled_request_ratio
                && a.p99_ms <= b.p99_ms
                && (g(a) > g(b)
                    || a.throttled_request_ratio < b.throttled_request_ratio
                    || a.p99_ms < b.p99_ms)
        })
    };
    let front: Vec<&Trial> = cand.iter().copied().filter(|t| !dominated(t)).collect();
    let pool = if front.is_empty() {
        cand.clone()
    } else {
        front
    };
    let best_by = |f: &dyn Fn(&&Trial) -> bool, key: &dyn Fn(&Trial) -> f64| {
        pool.iter()
            .copied()
            .filter(|t| f(t))
            .max_by(|a, b| key(a).total_cmp(&key(b)))
    };
    let within = |t: &&Trial| t.throttled_request_ratio <= m.max_throttle_ratio && t.feasible;
    let mut picks: Vec<(&Trial, &str)> = Vec::new();
    fn add<'t>(
        t: Option<&'t Trial>,
        label: &'static str,
        picks: &mut Vec<(&'t Trial, &'static str)>,
    ) {
        if let Some(t) = t
            && !picks.iter().any(|(p, _)| p.id == t.id)
        {
            picks.push((t, label));
        }
    }
    let rec = best_by(&within, &|t| g(t));
    add(rec, "best goodput within the throttle budget", &mut picks);
    add(
        best_by(&|t: &&Trial| t.throttled_responses == 0, &|t| g(t)),
        "no throttling",
        &mut picks,
    );
    add(
        best_by(&|_| true, &|t| g(t)),
        "highest goodput (throttles)",
        &mut picks,
    );
    add(best_by(&|_| true, &|t| -t.p99_ms), "lowest p99", &mut picks);
    picks.truncate(4);
    let month = |t: &Trial| {
        m.monthly_ops
            .filter(|_| t.cost_per_million_ops > 0.0)
            .map(|ops| t.cost_per_million_ops * ops / 1e6)
    };
    let reference_feasible =
        m.reference.feasible && m.reference.throttled_request_ratio <= m.max_throttle_ratio;
    let mut out: Vec<FixOption> = picks
        .into_iter()
        .map(|(t, label)| {
            let cost = month(t);
            FixOption {
                label: label.to_owned(),
                overrides: (m.overrides_of)(t),
                trial_id: t.id as u64,
                goodput: g(t),
                goodput_unit: if m.use_bytes {
                    "mib_per_s"
                } else {
                    "ops_per_s"
                }
                .into(),
                goodput_delta_ratio: crate::util::ratio(g(t), g(m.reference)) - 1.0,
                p99_ms: t.p99_ms,
                throttled_request_ratio: t.throttled_request_ratio,
                cost_per_month_usd: cost,
                cost_delta_per_month_usd: cost.zip(month(m.reference)).map(|(a, b)| a - b),
                feasible: within(&t),
                recommended: false,
                verify: OptionVerify::NotVerified,
            }
        })
        .collect();
    // Never recommend a change predicted (measured) worse on the objective than a diagnosis
    // that already met the budget.
    let rec_idx = out
        .iter()
        .position(|o| o.feasible)
        .filter(|&i| !(reference_feasible && out[i].goodput < g(m.reference) * 0.97));
    match rec_idx {
        Some(i) => out[i].recommended = true,
        None if reference_feasible => {
            out.insert(
                0,
                FixOption {
                    label: "keep the current configuration".into(),
                    overrides: Overrides::new(),
                    trial_id: m.reference.id as u64,
                    goodput: g(m.reference),
                    goodput_unit: if m.use_bytes {
                        "mib_per_s"
                    } else {
                        "ops_per_s"
                    }
                    .into(),
                    goodput_delta_ratio: 0.0,
                    p99_ms: m.reference.p99_ms,
                    throttled_request_ratio: m.reference.throttled_request_ratio,
                    cost_per_month_usd: month(m.reference),
                    cost_delta_per_month_usd: month(m.reference).map(|_| 0.0),
                    feasible: true,
                    recommended: true,
                    verify: OptionVerify::NotVerified,
                },
            );
        }
        None => {}
    }
    out.sort_by_key(|o| !o.recommended);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(id: usize, ops: f64, thr: f64, p99: f64) -> Trial {
        Trial {
            id,
            stage: "concurrency".into(),
            ops_per_s: ops,
            p99_ms: p99,
            throttled_request_ratio: thr,
            throttled_responses: (thr * 10_000.0) as u64,
            requests: 10_000,
            cost_per_million_ops: 5.0,
            feasible: thr <= 0.005,
            ..Default::default()
        }
    }

    #[test]
    fn the_menu_offers_trades_and_recommends_the_best_within_budget() {
        let base = trial(0, 1000.0, 0.05, 80.0);
        let trials = vec![
            base.clone(),
            trial(1, 720.0, 0.0, 20.0),
            trial(2, 970.0, 0.004, 30.0),
            trial(3, 1010.0, 0.05, 90.0),
            trial(4, 600.0, 0.01, 60.0), // dominated
        ];
        let ov = |t: &Trial| Overrides::new().with_set("concurrency", &t.id.to_string());
        let m = menu(&MenuInputs {
            trials: &trials,
            reference: &base,
            use_bytes: false,
            max_throttle_ratio: 0.005,
            monthly_ops: Some(1e8),
            overrides_of: &ov,
        });
        assert!((2..=4).contains(&m.len()), "{m:#?}");
        assert!(m[0].recommended);
        assert_eq!(m[0].trial_id, 2);
        assert!(
            m.iter()
                .any(|o| o.trial_id == 1 && o.throttled_request_ratio == 0.0)
        );
        assert!(m.iter().all(|o| o.trial_id != 4));
        assert!(m[0].summary().contains("goodput"));
        assert_eq!(m[0].cost_per_month_usd, Some(500.0));
    }

    #[test]
    fn a_good_diagnosis_is_kept() {
        let base = trial(0, 1000.0, 0.0, 10.0);
        let trials = vec![base.clone(), trial(1, 800.0, 0.0, 9.0)];
        let ov = |_: &Trial| Overrides::new().with_set("concurrency", "4");
        let m = menu(&MenuInputs {
            trials: &trials,
            reference: &base,
            use_bytes: false,
            max_throttle_ratio: 0.001,
            monthly_ops: None,
            overrides_of: &ov,
        });
        let r = m.iter().find(|o| o.recommended).unwrap();
        assert!(r.overrides.is_empty() || r.goodput >= 1000.0, "{m:#?}");
    }
}
