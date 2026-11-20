//! Outlier attribution: which request attributes are over-represented among the slow (or
//! selected) requests compared with the rest — "p99 requests are 6× more likely to be on cold
//! connections to 52.x.x.x". Shared by `oprobe analyze`, findings, MCP and the GUI brush.
//!
//! The types and the entry points are the stable interface (docs/design/v3.md §12); the ranking
//! itself may be refined.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::ReqRow;

/// Which requests are "selected" (everything else is the comparison set).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Selection {
    /// Window start, µs from the measurement window start (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_us: Option<i64>,
    /// Window end, µs (exclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_us: Option<i64>,
    /// Requests at least this slow (total), µs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_total_us: Option<u64>,
    /// Restrict to one storage API (e.g. `GetObject`); applies to both sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

impl Selection {
    /// The slowest `pct` percent of requests (by total time), e.g. `1.0` for p99+.
    pub fn slowest(reqs: &[ReqRow], pct: f64) -> Self {
        let mut t: Vec<u64> = reqs.iter().map(|r| r.total_us).collect();
        if t.is_empty() {
            return Self::default();
        }
        t.sort_unstable();
        let idx = (((100.0 - pct.clamp(0.0, 100.0)) / 100.0) * (t.len() - 1) as f64).round();
        Self {
            min_total_us: Some(t[idx as usize]),
            ..Self::default()
        }
    }

    fn in_scope(&self, r: &ReqRow) -> bool {
        self.api.as_deref().is_none_or(|a| r.api == a)
    }

    fn selects(&self, r: &ReqRow) -> bool {
        self.from_us.is_none_or(|f| r.t_us >= f)
            && self.to_us.is_none_or(|t| r.t_us < t)
            && self.min_total_us.is_none_or(|m| r.total_us >= m)
    }
}

/// One attribute value and how much more common it is among the selected requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Factor {
    /// Attribute name: `api`, `status`, `protocol`, `connection`, `remote`, `attempt`,
    /// `error`, `stage` (the dominant stage of the request).
    pub attribute: String,
    pub value: String,
    /// Share of selected requests with this value (0–1).
    pub selected_ratio: f64,
    /// Share of the other requests with this value (0–1).
    pub rest_ratio: f64,
    /// `selected_ratio / rest_ratio` (capped; large when the value is absent from the rest).
    pub lift: f64,
    /// Selected requests with this value.
    pub selected_count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Attribution {
    pub selected_count: u64,
    pub rest_count: u64,
    /// Most distinctive first.
    pub factors: Vec<Factor>,
}

const MAX_LIFT: f64 = 100.0;

/// The stage that dominated a request's time.
pub fn dominant_stage(r: &ReqRow) -> &'static str {
    let connect = r.dns_us + r.tcp_us + r.proxy_us + r.tls_us;
    [
        ("queue", r.queue_us),
        ("connect", connect),
        ("send", r.send_us),
        ("ttfb", r.ttfb_us),
        ("transfer", r.body_us),
        ("stall", r.stall_us),
    ]
    .into_iter()
    .max_by_key(|(_, v)| *v)
    .map(|(n, _)| n)
    .unwrap_or("ttfb")
}

fn attributes(r: &ReqRow) -> [(&'static str, String); 8] {
    [
        ("api", r.api.to_string()),
        ("status", r.status.to_string()),
        ("protocol", format!("{:?}", r.protocol).to_lowercase()),
        (
            "connection",
            if r.reused { "warm" } else { "cold" }.to_owned(),
        ),
        (
            "remote",
            r.remote.map(|a| a.ip().to_string()).unwrap_or_default(),
        ),
        (
            "attempt",
            if r.attempt > 1 { "retry" } else { "first" }.to_owned(),
        ),
        (
            "error",
            r.error_code
                .map(|c| c.as_str().to_owned())
                .unwrap_or_else(|| "none".into()),
        ),
        ("stage", dominant_stage(r).to_owned()),
    ]
}

/// Rank attribute values by how over-represented they are among the selected requests.
/// Values seen in fewer than 2 selected requests or under 5 % of them are ignored.
pub fn attribute(reqs: &[ReqRow], sel: &Selection) -> Attribution {
    let mut selected: HashMap<(&'static str, String), u64> = HashMap::new();
    let mut rest: HashMap<(&'static str, String), u64> = HashMap::new();
    let (mut n_sel, mut n_rest) = (0u64, 0u64);
    for r in reqs.iter().filter(|r| sel.in_scope(r)) {
        let (map, n) = if sel.selects(r) {
            (&mut selected, &mut n_sel)
        } else {
            (&mut rest, &mut n_rest)
        };
        *n += 1;
        for (k, v) in attributes(r) {
            if !v.is_empty() {
                *map.entry((k, v)).or_default() += 1;
            }
        }
    }
    let mut factors: Vec<Factor> = selected
        .into_iter()
        .filter_map(|((attr, value), count)| {
            let s = count as f64 / n_sel.max(1) as f64;
            if count < 2 || s < 0.05 {
                return None;
            }
            let rc = rest.get(&(attr, value.clone())).copied().unwrap_or(0);
            let r = rc as f64 / n_rest.max(1) as f64;
            let lift = if r > 0.0 {
                (s / r).min(MAX_LIFT)
            } else {
                MAX_LIFT
            };
            (lift > 1.0).then(|| Factor {
                attribute: attr.to_owned(),
                value,
                selected_ratio: s,
                rest_ratio: r,
                lift,
                selected_count: count,
            })
        })
        .collect();
    // Distinctive *and* common: lift weighted by how much of the selection it explains.
    factors.sort_by(|a, b| {
        let ka = a.lift.ln() * a.selected_ratio;
        let kb = b.lift.ln() * b.selected_ratio;
        kb.total_cmp(&ka)
    });
    factors.truncate(12);
    Attribution {
        selected_count: n_sel,
        rest_count: n_rest,
        factors,
    }
}
