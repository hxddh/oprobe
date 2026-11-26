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
        self.api.as_deref().is_none_or(|a| {
            r.api.label().eq_ignore_ascii_case(a) || r.api_name().eq_ignore_ascii_case(a)
        })
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
    #[serde(rename = "lift_ratio")]
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

/// A stage of one backend request, in the order a request goes through them
/// (docs/concepts.md). Every consumer names a stage through this type: the findings, the
/// attribution, `show --by stage` and the tail split.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Waiting for an in-flight permit or a connection.
    Queue,
    /// DNS, TCP, proxy CONNECT and TLS of a new connection.
    Connect,
    /// Sending the request (its body included).
    Send,
    /// Waiting for the first byte of the response.
    Ttfb,
    /// Receiving the response body.
    Body,
    /// The body waiting for the caller to consume it.
    Stall,
}

impl Stage {
    pub const ALL: [Stage; 6] = [
        Stage::Queue,
        Stage::Connect,
        Stage::Send,
        Stage::Ttfb,
        Stage::Body,
        Stage::Stall,
    ];

    /// The stage's one name: a `--by stage` group, a `stage` attribution value, a tail-split
    /// key. It is the name of the stage's field in the report (`stages.body`).
    pub fn name(self) -> &'static str {
        match self {
            Stage::Queue => "queue",
            Stage::Connect => "connect",
            Stage::Send => "send",
            Stage::Ttfb => "ttfb",
            Stage::Body => "body",
            Stage::Stall => "stall",
        }
    }

    /// The stage `name` names. `transfer` is what 9.2 and earlier called `body` in
    /// `--by stage` and in the attribution; it is still understood.
    pub fn parse(name: &str) -> Option<Stage> {
        let name = name.trim().to_ascii_lowercase();
        if name == "transfer" {
            return Some(Stage::Body);
        }
        Stage::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The time `r` spent in this stage (µs).
    pub fn us(self, r: &ReqRow) -> u64 {
        match self {
            Stage::Queue => r.queue_us,
            Stage::Connect => r.connect_us(),
            Stage::Send => r.send_us,
            Stage::Ttfb => r.ttfb_us,
            Stage::Body => r.body_us,
            Stage::Stall => r.stall_us,
        }
    }

    /// The stage that dominated a request's time (the later stage of equals; the time to
    /// first byte for a request that recorded no stage).
    pub fn dominant(r: &ReqRow) -> Stage {
        Stage::ALL
            .into_iter()
            .filter(|s| s.us(r) > 0)
            .max_by_key(|s| s.us(r))
            .unwrap_or(Stage::Ttfb)
    }
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

/// The kernel's smoothed RTT of the requests' connections measures the server's response
/// delay as well as the network (AN-11): a delayed answer delays the ACK the RTT is sampled
/// from. It is no network signal when it tracks the server: on loopback (no network path),
/// when it is far above the TCP handshake's round trip (a clean RTT sample), or when it rises
/// and falls with the time to first byte (rank correlation ≥ 0.5).
pub fn rtt_tracks_server(reqs: &[ReqRow]) -> bool {
    let with: Vec<&ReqRow> = reqs
        .iter()
        .filter(|r| r.tcp.as_ref().is_some_and(|t| t.rtt_us > 0))
        .collect();
    if with.len() < 20 {
        return false;
    }
    if with
        .iter()
        .all(|r| r.remote.is_some_and(|a| a.ip().is_loopback()))
    {
        return true;
    }
    let mut rtt: Vec<u64> = with
        .iter()
        .filter_map(|r| r.tcp.as_ref().map(|t| t.rtt_us))
        .collect();
    let mut hs: Vec<u64> = reqs
        .iter()
        .filter(|r| !r.reused && r.tcp_us > 0)
        .map(|r| r.tcp_us)
        .collect();
    if hs.len() >= 5 {
        let (k, h) = (
            crate::analysis::stats::quantile_us_ms(&mut rtt, 0.5),
            crate::analysis::stats::quantile_us_ms(&mut hs, 0.5),
        );
        if k > 3.0 * h + 1.0 {
            return true;
        }
    }
    let pairs: Vec<(f64, f64)> = with
        .iter()
        .map(|r| {
            (
                r.tcp.as_ref().map_or(0, |t| t.rtt_us) as f64,
                r.ttfb_us as f64,
            )
        })
        .collect();
    rank_correlation(&pairs) >= 0.5
}

/// Spearman's rank correlation of `pairs` (0 when undefined).
fn rank_correlation(pairs: &[(f64, f64)]) -> f64 {
    fn ranks(v: &[f64]) -> Vec<f64> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|a, b| v[*a].total_cmp(&v[*b]));
        let mut r = vec![0.0; v.len()];
        let mut i = 0;
        while i < idx.len() {
            let mut j = i;
            while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
                j += 1;
            }
            let avg = (i + j) as f64 / 2.0;
            for k in &idx[i..=j] {
                r[*k] = avg;
            }
            i = j + 1;
        }
        r
    }
    if pairs.len() < 3 {
        return 0.0;
    }
    let a = ranks(&pairs.iter().map(|p| p.0).collect::<Vec<_>>());
    let b = ranks(&pairs.iter().map(|p| p.1).collect::<Vec<_>>());
    let (ma, mb) = (
        crate::analysis::stats::mean(&a),
        crate::analysis::stats::mean(&b),
    );
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(&b) {
        num += (x - ma) * (y - mb);
        da += (x - ma).powi(2);
        db += (y - mb).powi(2);
    }
    if da <= 0.0 || db <= 0.0 {
        0.0
    } else {
        num / (da * db).sqrt()
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

fn attributes(r: &ReqRow, bins: &TimeBins, rtt: bool) -> [(&'static str, String); 12] {
    [
        ("api", r.api.label().to_owned()),
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
        ("stage", Stage::dominant(r).name().to_owned()),
        ("size", size_bin(r.bytes_up + r.bytes_down).to_owned()),
        (
            "rtt",
            r.tcp
                .as_ref()
                .filter(|t| rtt && t.rtt_us > 0)
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
/// z ≥ 1.65; 3 for the many-valued `remote`, `conn` and `time`; a time selection never
/// ranks `time`). Values are scored by `selected_ratio × ln(lift)` — distinctive *and* common —
/// damped while the evidence is weak (z < 4).
pub fn attribute(reqs: &[ReqRow], sel: &Selection) -> Attribution {
    attribute_weighted(reqs, sel, &HashMap::new())
}

/// [`attribute`] weighted by per-operation sampling weights (`op_id -> weight`, defaulting to 1).
pub fn attribute_weighted(
    reqs: &[ReqRow],
    sel: &Selection,
    weights: &HashMap<u64, f64>,
) -> Attribution {
    let bins = TimeBins::new(reqs);
    // An RTT inflated by the server's own delay would "explain" every slow request.
    let rtt = !rtt_tracks_server(reqs);
    let by_time = sel.from_us.is_some() || sel.to_us.is_some();
    let mut selected: HashMap<(&'static str, String), (u64, f64)> = HashMap::new();
    let mut rest: HashMap<(&'static str, String), f64> = HashMap::new();
    let (mut raw_sel, mut raw_rest) = (0u64, 0u64);
    let (mut w_sel, mut w_rest) = (0.0f64, 0.0f64);
    for r in reqs.iter().filter(|r| sel.in_scope(r)) {
        let w = weights.get(&r.op_id).copied().unwrap_or(1.0);
        if sel.selects(r) {
            raw_sel += 1;
            w_sel += w;
            for (k, v) in attributes(r, &bins, rtt) {
                if !v.is_empty() && !(k == "time" && by_time) {
                    let e = selected.entry((k, v)).or_default();
                    e.0 += 1;
                    e.1 += w;
                }
            }
        } else {
            raw_rest += 1;
            w_rest += w;
            for (k, v) in attributes(r, &bins, rtt) {
                if !v.is_empty() && !(k == "time" && by_time) {
                    *rest.entry((k, v)).or_default() += w;
                }
            }
        }
    }
    let scale = if w_sel > 0.0 {
        raw_sel as f64 / w_sel
    } else {
        1.0
    };
    let rest_scale = if w_rest > 0.0 {
        raw_rest as f64 / w_rest
    } else {
        1.0
    };
    let mut scored: Vec<(f64, Factor)> = selected
        .into_iter()
        .filter_map(|((attr, value), (raw_count, w_count))| {
            let s = w_count / w_sel.max(1.0);
            // Without a comparison set nothing can be over-represented.
            if raw_count < MIN_SUPPORT || s < MIN_SELECTED_RATIO || raw_rest == 0 {
                return None;
            }
            let w_rc = rest.get(&(attr, value.clone())).copied().unwrap_or(0.0);
            let r = w_rc / w_rest.max(1.0);
            let eff_sel_hit = w_count * scale;
            let eff_rest_hit = w_rc * rest_scale;
            let s_smooth = (eff_sel_hit + 1.0) / (raw_sel as f64 + 2.0);
            let r_smooth = (eff_rest_hit + 1.0) / (raw_rest as f64 + 2.0);
            let lift = (s_smooth / r_smooth).min(MAX_LIFT);
            let z = crate::analysis::stats::two_proportion_z(
                eff_sel_hit,
                raw_sel as f64,
                eff_rest_hit,
                raw_rest as f64,
            );
            // Many-valued attributes get a stricter bar (many chances for a coincidence).
            let min_z = if matches!(attr, "conn" | "time" | "remote") {
                3.0
            } else {
                MIN_Z
            };
            if lift <= 1.0 || z < min_z {
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
                    selected_count: w_count.round() as u64,
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
        selected_count: w_sel.round() as u64,
        rest_count: w_rest.round() as u64,
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
        api: crate::trace::Api::GetObject,
        service_op: None,
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
        detail: None,
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

    #[test]
    fn an_rtt_that_tracks_server_delay_is_not_a_factor() {
        // Loopback, and the kernel RTT rises with the server's delay (delayed ACKs).
        let mut reqs = Vec::new();
        for i in 0..1000 {
            let slow = i % 25 == 0;
            let mut r = test_req(i * 1000, if slow { 250_000 } else { 8_000 });
            r.remote = Some("127.0.0.1:9000".parse().unwrap());
            r.tcp = Some(Box::new(crate::model::TcpRow {
                rtt_us: if slow { 30_000 } else { 200 },
                ..Default::default()
            }));
            reqs.push(r);
        }
        assert!(rtt_tracks_server(&reqs));
        let a = attribute(&reqs, &Selection::slowest(&reqs, 2.0));
        assert!(a.factors.iter().all(|f| f.attribute != "rtt"), "{a:#?}");
        // Off loopback the correlation with the first byte gives it away too.
        for r in &mut reqs {
            r.remote = Some("10.0.0.1:443".parse().unwrap());
        }
        assert!(rtt_tracks_server(&reqs));
        // A genuinely slow path to one remote, independent of the server: kept.
        for (i, r) in reqs.iter_mut().enumerate() {
            r.tcp.as_mut().unwrap().rtt_us = if i % 2 == 0 { 40_000 } else { 400 };
        }
        assert!(!rtt_tracks_server(&reqs));
        assert!(rank_correlation(&[(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)]) > 0.99);
    }

    /// K2-3: one name per stage for every consumer. 9.2 called the body stage `transfer` here
    /// and `body` in the findings, the tail split and the report's fields.
    #[test]
    fn a_stage_has_one_name_and_the_old_one_is_still_understood() {
        let names: Vec<&str> = Stage::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["queue", "connect", "send", "ttfb", "body", "stall"]);
        for s in Stage::ALL {
            assert_eq!(Stage::parse(s.name()), Some(s));
        }
        assert_eq!(Stage::parse("transfer"), Some(Stage::Body));
        assert_eq!(Stage::parse(" TTFB "), Some(Stage::Ttfb));
        assert_eq!(Stage::parse("server"), None);
        let mut r = test_req(0, 9_000);
        assert_eq!(Stage::dominant(&r), Stage::Ttfb);
        (r.ttfb_us, r.body_us) = (1_000, 7_990);
        assert_eq!(Stage::dominant(&r), Stage::Body);
        assert_eq!(Stage::Body.us(&r), 7_990);
        (r.tcp_us, r.tls_us) = (5_000, 4_000);
        assert_eq!(Stage::dominant(&r), Stage::Connect);
        // A request that recorded no stage waited for its answer.
        let mut none = test_req(0, 0);
        (none.send_us, none.ttfb_us) = (0, 0);
        assert_eq!(Stage::dominant(&none), Stage::Ttfb);
    }
}
