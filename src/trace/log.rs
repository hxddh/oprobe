//! The in-memory trace log (design §5): one [`OpRow`] per measured operation and one [`ReqRow`]
//! per backend request, capped at `trace.max_requests` rows each with reservoir sampling beyond
//! the cap. Aggregates (the recorder's histograms) are always exact; the rows feed the run
//! file, the heatmap, per-remote splits and per-stage p99.
//!
//! Rows are built by moving the transport's [`RequestRecord`]s (no per-request string
//! allocation: API and method names are static).

use std::borrow::Cow;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use super::model::RequestRecord;
use crate::errors::ErrorCode;
use crate::model::{OpBreakdownUs, OpRow, ReqRow, TraceInfo};

/// Default cap of kept request (and operation) rows.
pub const DEFAULT_MAX_REQUESTS: usize = 1_000_000;

/// Rows taken out of a [`TraceLog`] at the end of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceRows {
    pub ops: Vec<OpRow>,
    pub reqs: Vec<ReqRow>,
    pub info: TraceInfo,
}

impl TraceRows {
    /// Merge shards (rows re-sorted by start time).
    pub fn merge(parts: Vec<TraceRows>) -> TraceRows {
        let mut out = TraceRows::default();
        for p in parts {
            out.ops.extend(p.ops);
            out.reqs.extend(p.reqs);
            let i = &mut out.info;
            i.max_requests_count += p.info.max_requests_count;
            i.ops_seen_count += p.info.ops_seen_count;
            i.ops_kept_count += p.info.ops_kept_count;
            i.requests_seen_count += p.info.requests_seen_count;
            i.requests_kept_count += p.info.requests_kept_count;
        }
        out.ops.sort_by_key(|o| (o.t_us, o.op_id));
        out.reqs.sort_by_key(|r| (r.t_us, r.op_id));
        let i = &mut out.info;
        i.sample_ratio = if i.requests_seen_count == 0 {
            1.0
        } else {
            i.requests_kept_count as f64 / i.requests_seen_count as f64
        };
        out
    }

    pub fn sampled(&self) -> bool {
        self.info.ops_seen_count > self.info.ops_kept_count
            || self.info.requests_seen_count > self.info.requests_kept_count
    }
}

/// Reservoir-sampled row store.
#[derive(Debug)]
pub struct TraceLog {
    max: usize,
    ops: Vec<OpRow>,
    reqs: Vec<ReqRow>,
    ops_seen: u64,
    reqs_seen: u64,
    rng: SmallRng,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_REQUESTS)
    }
}

/// Algorithm R: keep a uniform sample of `cap` items out of `seen`.
fn reservoir<T>(v: &mut Vec<T>, seen: &mut u64, cap: usize, item: T, rng: &mut SmallRng) {
    *seen += 1;
    if v.len() < cap {
        // Grow in large steps: no reallocation storm on the hot path.
        if v.len() == v.capacity() {
            v.reserve((v.len().max(1024)).min(cap - v.len()));
        }
        v.push(item);
    } else if cap > 0 {
        let j = rng.gen_range(0..*seen);
        if (j as usize) < cap {
            v[j as usize] = item;
        }
    }
}

impl TraceLog {
    pub fn new(max_requests: usize) -> Self {
        Self {
            max: max_requests,
            ops: Vec::new(),
            reqs: Vec::new(),
            ops_seen: 0,
            reqs_seen: 0,
            rng: SmallRng::seed_from_u64(0x0b5e_7ace),
        }
    }

    pub fn max_requests(&self) -> usize {
        self.max
    }

    /// Change the cap (takes effect for rows recorded afterwards).
    pub fn set_max_requests(&mut self, max: usize) {
        self.max = max;
    }

    /// Drop every row (a new measurement window starts).
    pub fn reset(&mut self) {
        let max = self.max;
        *self = Self::new(max);
    }

    pub fn push_op(&mut self, row: OpRow) {
        reservoir(
            &mut self.ops,
            &mut self.ops_seen,
            self.max,
            row,
            &mut self.rng,
        );
    }

    pub fn push_req(&mut self, row: ReqRow) {
        reservoir(
            &mut self.reqs,
            &mut self.reqs_seen,
            self.max,
            row,
            &mut self.rng,
        );
    }

    pub fn len(&self) -> (usize, usize) {
        (self.ops.len(), self.reqs.len())
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.reqs.is_empty()
    }

    /// Take the rows (sorted by start time) and reset the log.
    pub fn take(&mut self) -> TraceRows {
        let mut ops = std::mem::take(&mut self.ops);
        let mut reqs = std::mem::take(&mut self.reqs);
        ops.sort_by_key(|o| (o.t_us, o.op_id));
        reqs.sort_by_key(|r| (r.t_us, r.op_id));
        let info = TraceInfo {
            max_requests_count: self.max as u64,
            ops_seen_count: self.ops_seen,
            ops_kept_count: ops.len() as u64,
            requests_seen_count: self.reqs_seen,
            requests_kept_count: reqs.len() as u64,
            sample_ratio: if self.reqs_seen == 0 {
                1.0
            } else {
                reqs.len() as f64 / self.reqs_seen as f64
            },
        };
        self.reset();
        TraceRows { ops, reqs, info }
    }
}

/// Static method names (no allocation per row).
fn method_name(m: String) -> Cow<'static, str> {
    Cow::Borrowed(match m.as_str() {
        "GET" => "GET",
        "PUT" => "PUT",
        "POST" => "POST",
        "HEAD" => "HEAD",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "OPTIONS" => "OPTIONS",
        _ => return Cow::Owned(m),
    })
}

/// Build a request row from a transport record. `op_t_us` is the start of the owning
/// operation relative to the window start.
pub fn req_row(r: RequestRecord, op_id: u64, op_t_us: i64, attempt: u32) -> ReqRow {
    let t = r.timing;
    let error_code = if r.is_error() && r.status != 404 || r.error.is_some() {
        ErrorCode::from_status(r.status).or(Some(ErrorCode::Internal))
    } else {
        None
    };
    ReqRow {
        t_us: op_t_us + r.start_offset_us as i64,
        op_id,
        attempt,
        api: Cow::Borrowed(r.service_op.unwrap_or_else(|| r.api.label())),
        method: method_name(r.method),
        status: r.status,
        protocol: r.protocol.into(),
        reused: r.reused,
        conn_id: r.conn_id,
        remote: r.remote,
        queue_us: t.queue_us,
        dns_us: t.dns_us,
        tcp_us: t.tcp_us,
        proxy_us: t.proxy_us,
        tls_us: t.tls_us,
        send_us: t.send_us,
        ttfb_us: t.ttfb_us,
        body_us: t.body_us,
        stall_us: t.stall_us,
        total_us: t.total_us,
        bytes_up: r.bytes_up,
        bytes_down: r.bytes_down,
        request_id: r.request_id,
        retry_after_ms: r.retry_after_ms,
        error_code,
        error: r.error,
        tls: r.tls,
        tcp: r.tcp,
    }
}

/// Attempt number of each request (1 = first try of that API call in the operation):
/// a request is a retry when an earlier-started request of the same API failed.
/// `None` when nothing failed (the common case, no allocation): every request is attempt 1.
pub fn attempts(reqs: &[RequestRecord]) -> Option<Vec<u32>> {
    if !reqs.iter().any(|r| r.is_error() && r.status != 404) {
        return None;
    }
    let mut out = vec![1u32; reqs.len()];
    let mut order: Vec<usize> = (0..reqs.len()).collect();
    order.sort_by_key(|&i| reqs[i].start_offset_us);
    let mut failures = [0u32; 16];
    for i in order {
        let slot = (reqs[i].api as usize).min(15);
        out[i] = failures[slot] + 1;
        if reqs[i].is_error() && reqs[i].status != 404 {
            failures[slot] += 1;
        }
    }
    Some(out)
}

/// Build an operation row.
#[allow(clippy::too_many_arguments)]
pub fn op_row(
    op_id: u64,
    verb: &'static str,
    key: String,
    t_us: i64,
    latency_us: u64,
    bytes: u64,
    error: Option<String>,
    throttled: bool,
    retries: u64,
    breakdown: [u64; 5],
    requests: u64,
) -> OpRow {
    OpRow {
        op_id,
        verb: Cow::Borrowed(verb),
        key,
        t_us,
        latency_us,
        bytes,
        ok: error.is_none(),
        error_code: error.as_deref().map(ErrorCode::from_class),
        error,
        throttled,
        attempts: (retries as u32).saturating_add(1),
        breakdown: OpBreakdownUs {
            queue_us: breakdown[0],
            cache_us: breakdown[1],
            backend_us: breakdown[2],
            retry_us: breakdown[3],
            client_us: breakdown[4],
        },
        requests: requests.min(u32::MAX as u64) as u32,
        await_tree: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Api, Protocol, RequestTiming};

    fn op(id: u64) -> OpRow {
        op_row(
            id,
            "stat",
            String::new(),
            id as i64,
            10,
            0,
            None,
            false,
            0,
            [0; 5],
            1,
        )
    }

    #[test]
    fn keeps_everything_under_the_cap() {
        let mut l = TraceLog::new(100);
        for i in 0..50 {
            l.push_op(op(i));
        }
        let rows = l.take();
        assert_eq!(rows.ops.len(), 50);
        assert!(!rows.sampled());
        assert_eq!(rows.info.sample_ratio, 1.0);
        assert!(l.is_empty());
    }

    #[test]
    fn samples_uniformly_beyond_the_cap() {
        let mut l = TraceLog::new(1000);
        for i in 0..100_000 {
            l.push_op(op(i));
        }
        let rows = l.take();
        assert_eq!(rows.ops.len(), 1000);
        assert_eq!(rows.info.ops_seen_count, 100_000);
        assert!(rows.sampled());
        // A uniform sample: about half of the kept rows come from the second half.
        let late = rows.ops.iter().filter(|o| o.op_id >= 50_000).count();
        assert!((350..=650).contains(&late), "{late}");
        // Sorted by start.
        assert!(rows.ops.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    }

    fn rec(api: Api, start: u64, status: u16) -> RequestRecord {
        RequestRecord {
            api,
            method: "GET".into(),
            status,
            protocol: Protocol::Http1,
            reused: true,
            conn_id: 1,
            start_offset_us: start,
            timing: RequestTiming {
                total_us: 5,
                ..Default::default()
            },
            bytes_up: 0,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: None,
            remote: None,
            service_op: None,
            tls: None,
            tcp: None,
        }
    }

    #[test]
    fn retries_get_increasing_attempt_numbers() {
        let reqs = vec![
            rec(Api::GetObject, 300, 200),
            rec(Api::GetObject, 0, 503),
            rec(Api::HeadObject, 50, 200),
            rec(Api::GetObject, 100, 503),
        ];
        assert_eq!(attempts(&reqs), Some(vec![3, 1, 1, 2]));
        assert_eq!(attempts(&reqs[2..3]), None);
        let row = req_row(reqs[1].clone(), 7, 1000, 1);
        assert_eq!(row.t_us, 1000);
        assert_eq!(row.api, "GetObject");
        assert_eq!(row.error_code, Some(ErrorCode::StorageThrottled));
        assert!(matches!(row.method, Cow::Borrowed("GET")));
    }
}
