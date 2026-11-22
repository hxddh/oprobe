//! Competing limit models (design v5 §4): `capacity.hypotheses`.
//!
//! Stream ANA fits five limit models side by side (`capacity::fit`) and selects one by
//! penalised likelihood. Until then the foundation maps the 4.x estimate onto this shape: the
//! single in-flight ceiling becomes one [`HypothesisKind::InflightLimit`] hypothesis (selected),
//! and a run without a ceiling has [`HypothesisKind::None`]. Advice is unchanged.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::capacity::{Capacity, Confidence};

/// The kind of limit a hypothesis asserts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisKind {
    /// The service accepts a number of requests in flight (`ceiling`).
    InflightLimit,
    /// A token bucket: requests per second (`rps`, `burst`).
    RateLimit,
    /// A per-key or per-prefix rate (`key`, `rps`).
    KeyLimit,
    /// Latency grows with in-flight requests (`service_ms`, `servers`).
    Queueing,
    /// Aggregate bytes per second flat across streams (`bytes_per_s`).
    BandwidthCap,
    /// The client itself is saturated (CPU, scheduling lag).
    ClientBound,
    /// The limit changes over the run (change points).
    TimeVarying,
    /// No limit was reached.
    #[default]
    None,
}

impl HypothesisKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::InflightLimit => "in-flight limit",
            Self::RateLimit => "rate limit",
            Self::KeyLimit => "per-key limit",
            Self::Queueing => "queueing",
            Self::BandwidthCap => "bandwidth cap",
            Self::ClientBound => "client-bound",
            Self::TimeVarying => "time-varying limit",
            Self::None => "no limit",
        }
    }
}

/// The parameters of a limit model (the ones its kind uses are set).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HypothesisParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_inflight_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_count: Option<u64>,
    /// The key or prefix of a [`HypothesisKind::KeyLimit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_s: Option<f64>,
}

/// One fitted limit model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hypothesis {
    pub kind: HypothesisKind,
    pub params: HypothesisParams,
    /// Log-likelihood of the data under the model (absent until fitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_likelihood: Option<f64>,
    /// Why the model fits, in words.
    #[serde(default)]
    pub evidence: Vec<String>,
    pub confidence: Confidence,
    /// Share of the throttled (or slowed) requests the model explains.
    pub explains_ratio: f64,
    /// The chosen hypothesis (at most one).
    pub selected: bool,
}

/// The 4.x estimate as hypotheses: its in-flight ceiling (selected), else "no limit".
pub fn from_capacity(c: &Capacity) -> Vec<Hypothesis> {
    match &c.inflight_ceiling {
        Some(ceil) => {
            let mut evidence = vec![format!(
                "throttling starts between {} and {} requests in flight",
                ceil.lo, ceil.hi
            )];
            if !c.confidence_reason.is_empty() {
                evidence.push(c.confidence_reason.clone());
            }
            vec![Hypothesis {
                kind: HypothesisKind::InflightLimit,
                params: HypothesisParams {
                    ceiling_inflight_count: Some(ceil.requests.round().max(0.0) as u64),
                    requests_per_s: c.rate_ceiling_requests_per_s,
                    ..Default::default()
                },
                log_likelihood: None,
                evidence,
                confidence: c.confidence,
                explains_ratio: if c.observed.throttled_requests_count > 0 {
                    1.0
                } else {
                    0.0
                },
                selected: true,
            }]
        }
        None => vec![Hypothesis {
            kind: HypothesisKind::None,
            evidence: vec!["no limit was reached in this run".into()],
            confidence: c.confidence,
            selected: true,
            ..Default::default()
        }],
    }
}

/// The selected hypothesis, if any.
pub fn selected(hs: &[Hypothesis]) -> Option<&Hypothesis> {
    hs.iter().find(|h| h.selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::capacity::Ceiling;

    #[test]
    fn the_ceiling_becomes_one_selected_inflight_hypothesis() {
        let mut c = Capacity::default();
        let h = from_capacity(&c);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, HypothesisKind::None);
        c.inflight_ceiling = Some(Ceiling {
            requests: 24.0,
            lo: 24.0,
            hi: 25.0,
        });
        c.observed.throttled_requests_count = 3;
        let h = from_capacity(&c);
        let s = selected(&h).unwrap();
        assert_eq!(s.kind, HypothesisKind::InflightLimit);
        assert_eq!(s.params.ceiling_inflight_count, Some(24));
        assert_eq!(s.explains_ratio, 1.0);
    }
}
