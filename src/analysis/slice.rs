//! Re-slicing a run's request rows (`oprobe analyze`): filters (`--window`, `--where`),
//! group-by tables (`--by api|conn|remote|stage|second|status`) and per-stage latency tables.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::outliers::dominant_stage;
use super::stats::quantile_us_ms;
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
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "api" => Self::Api,
            "conn" | "connection" => Self::Conn,
            "remote" | "ip" => Self::Remote,
            "stage" => Self::Stage,
            "second" | "time" | "t" => Self::Second,
            "status" => Self::Status,
            _ => return None,
        })
    }
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
            Self::Api => r.api.to_string(),
            Self::Conn => r.conn_id.to_string(),
            Self::Remote => r
                .remote
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "-".into()),
            Self::Stage => dominant_stage(r).to_owned(),
            Self::Second => (r.t_us.div_euclid(1_000_000)).to_string(),
            Self::Status => r.status.to_string(),
        }
    }
}

/// Request filters: `--window A..B` and `--where k=v,…`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Filter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_s: Option<f64>,
    /// `attribute = value` conditions (all must hold): `api`, `status`, `protocol`, `remote`,
    /// `conn`, `stage`, `connection` (`cold`/`warm`), `attempt` (`first`/`retry`), `method`,
    /// `op` (operation id).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<(String, String)>,
}

/// Attributes `--where` accepts.
pub const WHERE_KEYS: [&str; 10] = [
    "api",
    "status",
    "protocol",
    "remote",
    "conn",
    "stage",
    "connection",
    "attempt",
    "method",
    "op",
];

impl Filter {
    /// Parse `--window A..B` (seconds; either side may be empty).
    pub fn parse_window(&mut self, s: &str) -> Result<(), String> {
        let (a, b) = s.split_once("..").ok_or_else(|| {
            format!("invalid window `{s}` (expected A..B in seconds, e.g. 5..10)")
        })?;
        let num = |x: &str| -> Result<Option<f64>, String> {
            let x = x.trim().trim_end_matches('s');
            if x.is_empty() {
                Ok(None)
            } else {
                x.parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("invalid window bound `{x}`"))
            }
        };
        self.from_s = num(a)?;
        self.to_s = num(b)?;
        if let (Some(a), Some(b)) = (self.from_s, self.to_s)
            && b <= a
        {
            return Err(format!("empty window `{s}`"));
        }
        Ok(())
    }

    /// Parse `--where k=v[,k=v…]`.
    pub fn parse_where(&mut self, s: &str) -> Result<(), String> {
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("invalid condition `{part}` (expected key=value)"))?;
            let k = k.trim().to_ascii_lowercase();
            if !WHERE_KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "unknown attribute `{k}` (one of: {})",
                    WHERE_KEYS.join(", ")
                ));
            }
            self.conditions.push((k, v.trim().to_owned()));
        }
        Ok(())
    }

    pub fn matches(&self, r: &ReqRow) -> bool {
        let t = r.t_us as f64 / 1e6;
        if self.from_s.is_some_and(|a| t < a) || self.to_s.is_some_and(|b| t >= b) {
            return false;
        }
        self.conditions.iter().all(|(k, v)| {
            let eq = |x: &str| x.eq_ignore_ascii_case(v);
            match k.as_str() {
                "api" => eq(&r.api),
                "status" => eq(&r.status.to_string()),
                "protocol" => eq(&format!("{:?}", r.protocol)),
                "remote" => r
                    .remote
                    .is_some_and(|a| eq(&a.ip().to_string()) || eq(&a.to_string())),
                "conn" => eq(&r.conn_id.to_string()),
                "stage" => eq(dominant_stage(r)),
                "connection" => eq(if r.reused { "warm" } else { "cold" }),
                "attempt" => {
                    eq(if r.attempt > 1 { "retry" } else { "first" }) || eq(&r.attempt.to_string())
                }
                "method" => eq(&r.method),
                "op" => eq(&r.op_id.to_string()),
                _ => false,
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.from_s.is_none() && self.to_s.is_none() && self.conditions.is_empty()
    }

    pub fn apply<'a>(&self, reqs: &'a [ReqRow]) -> Vec<&'a ReqRow> {
        reqs.iter().filter(|r| self.matches(r)).collect()
    }
}

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
    pub bytes_down: u64,
    pub bytes_up: u64,
    /// Share of the filtered requests' total time spent in this group (0–100).
    pub time_pct: f64,
}

/// Group `reqs` by `by` (seconds in time order; other keys by request count, descending).
pub fn group(reqs: &[&ReqRow], by: By) -> Vec<GroupRow> {
    let mut m: BTreeMap<String, Vec<&ReqRow>> = BTreeMap::new();
    for r in reqs {
        m.entry(by.key(r)).or_default().push(r);
    }
    let all: u64 = reqs.iter().map(|r| r.total_us).sum();
    let mut rows: Vec<GroupRow> = m
        .into_iter()
        .map(|(key, v)| {
            let mut t: Vec<u64> = v.iter().map(|r| r.total_us).collect();
            let sum: u64 = t.iter().sum();
            GroupRow {
                key,
                requests_count: v.len() as u64,
                errors_count: v.iter().filter(|r| r.is_error()).count() as u64,
                throttled_count: v.iter().filter(|r| r.is_throttle()).count() as u64,
                cold_count: v.iter().filter(|r| !r.reused && r.connect_us() > 0).count() as u64,
                mean_ms: sum as f64 / v.len().max(1) as f64 / 1000.0,
                p50_ms: quantile_us_ms(&mut t, 0.5),
                p90_ms: quantile_us_ms(&mut t, 0.9),
                p99_ms: quantile_us_ms(&mut t, 0.99),
                max_ms: t.iter().max().copied().unwrap_or(0) as f64 / 1000.0,
                bytes_down: v.iter().map(|r| r.bytes_down).sum(),
                bytes_up: v.iter().map(|r| r.bytes_up).sum(),
                time_pct: if all > 0 {
                    sum as f64 * 100.0 / all as f64
                } else {
                    0.0
                },
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
}

fn stage_row(api: String, reqs: &[&ReqRow]) -> StageRow {
    let q = |f: &dyn Fn(&ReqRow) -> Option<u64>| {
        let mut v: Vec<u64> = reqs.iter().filter_map(|r| f(r)).collect();
        StageQ {
            p50_ms: quantile_us_ms(&mut v, 0.5),
            p99_ms: quantile_us_ms(&mut v, 0.99),
        }
    };
    StageRow {
        api,
        requests_count: reqs.len() as u64,
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
    let mut m: BTreeMap<&str, Vec<&ReqRow>> = BTreeMap::new();
    for r in reqs {
        m.entry(&r.api).or_default().push(r);
    }
    let mut rows: Vec<StageRow> = m
        .into_iter()
        .map(|(api, v)| stage_row(api.to_owned(), &v))
        .collect();
    rows.sort_by(|a, b| b.requests_count.cmp(&a.requests_count));
    if rows.len() > 1 {
        rows.push(stage_row("all".into(), reqs));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::outliers::test_req;

    #[test]
    fn filters_and_groups() {
        let mut reqs = Vec::new();
        for i in 0..100i64 {
            let mut r = test_req(i * 50_000, 1000 + i as u64 * 10);
            if i % 10 == 0 {
                r.status = 503;
                r.api = "PutObject".into();
            }
            reqs.push(r);
        }
        let mut f = Filter::default();
        f.parse_window("1..3").unwrap();
        f.parse_where("api=GetObject").unwrap();
        let v = f.apply(&reqs);
        assert_eq!(v.len(), 36);
        assert!(f.parse_where("bogus=1").is_err());
        assert!(Filter::default().parse_window("5..1").is_err());
        let g = group(&reqs.iter().collect::<Vec<_>>(), By::Status);
        assert_eq!(g[0].key, "200");
        assert_eq!(g[1].throttled_count, 10);
        let s = group(&reqs.iter().collect::<Vec<_>>(), By::Second);
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].key, "0");
        let t = stage_table(&reqs.iter().collect::<Vec<_>>());
        assert_eq!(t.len(), 3);
        assert_eq!(t.last().unwrap().api, "all");
        let mut w = Filter::default();
        w.parse_where("status=503").unwrap();
        assert_eq!(w.apply(&reqs).len(), 10);
    }
}
