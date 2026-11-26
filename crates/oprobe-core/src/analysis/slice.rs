//! Re-slicing a run's request rows: group-by tables
//! (`--by api|conn|remote|stage|second|status`) and per-stage latency tables.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::outliers::Stage;
use crate::model::ReqRow;

/// How to group request rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum By {
    #[default]
    Api,
    /// Connection id.
    Conn,
    /// Remote IP.
    Remote,
    /// Dominant stage of each request.
    Stage,
    /// Second of the window the request started in.
    Second,
    Status,
}

impl By {
    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Conn => "conn",
            Self::Remote => "remote",
            Self::Stage => "stage",
            Self::Second => "second",
            Self::Status => "status",
        }
    }
    fn key(self, r: &ReqRow) -> String {
        match self {
            Self::Api => r.api.label().to_owned(),
            // A request that never got a connection (reset, refused) has none to group by.
            Self::Conn if r.conn_id == 0 => NO_CONNECTION.into(),
            Self::Conn => r.conn_id.to_string(),
            Self::Remote => r
                .remote
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| NO_CONNECTION.into()),
            Self::Stage => Stage::dominant(r).name().to_owned(),
            Self::Second => (r.t_us.div_euclid(1_000_000)).to_string(),
            Self::Status => r.status.to_string(),
        }
    }
}

/// The group of the requests that had no connection (reset or refused before one was
/// reported): `--by conn` and the remotes table count them under this row.
pub const NO_CONNECTION: &str = "no connection";

/// One group of a `--by` table.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GroupRow {
    pub key: String,
    pub requests_count: u64,
    pub errors_count: u64,
    pub throttled_count: u64,
    /// Requests that opened a new connection.
    pub cold_count: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    #[serde(rename = "down_bytes")]
    pub bytes_down: u64,
    #[serde(rename = "up_bytes")]
    pub bytes_up: u64,
    /// Share of the filtered requests' total time spent in this group (0–100).
    pub time_pct: f64,
}

/// Group `reqs` by `by` (seconds in time order; other keys by request count, descending).
pub fn group(reqs: &[&ReqRow], by: By) -> Vec<GroupRow> {
    group_weighted(reqs, by, &std::collections::HashMap::new())
}

/// [`group`] weighted by per-operation sampling weights (`op_id -> weight`, defaulting to 1).
pub fn group_weighted(
    reqs: &[&ReqRow],
    by: By,
    weights: &std::collections::HashMap<u64, f64>,
) -> Vec<GroupRow> {
    use crate::analysis::stats::weighted_quantile_us_ms;
    let w = |r: &ReqRow| weights.get(&r.op_id).copied().unwrap_or(1.0);
    let mut m: BTreeMap<String, Vec<&ReqRow>> = BTreeMap::new();
    for r in reqs {
        m.entry(by.key(r)).or_default().push(r);
    }
    let all: f64 = reqs.iter().map(|r| r.total_us as f64 * w(r)).sum();
    let mut rows: Vec<GroupRow> = m
        .into_iter()
        .map(|(key, v)| {
            let mut t: Vec<(u64, f64)> = v.iter().map(|r| (r.total_us, w(r))).collect();
            let n_w: f64 = v.iter().map(|r| w(r)).sum();
            let sum_w: f64 = v.iter().map(|r| r.total_us as f64 * w(r)).sum();
            let is_err = |r: &ReqRow| {
                r.is_error() && r.status != 404 && !r.detail.as_ref().is_some_and(|d| d.cut)
            };
            let err_w: f64 = v.iter().filter(|r| is_err(r)).map(|r| w(r)).sum();
            let thr_w: f64 = v.iter().filter(|r| r.is_throttle()).map(|r| w(r)).sum();
            let cold_w: f64 = v
                .iter()
                .filter(|r| !r.reused && r.connect_us() > 0)
                .map(|r| w(r))
                .sum();
            let down_w: f64 = v.iter().map(|r| r.bytes_down as f64 * w(r)).sum();
            let up_w: f64 = v.iter().map(|r| r.bytes_up as f64 * w(r)).sum();
            GroupRow {
                key,
                requests_count: n_w.round() as u64,
                errors_count: err_w.round() as u64,
                throttled_count: thr_w.round() as u64,
                cold_count: cold_w.round() as u64,
                mean_ms: sum_w / n_w.max(1.0) / 1000.0,
                p50_ms: weighted_quantile_us_ms(&mut t, 0.5),
                p90_ms: weighted_quantile_us_ms(&mut t, 0.9),
                p99_ms: weighted_quantile_us_ms(&mut t, 0.99),
                max_ms: v.iter().map(|r| r.total_us).max().unwrap_or(0) as f64 / 1000.0,
                bytes_down: down_w.round() as u64,
                bytes_up: up_w.round() as u64,
                time_pct: if all > 0.0 { sum_w * 100.0 / all } else { 0.0 },
            }
        })
        .collect();
    if by == By::Second {
        rows.sort_by_key(|r| r.key.parse::<i64>().unwrap_or(0));
    } else {
        rows.sort_by(|a, b| {
            b.requests_count
                .cmp(&a.requests_count)
                .then(a.key.cmp(&b.key))
        });
    }
    rows
}

/// p50 / p99 of one request stage (ms).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageQ {
    pub p50_ms: f64,
    pub p99_ms: f64,
}

/// Per-stage latency of one API's requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageRow {
    /// API name, or `all`.
    pub api: String,
    pub requests_count: u64,
    pub queue: StageQ,
    /// Requests that dialed only.
    pub connect: StageQ,
    pub send: StageQ,
    pub ttfb: StageQ,
    pub body: StageQ,
    pub stall: StageQ,
    pub total: StageQ,
    /// The most HTTP/2 streams open on a connection when one of these requests started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streams_max_count: Option<u64>,
    /// From the request start until the server acknowledged the last request byte (kernel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_acked: Option<StageQ>,
    /// Until the request body reached the kernel's send buffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_buffered: Option<StageQ>,
    /// CDN cache status (`hit`, `miss`, …) → requests.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cache_status_count: BTreeMap<String, u64>,
    /// The bucket's region (`x-amz-bucket-region`) → requests.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub region_count: BTreeMap<String, u64>,
}

impl StageRow {
    /// Any request carried a signal beyond the stage timings.
    pub fn has_signals(&self) -> bool {
        self.streams_max_count.is_some()
            || self.upload_acked.is_some()
            || self.send_buffered.is_some()
            || !self.cache_status_count.is_empty()
            || !self.region_count.is_empty()
    }
}

fn stage_row(
    api: String,
    reqs: &[&ReqRow],
    weights: &std::collections::HashMap<u64, f64>,
) -> StageRow {
    use crate::analysis::stats::weighted_quantile_us_ms;
    let w = |r: &ReqRow| weights.get(&r.op_id).copied().unwrap_or(1.0);
    let q = |f: &dyn Fn(&ReqRow) -> Option<u64>| {
        let mut v: Vec<(u64, f64)> = reqs.iter().filter_map(|r| Some((f(r)?, w(r)))).collect();
        StageQ {
            p50_ms: weighted_quantile_us_ms(&mut v, 0.5),
            p99_ms: weighted_quantile_us_ms(&mut v, 0.99),
        }
    };
    // Over the requests that carry the signal only (`None` when none does).
    let some =
        |f: &dyn Fn(&ReqRow) -> Option<u64>| reqs.iter().any(|r| f(r).is_some()).then(|| q(f));
    let count = |f: &dyn Fn(&ReqRow) -> Option<String>| {
        let mut m: BTreeMap<String, f64> = BTreeMap::new();
        for r in reqs {
            if let Some(k) = f(r) {
                *m.entry(k).or_default() += w(r);
            }
        }
        m.into_iter().map(|(k, c)| (k, c.round() as u64)).collect()
    };
    fn d(r: &ReqRow) -> Option<&crate::model::ReqDetail> {
        r.detail.as_deref()
    }
    let n_w: f64 = reqs.iter().map(|r| w(r)).sum();
    StageRow {
        streams_max_count: reqs
            .iter()
            .filter_map(|r| d(r).and_then(|d| d.streams_count))
            .max()
            .map(u64::from),
        upload_acked: some(&|r| d(r).and_then(|d| d.upload_acked_us)),
        send_buffered: some(&|r| d(r).and_then(|d| d.send_buffered_us)),
        cache_status_count: count(&|r| d(r).and_then(|d| d.cache_status.clone())),
        region_count: count(&|r| d(r).and_then(|d| d.region.clone())),
        api,
        requests_count: n_w.round() as u64,
        queue: q(&|r| Some(r.queue_us)),
        connect: q(&|r| (!r.reused && r.connect_us() > 0).then(|| r.connect_us())),
        send: q(&|r| Some(r.send_us)),
        ttfb: q(&|r| Some(r.ttfb_us)),
        body: q(&|r| Some(r.body_us)),
        stall: q(&|r| Some(r.stall_us)),
        total: q(&|r| Some(r.total_us)),
    }
}

/// Per-stage p50/p99 per API (busiest first) and for all requests (last).
pub fn stage_table(reqs: &[&ReqRow]) -> Vec<StageRow> {
    stage_table_weighted(reqs, &std::collections::HashMap::new())
}

/// [`stage_table`] weighted by per-operation sampling weights (`op_id -> weight`, defaulting to 1).
pub fn stage_table_weighted(
    reqs: &[&ReqRow],
    weights: &std::collections::HashMap<u64, f64>,
) -> Vec<StageRow> {
    let mut m: BTreeMap<&str, Vec<&ReqRow>> = BTreeMap::new();
    for r in reqs {
        m.entry(r.api.label()).or_default().push(r);
    }
    let mut rows: Vec<StageRow> = m
        .into_iter()
        .map(|(api, v)| stage_row(api.to_owned(), &v, weights))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.requests_count));
    if rows.len() > 1 {
        rows.push(stage_row("all".into(), reqs, weights));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::outliers::test_req;

    #[test]
    fn the_stage_table_summarises_the_request_signals() {
        let mut reqs: Vec<ReqRow> = (0..10).map(|i| test_req(i * 1_000, 1_000)).collect();
        assert!(!stage_table(&reqs.iter().collect::<Vec<_>>())[0].has_signals());
        for (i, r) in reqs.iter_mut().enumerate().take(4) {
            r.detail = Some(Box::new(crate::model::ReqDetail {
                send_buffered_us: Some(2_000),
                upload_acked_us: Some(if i == 0 { 9_000 } else { 1_000 }),
                streams_count: Some(i as u32 + 1),
                cache_status: Some(if i == 0 { "miss" } else { "hit" }.into()),
                region: Some("eu-west-1".into()),
                ..Default::default()
            }));
        }
        let t = stage_table(&reqs.iter().collect::<Vec<_>>());
        let s = &t[0];
        assert!(s.has_signals());
        assert_eq!(s.upload_acked.unwrap().p50_ms, 1.0);
        assert_eq!(s.upload_acked.unwrap().p99_ms, 9.0);
        assert_eq!(s.send_buffered.unwrap().p50_ms, 2.0);
        assert_eq!(s.streams_max_count, Some(4));
        assert_eq!(s.cache_status_count["hit"], 3);
        assert_eq!(s.region_count["eu-west-1"], 4);
    }

    #[test]
    fn groups() {
        let mut reqs = Vec::new();
        for i in 0..100i64 {
            let mut r = test_req(i * 50_000, 1000 + i as u64 * 10);
            if i % 10 == 0 {
                r.status = 503;
                r.api = crate::trace::Api::PutObject;
            }
            reqs.push(r);
        }
        let g = group(&reqs.iter().collect::<Vec<_>>(), By::Status);
        assert_eq!(g[0].key, "200");
        assert_eq!(g[1].throttled_count, 10);
        let s = group(&reqs.iter().collect::<Vec<_>>(), By::Second);
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].key, "0");
        let t = stage_table(&reqs.iter().collect::<Vec<_>>());
        assert_eq!(t.len(), 3);
        assert_eq!(t.last().unwrap().api, "all");
    }

    /// K2-3: `--by stage` groups a request that spent its time receiving its body under
    /// `body`.
    #[test]
    fn the_body_stage_is_named_body() {
        let mut reqs: Vec<ReqRow> = (0..10).map(|i| test_req(i * 1_000, 1_000)).collect();
        for r in reqs.iter_mut().take(4) {
            (r.ttfb_us, r.body_us) = (100, 890);
        }
        let g = group(&reqs.iter().collect::<Vec<_>>(), By::Stage);
        let keys: Vec<(&str, u64)> = g
            .iter()
            .map(|r| (r.key.as_str(), r.requests_count))
            .collect();
        assert_eq!(keys, [("ttfb", 6), ("body", 4)]);
    }
}
