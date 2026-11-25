//! The in-memory trace log (design §5, v5 §2): one [`OpRow`] per measured operation and one
//! [`ReqRow`] per backend request, within a row budget (`trace.max_requests`); beyond it whole
//! operations are sampled ([`crate::sample`]). Aggregates (the recorder's histograms) are always
//! exact; the rows feed the run file, the heatmap, per-remote splits and per-stage p99.
//!
//! Rows are built by moving the transport's [`RequestRecord`]s (no per-request string
//! allocation: API and method names are static).

use std::borrow::Cow;

use crate::record::RequestRecord;
use crate::rows::{FailureCode, OpBreakdownUs, OpRow, ReqRow, TraceInfo};
use crate::sample::{OpSampler, SampleConfig, SamplingStats};

/// Default cap of kept trace rows.
pub const DEFAULT_MAX_REQUESTS: usize = 1_000_000;

/// Rows taken out of a [`TraceLog`] at the end of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceRows {
    pub ops: Vec<OpRow>,
    pub reqs: Vec<ReqRow>,
    pub info: TraceInfo,
    /// What the whole-operation sampler kept (`report.sampling`).
    pub sampling: SamplingStats,
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
            out.sampling.add(&p.sampling);
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

/// The row store of one recorder shard: whole operations sampled by an [`OpSampler`]
/// (design v5 §2) within a row budget.
#[derive(Debug)]
pub struct TraceLog {
    max: usize,
    sampler: OpSampler,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_REQUESTS)
    }
}

impl TraceLog {
    /// A log keeping at most `max_requests` rows (operations plus requests).
    pub fn new(max_requests: usize) -> Self {
        Self {
            max: max_requests,
            sampler: OpSampler::new(SampleConfig {
                max_rows: max_requests,
                ..SampleConfig::default()
            }),
        }
    }

    pub fn max_requests(&self) -> usize {
        self.max
    }

    /// Change the cap (takes effect for rows recorded afterwards).
    pub fn set_max_requests(&mut self, max: usize) {
        self.max = max;
        self.sampler.set_max_rows(max);
    }

    /// Drop every row (a new measurement window starts).
    pub fn reset(&mut self) {
        let max = self.max;
        *self = Self::new(max);
    }

    /// One operation with all its requests.
    pub fn push_whole_op(&mut self, op: OpRow, reqs: Vec<ReqRow>) {
        self.sampler.offer(op, reqs);
    }

    /// An operation row without request rows.
    pub fn push_op(&mut self, row: OpRow) {
        self.sampler.offer(row, Vec::new());
    }

    /// A request issued outside any operation.
    pub fn push_unscoped(&mut self, row: ReqRow) {
        self.sampler.offer_unscoped(row);
    }

    /// Rows kept now.
    pub fn len(&self) -> usize {
        self.sampler.kept_rows()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the rows (sorted by start time) and reset the log.
    pub fn take(&mut self) -> TraceRows {
        let (mut ops, mut reqs, sampling) = self.sampler.take();
        ops.sort_by_key(|o| (o.t_us, o.op_id));
        reqs.sort_by_key(|r| (r.t_us, r.op_id));
        let info = TraceInfo {
            max_requests_count: self.max as u64,
            ops_seen_count: sampling.seen_ops,
            ops_kept_count: ops.len() as u64,
            requests_seen_count: sampling.seen_requests,
            requests_kept_count: reqs.len() as u64,
            sample_ratio: if sampling.seen_requests == 0 {
                1.0
            } else {
                reqs.len() as f64 / sampling.seen_requests as f64
            },
        };
        self.reset();
        TraceRows {
            ops,
            reqs,
            info,
            sampling,
        }
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
pub fn req_row(mut r: RequestRecord, op_id: u64, op_t_us: i64, attempt: u32) -> ReqRow {
    let t = r.timing;
    let error_code = if r.is_error() && r.status != 404 || r.error.is_some() {
        // Temporary or persistent (design v7 B6), unless the backend said (in-process).
        let status = r.status;
        let d = r.detail.get_or_insert_with(Default::default);
        d.temporary
            .get_or_insert(FailureCode::temporary_status(status));
        FailureCode::of_request(status, d.error_code.as_deref()).or(Some(FailureCode::Internal))
    } else {
        None
    };
    ReqRow {
        t_us: op_t_us + r.start_offset_us as i64,
        op_id,
        attempt,
        api: r.api,
        service_op: r.service_op.map(Cow::Borrowed),
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
        detail: r.detail,
    }
}

/// Attempt number of each request (1 = first try of that API call in the operation):
/// a request is a retry when an earlier-ended failed request (or an overlapping `TailCutLayer`
/// cut attempt) of the same API has not yet been resolved by a succeeding retry. Once a retry
/// succeeds, later parts/pages/ranges of the same API start fresh at attempt 1.
/// `None` when nothing failed (the common case, no allocation): every request is attempt 1.
pub fn attempts(reqs: &[RequestRecord]) -> Option<Vec<u32>> {
    if !reqs.iter().any(|r| {
        (r.is_error() && r.status != 404) || r.detail.as_deref().is_some_and(|d| d.cut)
    }) {
        return None;
    }
    let mut out = vec![1u32; reqs.len()];
    let mut order: Vec<usize> = (0..reqs.len()).collect();
    order.sort_by_key(|&i| reqs[i].start_offset_us);
    // Per API slot: `(attempt, available_from_offset_us)` of unresolved failed/cut chains.
    let mut pending: [Vec<(u32, u64)>; 16] = std::array::from_fn(|_| Vec::new());
    for i in order {
        let r = &reqs[i];
        let slot = (r.api as usize).min(15);
        let start = r.start_offset_us;
        let att = if let Some(pos) = pending[slot]
            .iter()
            .position(|&(_, ready_at)| start >= ready_at)
        {
            let (prev_att, _) = pending[slot].remove(pos);
            prev_att + 1
        } else {
            1
        };
        out[i] = att;
        let is_cut = r.detail.as_deref().is_some_and(|d| d.cut);
        if is_cut {
            // A TailCutLayer replacement starts while the cut request is still in flight.
            pending[slot].push((att, start));
        } else if r.is_error() && r.status != 404 {
            // A RetryLayer retry starts only after this failed attempt ended.
            pending[slot].push((att, start.saturating_add(r.timing.total_us)));
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
        error_code: error.as_deref().map(FailureCode::from_class),
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
        weight: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use crate::rows::FailureCode;

    fn failed(status: u16, provider: Option<&str>) -> ReqRow {
        let mut r = rec(Api::PutObject, 0, status);
        if let Some(p) = provider {
            r.detail = Some(Box::new(crate::rows::ReqDetail {
                error_code: Some(p.into()),
                ..Default::default()
            }));
        }
        req_row(r, 1, 0, 1)
    }

    /// 409, 412 and 416 keep their meaning (design v7 B6), with a temporary/persistent bit.
    #[test]
    fn failed_requests_keep_their_class() {
        let code = |s, p| failed(s, p).error_code.unwrap();
        let temporary = |s| failed(s, None).detail.unwrap().temporary;
        assert_eq!(code(412, None), FailureCode::StorageConditionNotMatch);
        assert_eq!(code(409, None), FailureCode::StorageConflict);
        assert_eq!(
            code(409, Some("BlobAlreadyExists")),
            FailureCode::StorageAlreadyExists
        );
        assert_eq!(code(416, None), FailureCode::StorageRangeNotSatisfied);
        assert_eq!(code(400, None), FailureCode::Internal);
        assert_eq!(code(503, None), FailureCode::StorageThrottled);
        assert_eq!(temporary(503), Some(true));
        assert_eq!(temporary(500), Some(true));
        assert_eq!(temporary(0), Some(true));
        assert_eq!(temporary(412), Some(false));
        assert_eq!(temporary(400), Some(false));
        // A success carries neither.
        let ok = req_row(rec(Api::GetObject, 0, 200), 1, 0, 1);
        assert!(ok.error_code.is_none() && ok.detail.is_none());
        // The codes serialize to stable strings.
        assert_eq!(
            serde_json::to_value(FailureCode::StorageConditionNotMatch).unwrap(),
            "storage.condition_not_match"
        );
        assert_eq!(FailureCode::from_class("already_exists"), FailureCode::StorageAlreadyExists);
    }

    use super::*;
    use crate::{Api, Protocol, RequestTiming};

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
    fn samples_whole_ops_across_time_beyond_the_cap() {
        let mut l = TraceLog::new(1000);
        for i in 0..100_000 {
            // 100 s of operations, 1000 per second.
            let mut o = op(i);
            o.t_us = i as i64 * 1_000;
            l.push_op(o);
        }
        let rows = l.take();
        assert!(
            rows.ops.len() <= 1000 && rows.ops.len() > 500,
            "{}",
            rows.ops.len()
        );
        assert_eq!(rows.info.ops_seen_count, 100_000);
        assert_eq!(rows.sampling.seen_ops, 100_000);
        assert!(rows.sampled());
        // Time-stratified: about half of the kept rows come from the second half.
        let late = rows.ops.iter().filter(|o| o.op_id >= 50_000).count();
        let n = rows.ops.len();
        assert!(late * 10 >= n * 3 && late * 10 <= n * 7, "{late} of {n}");
        // Sorted by start.
        assert!(rows.ops.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    }

    fn rec(api: Api, start: u64, status: u16) -> RequestRecord {
        RequestRecord {
            operation: None,
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
            detail: None,
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
        // Subsequent parts/pages after a recovered retry start fresh at attempt 1.
        let multipart = vec![
            rec(Api::UploadPart, 0, 503),
            rec(Api::UploadPart, 2, 200),   // concurrent part #2 (started before #1 ended at 5)
            rec(Api::UploadPart, 100, 200), // retry of part #1
            rec(Api::UploadPart, 200, 200), // part #3 starts fresh at attempt 1
        ];
        assert_eq!(attempts(&multipart), Some(vec![1, 1, 2, 1]));
        let row = req_row(reqs[1].clone(), 7, 1000, 1);
        assert_eq!(row.t_us, 1000);
        assert_eq!(row.api, Api::GetObject);
        assert_eq!(row.api_name(), "GetObject");
        assert_eq!(row.error_code, Some(FailureCode::StorageThrottled));
        assert!(matches!(row.method, Cow::Borrowed("GET")));
    }
}
