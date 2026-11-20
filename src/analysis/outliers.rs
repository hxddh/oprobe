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
    /// `error`, `stage` (the dominant stage of the request), `size` (bytes moved, binned),
    /// `rtt` (TCP RTT, binned), `time` (tenth of the run), `conn` (connection id).
    pub attribute: String,
    pub value: String,
    /// Share of selected requests with this value (0–1).
    pub selected_ratio: f64,
    /// Share of the other requests with this value (0–1).
    pub rest_ratio: f64,
    /// `selected_ratio / rest_ratio`, smoothed (add-one) and capped.
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

/// Fewest selected requests a value needs before it is ranked at all.
const MIN_SUPPORT: u64 = 2;
/// Smallest share of the selection a value must explain (noise floor).
const MIN_SELECTED_RATIO: f64 = 0.05;
/// One-sided significance: z of the two-proportion test (≈ p < 0.05).
const MIN_Z: f64 = 1.65;

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

/// Log-scale size bin of the bytes a request moved.
pub fn size_bin(bytes: u64) -> &'static str {
    const KIB: u64 = 1024;
    match bytes {
        0 => "0B",
        b if b < 4 * KIB => "<4KiB",
        b if b < 64 * KIB => "4-64KiB",
        b if b < KIB * KIB => "64KiB-1MiB",
        b if b < 16 * KIB * KIB => "1-16MiB",
        _ => ">=16MiB",
    }
}

/// Bin of the kernel's smoothed RTT of the request's connection.
pub fn rtt_bin(rtt_us: u64) -> &'static str {
    match rtt_us {
        r if r < 1_000 => "<1ms",
        r if r < 10_000 => "1-10ms",
        r if r < 50_000 => "10-50ms",
        r if r < 200_000 => "50-200ms",
        _ => ">=200ms",
    }
}

/// Time bins over the run: ten equal slices of the observed request start times.
struct TimeBins {
    from_us: i64,
    width_us: i64,
}

impl TimeBins {
    fn new(reqs: &[ReqRow]) -> Self {
        let lo = reqs.iter().map(|r| r.t_us).min().unwrap_or(0).max(0);
        let hi = reqs.iter().map(|r| r.t_us).max().unwrap_or(0).max(lo);
        Self {
            from_us: lo,
            // Whole 100 ms steps keep the labels readable.
            width_us: (((hi - lo) / 10).max(1) + 99_999) / 100_000 * 100_000,
        }
    }
    fn label(&self, t_us: i64) -> String {
        let i = ((t_us.max(self.from_us) - self.from_us) / self.width_us).min(9);
        let a = (self.from_us + i * self.width_us) as f64 / 1e6;
        let b = a + self.width_us as f64 / 1e6;
        format!("{a:.1}-{b:.1}s")
    }
}

fn attributes(r: &ReqRow, bins: &TimeBins) -> [(&'static str, String); 12] {
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
        ("size", size_bin(r.bytes_up + r.bytes_down).to_owned()),
        (
            "rtt",
            r.tcp
                .as_ref()
                .filter(|t| t.rtt_us > 0)
                .map(|t| rtt_bin(t.rtt_us).to_owned())
                .unwrap_or_default(),
        ),
        ("time", bins.label(r.t_us)),
        (
            "conn",
            if r.conn_id > 0 {
                r.conn_id.to_string()
            } else {
                String::new()
            },
        ),
    ]
}

/// Rank attribute values by how over-represented they are among the selected requests.
///
/// The lift of each value is smoothed (add-one on both sides, so a value seen twice in a tiny
/// selection never gets an infinite lift); a value needs at least two selected requests and
/// 5 % of the selection, and its over-representation must be significant (two-proportion
/// z ≥ 1.65). Values are scored by `selected_ratio × ln(lift)` — distinctive *and* common —
/// damped while the evidence is weak (z < 4).
pub fn attribute(reqs: &[ReqRow], sel: &Selection) -> Attribution {
    let bins = TimeBins::new(reqs);
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
        for (k, v) in attributes(r, &bins) {
            if !v.is_empty() {
                *map.entry((k, v)).or_default() += 1;
            }
        }
    }
    let mut scored: Vec<(f64, Factor)> = selected
        .into_iter()
        .filter_map(|((attr, value), count)| {
            let s = count as f64 / n_sel.max(1) as f64;
            // Without a comparison set nothing can be over-represented.
            if count < MIN_SUPPORT || s < MIN_SELECTED_RATIO || n_rest == 0 {
                return None;
            }
            let rc = rest.get(&(attr, value.clone())).copied().unwrap_or(0);
            let r = rc as f64 / n_rest as f64;
            let s_smooth = (count as f64 + 1.0) / (n_sel as f64 + 2.0);
            let r_smooth = (rc as f64 + 1.0) / (n_rest as f64 + 2.0);
            let lift = (s_smooth / r_smooth).min(MAX_LIFT);
            let z = crate::analysis::stats::two_proportion_z(
                count as f64,
                n_sel as f64,
                rc as f64,
                n_rest as f64,
            );
            if lift <= 1.0 || z < MIN_Z {
                return None;
            }
            let score = s * lift.ln() * (z / 4.0).min(1.0);
            Some((
                score,
                Factor {
                    attribute: attr.to_owned(),
                    value,
                    selected_ratio: s,
                    rest_ratio: r,
                    lift,
                    selected_count: count,
                },
            ))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.attribute.cmp(&b.1.attribute))
            .then_with(|| a.1.value.cmp(&b.1.value))
    });
    let mut factors: Vec<Factor> = scored.into_iter().map(|(_, f)| f).collect();
    factors.truncate(12);
    Attribution {
        selected_count: n_sel,
        rest_count: n_rest,
        factors,
    }
}

/// A request row for tests (GetObject, 200, warm, 1 KiB down).
#[cfg(test)]
pub(crate) fn test_req(t_us: i64, total_us: u64) -> ReqRow {
    use std::borrow::Cow;
    ReqRow {
        t_us,
        op_id: 1,
        attempt: 1,
        api: Cow::Borrowed("GetObject"),
        method: Cow::Borrowed("GET"),
        status: 200,
        protocol: crate::model::WireProtocol::H1,
        reused: true,
        conn_id: 1,
        remote: Some("10.0.0.1:443".parse().unwrap()),
        queue_us: 0,
        dns_us: 0,
        tcp_us: 0,
        proxy_us: 0,
        tls_us: 0,
        send_us: 10,
        ttfb_us: total_us.saturating_sub(10),
        body_us: 0,
        stall_us: 0,
        total_us,
        bytes_up: 0,
        bytes_down: 1024,
        request_id: None,
        retry_after_ms: None,
        error_code: None,
        error: None,
        tls: None,
        tcp: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_remote_ranks_first_and_shared_values_are_ignored() {
        let mut reqs = Vec::new();
        for i in 0..2000 {
            let mut r = test_req(i * 1000, 5_000 + (i as u64 % 7) * 100);
            r.conn_id = (i % 16) as u64 + 1;
            if i % 50 == 0 {
                // One bad front-end: slow, and only there.
                r.remote = Some("10.0.0.9:443".parse().unwrap());
                r.total_us = 400_000;
                r.ttfb_us = 399_000;
            }
            reqs.push(r);
        }
        let a = attribute(&reqs, &Selection::slowest(&reqs, 2.0));
        assert_eq!(a.factors[0].attribute, "remote", "{a:#?}");
        assert_eq!(a.factors[0].value, "10.0.0.9");
        assert!(a.factors[0].lift > 10.0);
        // GetObject on both sides: never a factor.
        assert!(a.factors.iter().all(|f| f.attribute != "api"));
    }

    #[test]
    fn small_coincidences_are_not_significant() {
        let mut reqs: Vec<ReqRow> = (0..40)
            .map(|i| test_req(i * 1000, 1000 + i as u64))
            .collect();
        // Two of the four slowest are cold; so are 8 of the other 36.
        for (i, r) in reqs.iter_mut().enumerate() {
            r.reused = !(i == 39 || i == 38 || i < 8);
        }
        let a = attribute(&reqs, &Selection::slowest(&reqs, 10.0));
        assert!(
            a.factors.iter().all(|f| f.attribute != "connection"),
            "{a:#?}"
        );
    }

    #[test]
    fn numeric_attributes_are_binned() {
        let mut reqs = Vec::new();
        for i in 0..1000 {
            let mut r = test_req(i * 10_000, 2_000);
            if i % 10 == 0 {
                r.bytes_down = 32 << 20;
                r.total_us = 300_000;
            }
            reqs.push(r);
        }
        let a = attribute(&reqs, &Selection::slowest(&reqs, 5.0));
        assert!(
            a.factors
                .iter()
                .any(|f| f.attribute == "size" && f.value == ">=16MiB"),
            "{a:#?}"
        );
        assert_eq!(size_bin(100), "<4KiB");
        assert_eq!(rtt_bin(25_000), "10-50ms");
    }

    #[test]
    fn time_windows_are_attributed() {
        let mut reqs = Vec::new();
        for i in 0..1000i64 {
            // A slow patch in the 4th second of a 10 s run.
            let slow = (3_000_000..4_000_000).contains(&(i * 10_000));
            reqs.push(test_req(i * 10_000, if slow { 90_000 } else { 3_000 }));
        }
        let a = attribute(&reqs, &Selection::slowest(&reqs, 5.0));
        let t = a.factors.iter().find(|f| f.attribute == "time").unwrap();
        assert_eq!(t.value, "3.0-4.0s");
    }
}
