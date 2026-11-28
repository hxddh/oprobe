//! Thread-safe aggregation of trace records into histograms, timelines and slow-op samples.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use hdrhistogram::Histogram;

use crate::log::{TraceLog, TraceRows, attempts, op_row, req_row};
use crate::record::{Api, Protocol, RequestRecord};
use crate::scope::OpScopeData;
use crate::snapshot::*;
use crate::util::FxHashMap as HashMap;

const HIST_MAX_US: u64 = 3_600_000_000;
const SLOW_OPS_KEPT: usize = 8;

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_US, 3).expect("valid histogram bounds")
}

fn rec(h: &mut Histogram<u64>, us: u64) {
    let _ = h.record(us.min(HIST_MAX_US));
}

/// Whether request `r` dialed its connection: a connection counts as new through the request
/// that used it first.
fn dialed(r: &RequestRecord) -> bool {
    !r.reused && r.protocol != Protocol::InProcess && (r.status != 0 || r.timing.connect_us() > 0)
}

pub fn quantiles(h: &Histogram<u64>) -> Quantiles {
    if h.is_empty() {
        return Quantiles::default();
    }
    let ms = |v: u64| v as f64 / 1000.0;
    Quantiles {
        count: h.len(),
        mean: h.mean() / 1000.0,
        p50: ms(h.value_at_quantile(0.50)),
        p90: ms(h.value_at_quantile(0.90)),
        p99: ms(h.value_at_quantile(0.99)),
        p999: ms(h.value_at_quantile(0.999)),
        max: ms(h.max()),
    }
}

struct StageHists {
    queue: Histogram<u64>,
    connect: Histogram<u64>,
    send: Histogram<u64>,
    ttfb: Histogram<u64>,
    body: Histogram<u64>,
    stall: Histogram<u64>,
    total: Histogram<u64>,
}

impl StageHists {
    fn new() -> Self {
        Self {
            queue: hist(),
            connect: hist(),
            send: hist(),
            ttfb: hist(),
            body: hist(),
            stall: hist(),
            total: hist(),
        }
    }
}

struct ApiAgg {
    count: u64,
    connected: u64,
    errors: u64,
    throttled: u64,
    reused: u64,
    status: BTreeMap<u16, u64>,
    bytes_up: u64,
    bytes_down: u64,
    stages: StageHists,
}

impl ApiAgg {
    fn new() -> Self {
        Self {
            count: 0,
            connected: 0,
            errors: 0,
            throttled: 0,
            reused: 0,
            status: BTreeMap::new(),
            bytes_up: 0,
            bytes_down: 0,
            stages: StageHists::new(),
        }
    }
}

struct VerbAgg {
    latency: Histogram<u64>,
    count: u64,
    errors: u64,
    throttled: u64,
    bytes: u64,
    requests: u64,
    retries: u64,
    queue_us: u64,
    cache_us: u64,
    backend_us: u64,
    retry_us: u64,
    client_us: u64,
}

impl VerbAgg {
    fn new() -> Self {
        Self {
            latency: hist(),
            count: 0,
            errors: 0,
            throttled: 0,
            bytes: 0,
            requests: 0,
            retries: 0,
            queue_us: 0,
            cache_us: 0,
            backend_us: 0,
            retry_us: 0,
            client_us: 0,
        }
    }
}

/// Fixed log-scale latency bounds of the per-interval heatmap columns (ms; the last bucket is
/// open-ended). The same bounds as the report's heatmap, so runs compare.
pub const HEAT_BOUNDS_MS: [f64; 20] = [
    0.0, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0,
    5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0,
];

/// The heatmap row of a latency.
pub fn heat_index(latency_us: u64) -> usize {
    let ms = latency_us as f64 / 1000.0;
    HEAT_BOUNDS_MS.iter().rposition(|b| ms >= *b).unwrap_or(0)
}

/// Interval resolution (design v5 §3, M11): one-second intervals for the first ten minutes,
/// ten-second intervals afterwards, so an hour-long run keeps about 900 intervals.
const FINE_INTERVALS: usize = 600;
const COARSE_S: usize = 10;

/// The interval holding second `secs` of the window.
fn bucket_index(secs: u64) -> usize {
    let s = secs as usize;
    if s < FINE_INTERVALS {
        s
    } else {
        FINE_INTERVALS + (s - FINE_INTERVALS) / COARSE_S
    }
}

/// The interval of an event `us` after the window start. Once the window closed (`frozen` is
/// its length), events of draining operations land in its last interval: the timeline never
/// grows past the window.
fn event_bucket(us: u64, frozen: u64) -> usize {
    let us = if frozen == WINDOW_OPEN {
        us
    } else {
        us.min(frozen.saturating_sub(1))
    };
    bucket_index(us / 1_000_000)
}

/// The window closes a hair after a whole interval (`-d 5s` closes at 5.00008 s): the last
/// interval then holds only the events of operations draining after the close, and no
/// operation, and would draw as a drop to zero. Such a sliver (under 5 % of its length inside
/// the window, with no operation started in it) is folded into the interval before it; its
/// rates are over the previous interval's length. So is every interval that starts after the
/// close (operations still draining a second or more later).
fn fold_end_sliver(mut t: Vec<TimelinePoint>, elapsed_s: f64) -> Vec<TimelinePoint> {
    while t.len() >= 2 {
        let last = &t[t.len() - 1];
        let covered = elapsed_s - last.t_s;
        if last.ops_count > 0 || covered >= 0.05 * last.len_s.max(1e-9) {
            break;
        }
        t = fold_last(t);
    }
    t
}

/// The last interval folded into the one before it.
fn fold_last(mut t: Vec<TimelinePoint>) -> Vec<TimelinePoint> {
    let s = t.pop().expect("two intervals");
    let p = t.last_mut().expect("one interval");
    let width = p.len_s.max(1e-9);
    p.errors += s.errors;
    p.throttled += s.throttled;
    p.throttled_ops += s.throttled_ops;
    p.requests_count += s.requests_count;
    p.requests_per_s = p.requests_count as f64 / width;
    p.retries_count += s.retries_count;
    p.missed_arrivals += s.missed_arrivals;
    p.new_connections += s.new_connections;
    p.inflight_peak = p.inflight_peak.max(s.inflight_peak);
    for v in s.verbs {
        match p.verbs.iter_mut().find(|x| x.verb == v.verb) {
            Some(x) => {
                x.errors_count += v.errors_count;
                x.throttled_count += v.throttled_count;
            }
            None => p.verbs.push(VerbInterval {
                ops_count: 0,
                ok_ops_count: 0,
                latency_sum_ms: 0.0,
                latency_rows_count: Vec::new(),
                ..v
            }),
        }
    }
    t
}

/// Intervals a timeline keeps at most (design v7 P2): beyond it the oldest closed intervals
/// are merged in pairs, so a recorder embedded in a long-lived process stays bounded (a month of
/// 10-second intervals would be 259 200) and its file opens quickly.
const MAX_INTERVALS: usize = 2000;

/// Intervals an operation still running can hold open (with their latency histograms): an
/// interval older than this closes even if one of its operations has not finished.
const MAX_OPEN_INTERVALS: usize = MAX_INTERVALS / 4;

/// Start and length (s) of interval `idx`.
fn bucket_span(idx: usize) -> (f64, f64) {
    if idx < FINE_INTERVALS {
        (idx as f64, 1.0)
    } else {
        (
            (FINE_INTERVALS + (idx - FINE_INTERVALS) * COARSE_S) as f64,
            COARSE_S as f64,
        )
    }
}

/// A closed interval moved out of the live timeline (it may span several intervals once merged).
struct Archived {
    t_s: f64,
    len_s: f64,
    b: Bucket,
}

impl Archived {
    /// Two adjacent closed intervals as one. Counts add up; the quantiles of the merged
    /// interval are the operation-weighted means of the two (the maximum is exact): an
    /// approximation, for intervals old enough to be merged.
    fn merge(self, next: Archived) -> Archived {
        let (mut x, y) = (self.b, next.b);
        let (wx, wy) = (x.ops as f64, y.ops as f64);
        for i in 0..4 {
            x.q_us[i] = if wx + wy > 0.0 {
                ((x.q_us[i] as f64 * wx + y.q_us[i] as f64 * wy) / (wx + wy)) as u64
            } else {
                x.q_us[i].max(y.q_us[i])
            };
        }
        x.q_us[4] = x.q_us[4].max(y.q_us[4]);
        x.ops += y.ops;
        x.ok_ops += y.ok_ops;
        x.bytes += y.bytes;
        x.errors += y.errors;
        x.throttled += y.throttled;
        x.throttled_ops += y.throttled_ops;
        x.requests += y.requests;
        x.retries += y.retries;
        x.missed += y.missed;
        x.dials += y.dials;
        x.inflight_peak = x.inflight_peak.max(y.inflight_peak);
        x.cpu_pct = match (x.cpu_pct, y.cpu_pct) {
            (Some(a), Some(b)) => Some((a + b) / 2.0),
            (a, b) => a.or(b),
        };
        for (h, k) in x.heat.iter_mut().zip(y.heat) {
            *h += k;
        }
        for v in y.verbs {
            let w = x.verb(v.verb);
            w.ops += v.ops;
            w.ok += v.ok;
            w.errors += v.errors;
            w.throttled += v.throttled;
            w.latency_us += v.latency_us;
            for (h, k) in w.heat.iter_mut().zip(v.heat) {
                *h += k;
            }
        }
        Archived {
            t_s: self.t_s,
            len_s: next.t_s + next.len_s - self.t_s,
            b: x,
        }
    }
}

/// One verb's counters in one interval (the conventions of [`Bucket`]).
struct VerbBucket {
    verb: &'static str,
    ops: u64,
    ok: u64,
    errors: u64,
    throttled: u64,
    latency_us: u64,
    heat: [u32; 20],
}

/// One interval of the timeline. Operation counts, bytes, latencies and heat rows belong to
/// the interval the operation started in; `errors` (when the operation ended), `throttled`
/// (when the response arrived), `requests` (when sent) and `retries` (when issued) to the
/// interval they happened in.
struct Bucket {
    ops: u64,
    ok_ops: u64,
    bytes: u64,
    errors: u64,
    /// Throttled responses.
    throttled: u64,
    /// Operations that saw a throttled response.
    throttled_ops: u64,
    requests: u64,
    retries: u64,
    /// Per verb, in first-seen order (a handful: a linear scan beats a map).
    verbs: Vec<VerbBucket>,
    /// Open-loop arrivals due in this interval that never started.
    missed: u64,
    /// Whether the interval is still open: an operation that started in it may still finish.
    open: bool,
    /// Latencies of the interval while it is open, allocated by its first operation (an open
    /// interval nothing started in costs no histogram); dropped (and its quantiles kept) once
    /// every operation that started in it finished, so a long run holds a few numbers per
    /// interval, not a histogram each, and a snapshot does not recompute quantiles over the
    /// whole run.
    latency: Option<Histogram<u64>>,
    /// p50, p90, p99, p99.9, max (µs) of a closed interval, at full histogram precision.
    q_us: [u64; 5],
    /// Operations per heatmap latency row ([`HEAT_BOUNDS_MS`]).
    heat: [u32; 20],
    /// Peak requests in flight seen by the series probe.
    inflight_peak: u64,
    /// Connections dialed during the interval.
    dials: u64,
    /// Client CPU (percent of one core) sampled in the interval.
    cpu_pct: Option<f64>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            ops: 0,
            ok_ops: 0,
            bytes: 0,
            errors: 0,
            throttled: 0,
            throttled_ops: 0,
            requests: 0,
            retries: 0,
            verbs: Vec::new(),
            missed: 0,
            open: true,
            latency: None,
            q_us: [0; 5],
            heat: [0; 20],
            inflight_peak: 0,
            dials: 0,
            cpu_pct: None,
        }
    }

    fn qs(h: &Histogram<u64>) -> [u64; 5] {
        if h.is_empty() {
            return [0; 5];
        }
        [
            h.value_at_quantile(0.5),
            h.value_at_quantile(0.9),
            h.value_at_quantile(0.99),
            h.value_at_quantile(0.999),
            h.max(),
        ]
    }

    fn close(&mut self) {
        self.open = false;
        if let Some(h) = self.latency.take() {
            self.q_us = Self::qs(&h);
        }
    }

    fn quantiles(&self) -> [u64; 5] {
        match &self.latency {
            Some(h) => Self::qs(h),
            None => self.q_us,
        }
    }

    fn verb(&mut self, verb: &'static str) -> &mut VerbBucket {
        let i = match self.verbs.iter().position(|v| v.verb == verb) {
            Some(i) => i,
            None => {
                self.verbs.push(VerbBucket {
                    verb,
                    ops: 0,
                    ok: 0,
                    errors: 0,
                    throttled: 0,
                    latency_us: 0,
                    heat: [0; 20],
                });
                self.verbs.len() - 1
            }
        };
        &mut self.verbs[i]
    }
}

#[derive(Default)]
struct ConnInfo {
    dialed: u64,
    failures: u64,
    alpn: String,
    tls_version: String,
    cipher: String,
    remote_addrs: Vec<String>,
}

struct Agg {
    all_latency: Histogram<u64>,
    /// Keyed by the static verb name: no allocation per operation (X6).
    verbs: BTreeMap<&'static str, VerbAgg>,
    apis: BTreeMap<Api, ApiAgg>,
    /// The live intervals: `buckets[i]` is interval `base + i`.
    buckets: Vec<Bucket>,
    /// The interval index of `buckets[0]`.
    base: usize,
    /// Closed intervals moved out of `buckets`, oldest first, merged in pairs as the timeline
    /// grows ([`MAX_INTERVALS`]).
    archive: Vec<Archived>,
    slow: Vec<SlowOp>,
    dns: Histogram<u64>,
    tcp: Histogram<u64>,
    proxy: Histogram<u64>,
    tls: Histogram<u64>,
    conn: ConnInfo,
    requests: u64,
    reused_requests: u64,
    h1_requests: u64,
    h2_requests: u64,
    res: ResilienceStats,
    bytes: ByteStats,
    cache_lookups: u64,
    cache_hits: u64,
    cache_lookup_us: u64,
    integrity: IntegrityStats,
    ops: u64,
    failed_ops: u64,
    logical_read: u64,
    logical_write: u64,
    open_loop: Option<OpenLoopStats>,
    /// The `schedule` stage of open-loop operations (intended → actual start).
    schedule: Histogram<u64>,
    /// Missed arrivals recorded as censored latencies.
    censored: u64,
    /// Prefixes, hot keys, size buckets, cold vs warm (design v5 §3, M12).
    dims: crate::dims::DimAgg,
}

impl Agg {
    fn new() -> Self {
        Self {
            all_latency: hist(),
            verbs: BTreeMap::new(),
            apis: BTreeMap::new(),
            buckets: Vec::new(),
            base: 0,
            archive: Vec::new(),
            slow: Vec::new(),
            dns: hist(),
            tcp: hist(),
            proxy: hist(),
            tls: hist(),
            conn: ConnInfo::default(),
            requests: 0,
            reused_requests: 0,
            h1_requests: 0,
            h2_requests: 0,
            res: ResilienceStats::default(),
            bytes: ByteStats::default(),
            cache_lookups: 0,
            cache_hits: 0,
            cache_lookup_us: 0,
            integrity: IntegrityStats::default(),
            ops: 0,
            failed_ops: 0,
            logical_read: 0,
            logical_write: 0,
            open_loop: None,
            schedule: hist(),
            censored: 0,
            dims: crate::dims::DimAgg::default(),
        }
    }

    /// Live interval `idx` (an event in an interval already archived lands in the oldest live
    /// one). [`Recorder::ensure_bucket`] made it exist.
    fn bucket(&mut self, idx: usize) -> &mut Bucket {
        let i = idx.saturating_sub(self.base).min(self.buckets.len() - 1);
        &mut self.buckets[i]
    }

    /// Keep the timeline within [`MAX_INTERVALS`]: archive the closed intervals at the front,
    /// then merge the oldest half of the archive in pairs until it fits.
    fn compact(&mut self) {
        if self.archive.len() + self.buckets.len() <= MAX_INTERVALS {
            return;
        }
        let closed = self
            .buckets
            .iter()
            .take_while(|b| !b.open)
            .count()
            .min(self.buckets.len().saturating_sub(1));
        let base = self.base;
        self.archive
            .extend(self.buckets.drain(..closed).enumerate().map(|(i, b)| {
                let (t_s, len_s) = bucket_span(base + i);
                Archived { t_s, len_s, b }
            }));
        self.base += closed;
        while self.archive.len() + self.buckets.len() > MAX_INTERVALS && self.archive.len() >= 2 {
            let n = (self.archive.len() / 2).max(2) & !1;
            let mut old = self.archive.drain(..n);
            let mut merged = Vec::with_capacity(n / 2);
            while let (Some(x), Some(y)) = (old.next(), old.next()) {
                merged.push(x.merge(y));
            }
            drop(old);
            self.archive.splice(0..0, merged);
        }
    }

    fn record_request(&mut self, r: &RequestRecord) {
        self.requests += 1;
        // `dialed(r)`: counted in `conn.dialed` here, and in its interval by the caller.
        let is_cut = r.detail.as_deref().is_some_and(|d| d.cut);
        if is_cut {
            self.res.tail_cuts += 1;
        }
        // Whether the request reused a connection is only known where its connection was
        // observed: the instrumented client names it (`conn_id`), and an in-process call has
        // none to dial. A request through a transport the layer only wraps
        // (`Config::keep_http_transport`) was answered on a connection nobody saw: it counts
        // by protocol, and neither as warm nor as a dial (9.2 counted every one as warm:
        // "100 % reused").
        let observed = r.conn_id > 0 || r.protocol == Protocol::InProcess;
        if observed && r.reused {
            self.reused_requests += 1;
        }
        if observed || r.status != 0 {
            match r.protocol {
                Protocol::Http1 => self.h1_requests += 1,
                Protocol::Http2 => self.h2_requests += 1,
                Protocol::InProcess => {}
            }
        }
        self.bytes.backend_down += r.bytes_down;
        self.bytes.backend_up += r.bytes_up;
        match r.status {
            0 if !is_cut => self.res.network_errors += 1,
            0 => {}
            429 => self.res.status_429 += 1,
            503 if r.is_throttle() => self.res.status_503 += 1,
            s if s >= 500 => self.res.server_errors += 1,
            s if s >= 400 && s != 404 => self.res.client_errors += 1,
            _ if r.error.is_some() && !is_cut => self.res.network_errors += 1,
            _ => {}
        }
        if r.is_throttle() {
            self.res.throttled_responses += 1;
        }
        if let Some(ra) = r.retry_after_ms {
            self.res.max_retry_after_ms = self.res.max_retry_after_ms.max(ra);
        }
        let a = self.apis.entry(r.api).or_insert_with(ApiAgg::new);
        a.count += 1;
        if r.is_error() && r.status != 404 && !is_cut {
            a.errors += 1;
        }
        if r.is_throttle() {
            a.throttled += 1;
        }
        if observed {
            a.connected += 1;
            if r.reused {
                a.reused += 1;
            }
        }
        *a.status.entry(r.status).or_default() += 1;
        a.bytes_up += r.bytes_up;
        a.bytes_down += r.bytes_down;
        let t = &r.timing;
        rec(&mut a.stages.queue, t.queue_us);
        // A connection counts as dialed in the window through the request that used it first,
        // so "new connections" and "warm %" always describe the same requests (and unconnected
        // cancelled requests with `status == 0` and `connect_us == 0` do not pollute `connect`).
        if dialed(r) {
            rec(&mut a.stages.connect, t.connect_us());
            self.conn.dialed += 1;
            rec(&mut self.dns, t.dns_us);
            rec(&mut self.tcp, t.tcp_us);
            if t.proxy_us > 0 {
                rec(&mut self.proxy, t.proxy_us);
            }
            if t.tls_us > 0 {
                rec(&mut self.tls, t.tls_us);
            }
        }
        rec(&mut a.stages.send, t.send_us);
        rec(&mut a.stages.ttfb, t.ttfb_us);
        rec(&mut a.stages.body, t.body_us);
        rec(&mut a.stages.stall, t.stall_us);
        rec(&mut a.stages.total, t.total_us);
    }
}

/// A completed logical operation handed to the recorder.
pub struct OpCompletion {
    pub verb: &'static str,
    pub op_id: u64,
    /// When the operation actually started.
    pub started: Instant,
    /// Latency from the *intended* start (open-loop) or actual start (closed-loop).
    pub latency_us: u64,
    pub logical_read: u64,
    pub logical_write: u64,
    pub error: Option<String>,
    /// Object key (or listed prefix) the operation worked on.
    pub key: String,
    pub data: OpScopeData,
    /// Open-loop operations: time from the intended to the actual start (the `schedule`
    /// stage, part of `latency_us`); `None` in closed-loop runs.
    pub schedule_us: Option<u64>,
}

/// Dial outcome reported by the transport.
pub struct DialRecord {
    pub ok: bool,
    /// Egress proxy the connection was tunnelled through.
    pub proxy: Option<String>,
    pub dns_us: u64,
    pub tcp_us: u64,
    pub proxy_us: u64,
    pub tls_us: u64,
    pub remote: String,
    pub alpn: String,
    pub tls_version: String,
    pub cipher: String,
}

/// Marks one HTTP request in flight for its whole life: dropping it (normal completion, error,
/// or the request future being cancelled by a timeout / tail-cut) releases the slot exactly once.
pub struct InflightGuard(Option<Arc<Recorder>>);

impl InflightGuard {
    pub fn start(rec: Option<Arc<Recorder>>) -> Self {
        if let Some(r) = &rec {
            r.request_started();
        }
        Self(rec)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(r) = &self.0 {
            r.request_finished();
        }
    }
}

/// Budgets fed by the recorder: every completed operation's requests and bytes (all phases).
/// `oprobe-core`'s safety guard implements it.
pub trait Budget: Send + Sync {
    fn add(&self, requests: u64, bytes: u64);
}

/// The exporter hook: called for every recorded request and operation, in every phase
/// (OpenTelemetry metrics with the `otel` feature, logs, your own sink). Keep it cheap: it
/// runs on the request path.
pub trait Exporter: Send + Sync {
    fn on_request(&self, r: &RequestRecord);
    /// A completed operation (`verb`, latency, failed).
    fn on_op(&self, verb: &'static str, latency_us: u64, failed: bool) {
        let _ = (verb, latency_us, failed);
    }
    /// Local cache lookups performed during an operation (`lookups`, `hits`).
    fn on_cache(&self, lookups: u64, hits: u64) {
        let _ = (lookups, hits);
    }
    /// The OpenDAL service the layer was applied to (once, before its first request): its
    /// scheme, name (the bucket: OpenDAL's `namespace` label) and root. A layer applied to
    /// several operators reports the first one here; each HTTP request names its own
    /// ([`RequestRecord::service`]).
    fn on_service(&self, scheme: &str, name: &str, root: &str) {
        let _ = (scheme, name, root);
    }
}

/// Receives every trace row of the measurement window as operations complete (design v5 §2):
/// the run file is written while the run is in progress, so the in-memory sample can stay
/// small. Called on the completion path: keep it cheap (hand rows to a writer thread).
pub trait RowSink: Send + Sync {
    /// One completed operation with all its requests.
    fn on_op(&self, op: &crate::rows::OpRow, reqs: &[crate::rows::ReqRow]);
    /// A request issued outside any operation.
    fn on_unscoped(&self, req: &crate::rows::ReqRow) {
        let _ = req;
    }
}

/// Trace log shards (each keeps `max_requests / TRACE_SHARDS` rows).
const TRACE_SHARDS: usize = 16;

/// One completed operation in this many is timed for the observer cost.
pub const OBSERVER_SAMPLE_EVERY: u64 = 16;

/// `Recorder::frozen_elapsed_us` while the measurement window is open.
const WINDOW_OPEN: u64 = u64::MAX;

/// Central sink shared by the transport, OpenDAL layers and workload workers.
pub struct Recorder {
    phase: AtomicU8,
    measuring: AtomicBool,
    /// Window length in microseconds once the window closed (no new operation is measured,
    /// while operations that started inside it may still be completing); `WINDOW_OPEN` before.
    frozen_elapsed_us: AtomicU64,
    window_start: Mutex<Instant>,
    agg: Mutex<Agg>,
    inflight_ops: AtomicU64,
    inflight_requests: AtomicU64,
    peak_inflight_requests: AtomicU64,
    /// The current interval's in-flight peak, kept off the aggregate lock (every request start
    /// updates it): `(interval + 1) << 32 | peak`, 0 for none. It is folded into its interval
    /// when the next interval starts and whenever the timeline is read.
    interval_peak: AtomicU64,
    wire_rx: AtomicU64,
    wire_tx: AtomicU64,
    cache_enabled: AtomicBool,
    proxy: Mutex<Option<String>>,
    // await-tree slow-path capture
    tree_registry: Mutex<Option<await_tree::Registry>>,
    /// A tree registry is set: completions look for captured trees.
    trees_on: AtomicBool,
    live_ops: Mutex<HashMap<u64, Instant>>,
    trees: Mutex<HashMap<u64, String>>,
    client: Mutex<ClientStats>,
    /// Per-request / per-operation rows of the measurement window, sharded by operation id so
    /// concurrent completions rarely contend.
    trace: Vec<Mutex<TraceLog>>,
    /// Safety rails fed with every completed operation (all phases).
    guard: OnceLock<Arc<dyn Budget>>,
    /// The exporter hook (all phases).
    exporter: OnceLock<Arc<dyn Exporter>>,
    /// Streams the window's rows (the run file writer).
    row_sink: OnceLock<Arc<dyn RowSink>>,
    /// Spreads requests issued outside any operation over the shards.
    unscoped_seq: AtomicU64,
    /// Observer cost: nanoseconds spent recording sampled operations, and how many.
    observer_ns: AtomicU64,
    observer_samples: AtomicU64,
    completions: AtomicU64,
    /// Cold start sampled during warmup: connect times and first-operation latencies.
    cold: Mutex<(Histogram<u64>, Histogram<u64>)>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(0),
            measuring: AtomicBool::new(false),
            frozen_elapsed_us: AtomicU64::new(WINDOW_OPEN),
            window_start: Mutex::new(Instant::now()),
            agg: Mutex::new(Agg::new()),
            inflight_ops: AtomicU64::new(0),
            inflight_requests: AtomicU64::new(0),
            peak_inflight_requests: AtomicU64::new(0),
            interval_peak: AtomicU64::new(0),
            wire_rx: AtomicU64::new(0),
            wire_tx: AtomicU64::new(0),
            cache_enabled: AtomicBool::new(false),
            proxy: Mutex::new(None),
            tree_registry: Mutex::new(None),
            trees_on: AtomicBool::new(false),
            live_ops: Mutex::new(HashMap::default()),
            trees: Mutex::new(HashMap::default()),
            client: Mutex::new(ClientStats {
                cpu_cores: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
                ..Default::default()
            }),
            trace: (0..TRACE_SHARDS)
                .map(|_| {
                    Mutex::new(TraceLog::new(
                        crate::log::DEFAULT_MAX_REQUESTS.div_ceil(TRACE_SHARDS),
                    ))
                })
                .collect(),
            guard: OnceLock::new(),
            exporter: OnceLock::new(),
            row_sink: OnceLock::new(),
            unscoped_seq: AtomicU64::new(0),
            observer_ns: AtomicU64::new(0),
            observer_samples: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            cold: Mutex::new((hist(), hist())),
        })
    }

    /// A recorder not tied to a run (what an application embeds): measuring from now on, with
    /// no window to open or close, keeping at most `max_rows` trace rows (whole operations are
    /// sampled beyond it; aggregates stay exact).
    pub fn always_on(max_rows: usize) -> Arc<Self> {
        let r = Self::new();
        r.set_trace_limit(max_rows);
        r.set_phase(Phase::Measure);
        r
    }

    /// Install the exporter hook (the first call wins; returns whether it was installed).
    pub fn set_exporter(&self, e: Arc<dyn Exporter>) -> bool {
        self.exporter.set(e).is_ok()
    }

    /// The exporter hook, when one is installed.
    pub fn exporter(&self) -> Option<&Arc<dyn Exporter>> {
        self.exporter.get()
    }

    fn trace(&self, op_id: u64) -> std::sync::MutexGuard<'_, TraceLog> {
        self.trace[(op_id as usize) % TRACE_SHARDS]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Stream every row of the measurement window to `sink` (the first call wins; returns
    /// whether it was installed).
    pub fn set_row_sink(&self, sink: Arc<dyn RowSink>) -> bool {
        self.row_sink.set(sink).is_ok()
    }

    /// Cap of kept trace rows (`trace.max_requests`).
    pub fn set_trace_limit(&self, max_requests: usize) {
        for t in &self.trace {
            t.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_max_requests(max_requests.div_ceil(TRACE_SHARDS));
        }
    }

    /// Feed every completed operation's requests and bytes to `guard` (budgets).
    pub fn set_guard(&self, guard: Arc<dyn Budget>) {
        let _ = self.guard.set(guard);
    }

    /// About how many bytes the kept trace rows take as JSON.
    pub fn trace_json_len(&self) -> u64 {
        self.trace
            .iter()
            .map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).json_len())
            .sum()
    }

    /// Take the trace rows recorded in the measurement window.
    pub fn take_trace(&self) -> TraceRows {
        let parts: Vec<TraceRows> = self
            .trace
            .iter()
            .map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).take())
            .collect();
        TraceRows::merge(parts)
    }

    fn agg(&self) -> std::sync::MutexGuard<'_, Agg> {
        self.agg.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_cache_enabled(&self, on: bool) {
        self.cache_enabled.store(on, Ordering::Relaxed);
    }

    pub fn set_proxy(&self, proxy: Option<String>) {
        *self.proxy.lock().unwrap_or_else(|e| e.into_inner()) =
            proxy.map(|p| crate::util::strip_url_credentials(&p));
    }

    pub fn set_tree_registry(&self, reg: await_tree::Registry) {
        *self.tree_registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(reg);
        self.trees_on.store(true, Ordering::Release);
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Acquire) {
            1 => Phase::Preparing,
            2 => Phase::Warmup,
            3 => Phase::Measure,
            4 => Phase::Verify,
            5 => Phase::Cleanup,
            6 => Phase::Done,
            _ => Phase::Idle,
        }
    }

    /// Switch phase. Entering [`Phase::Measure`] resets every aggregate and starts the window.
    pub fn set_phase(&self, p: Phase) {
        let code = match p {
            Phase::Idle => 0,
            Phase::Preparing => 1,
            Phase::Warmup => 2,
            Phase::Measure => 3,
            Phase::Verify => 4,
            Phase::Cleanup => 5,
            Phase::Done => 6,
        };
        if p == Phase::Measure {
            let mut a = self.agg();
            let conn = std::mem::take(&mut a.conn);
            *a = Agg::new();
            a.conn = ConnInfo {
                dialed: 0,
                failures: 0,
                ..conn
            };
            *self.window_start.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            self.frozen_elapsed_us.store(WINDOW_OPEN, Ordering::Release);
            drop(a);
            for t in &self.trace {
                t.lock().unwrap_or_else(|e| e.into_inner()).reset();
            }
            self.wire_rx.store(0, Ordering::Relaxed);
            self.wire_tx.store(0, Ordering::Relaxed);
            self.peak_inflight_requests.store(
                self.inflight_requests.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            self.interval_peak.store(0, Ordering::Release);
            self.measuring.store(true, Ordering::Release);
        } else if self.measuring.load(Ordering::Acquire) {
            // Freeze the window length at the moment measurement ends (unless the runner already
            // closed it at its deadline).
            self.close_window(Instant::now());
            // The last interval's in-flight peak, before nothing folds it any more.
            let packed = self.interval_peak.load(Ordering::Acquire);
            let (mut a, window_start) = self.agg_at();
            self.fold_interval_peak(&mut a, packed, window_start);
            drop(a);
            self.measuring.store(false, Ordering::Release);
        }
        self.phase.store(code, Ordering::Release);
    }

    /// End the measurement window at `at` (normally the deadline): rates are computed over
    /// `window_start..at`, operations starting afterwards are not measured, but operations that
    /// started inside the window keep being recorded until the phase changes (drain).
    /// Idempotent: the first call wins.
    pub fn close_window(&self, at: Instant) {
        if !self.measuring.load(Ordering::Acquire) {
            return;
        }
        let len = at.saturating_duration_since(self.window_start());
        let us = (len.as_micros() as u64).min(WINDOW_OPEN - 1);
        let _ = self.frozen_elapsed_us.compare_exchange(
            WINDOW_OPEN,
            us,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Whether the measurement phase is active (including the drain after the window closed):
    /// completions of operations that started in the window are still recorded.
    #[inline]
    pub fn is_measuring(&self) -> bool {
        self.measuring.load(Ordering::Acquire)
    }

    /// Whether an operation starting now belongs to the measurement window.
    #[inline]
    pub fn window_open(&self) -> bool {
        self.is_measuring() && self.frozen_elapsed_us.load(Ordering::Acquire) == WINDOW_OPEN
    }

    pub fn window_start(&self) -> Instant {
        *self.window_start.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The aggregates, locked, with the start of the window they belong to. A reset replaces
    /// both under this lock: a start read before taking it may be the previous window's, and
    /// an event placed by it lands in the wrong interval of the new window (9.3.3: a window
    /// 6 ms old held intervals up to 3 600 s).
    fn agg_at(&self) -> (std::sync::MutexGuard<'_, Agg>, Instant) {
        let a = self.agg();
        let start = self.window_start();
        (a, start)
    }

    /// Length of the measurement window: time since it started, or its final length once
    /// closed (draining operations do not stretch it).
    pub fn elapsed_s(&self) -> f64 {
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        if frozen != WINDOW_OPEN {
            return frozen as f64 / 1e6;
        }
        self.window_start().elapsed().as_secs_f64()
    }

    pub fn add_wire(&self, rx: u64, tx: u64) {
        if self.is_measuring() {
            if rx > 0 {
                self.wire_rx.fetch_add(rx, Ordering::Relaxed);
            }
            if tx > 0 {
                self.wire_tx.fetch_add(tx, Ordering::Relaxed);
            }
        }
    }

    pub fn request_started(&self) {
        let cur = self.inflight_requests.fetch_add(1, Ordering::AcqRel) + 1;
        if self.is_measuring() {
            self.peak_inflight_requests
                .fetch_max(cur, Ordering::Relaxed);
            // The interval's peak, where the request starts: an embedded layer has no series
            // probe ([`Self::sample_series`]) to sample it. Lock-free: this runs on every
            // request, from every worker.
            self.note_interval_peak(cur);
        }
    }

    /// Raise the current interval's in-flight peak to `cur` without taking the aggregate lock;
    /// the first request of a new interval folds the last interval's peak into the timeline.
    fn note_interval_peak(&self, cur: u64) {
        if !self.window_open() {
            return;
        }
        let window_start = self.window_start();
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        let idx = event_bucket(window_start.elapsed().as_micros() as u64, frozen) as u64 + 1;
        let new = (idx << 32) | cur.min(u64::from(u32::MAX));
        let mut old = self.interval_peak.load(Ordering::Acquire);
        loop {
            let (old_idx, old_peak) = (old >> 32, old & u64::from(u32::MAX));
            if old_idx == idx && old_peak >= cur {
                return;
            }
            match self.interval_peak.compare_exchange_weak(
                old,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Whichever interval it held (an earlier one, or a later one from a thread
                    // racing a window reset), the value it replaced is folded, never lost.
                    if old_idx != idx && old_idx != 0 {
                        let (mut a, window_start) = self.agg_at();
                        self.fold_interval_peak(&mut a, old, window_start);
                    }
                    return;
                }
                Err(x) => old = x,
            }
        }
    }

    /// Fold a packed [`Self::interval_peak`] into its interval (a max, so folding twice is
    /// harmless). An interval the timeline no longer holds open (compacted), or one past the
    /// current window's (stamped against the window before a reset), is dropped rather than
    /// folded into an unrelated interval.
    fn fold_interval_peak(&self, a: &mut Agg, packed: u64, window_start: Instant) {
        let Some(idx) = (packed >> 32).checked_sub(1) else {
            return;
        };
        let peak = packed & u64::from(u32::MAX);
        let idx = idx as usize;
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        let now = event_bucket(window_start.elapsed().as_micros() as u64, frozen);
        if idx < a.base || idx > now {
            return;
        }
        self.ensure_bucket(a, idx, window_start);
        let b = a.bucket(idx);
        b.inflight_peak = b.inflight_peak.max(peak);
    }

    /// Run `f` on the interval the window is in now (nothing outside the window).
    fn with_current_bucket(&self, f: impl FnOnce(&mut Bucket)) {
        if !self.window_open() {
            return;
        }
        let (mut a, window_start) = self.agg_at();
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        let idx = event_bucket(window_start.elapsed().as_micros() as u64, frozen);
        self.ensure_bucket(&mut a, idx, window_start);
        f(a.bucket(idx));
    }

    pub fn request_finished(&self) {
        self.inflight_requests.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn record_dial(&self, d: DialRecord) {
        let measuring = self.is_measuring();
        let mut a = self.agg();
        if !d.ok {
            if measuring {
                a.conn.failures += 1;
            }
            return;
        }
        if let Some(p) = d.proxy {
            *self.proxy.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(crate::util::strip_url_credentials(&p));
        }
        a.conn.alpn = d.alpn;
        a.conn.tls_version = d.tls_version;
        a.conn.cipher = d.cipher;
        if !d.remote.is_empty()
            && !a.conn.remote_addrs.contains(&d.remote)
            && a.conn.remote_addrs.len() < 16
        {
            a.conn.remote_addrs.push(d.remote);
        }
        // Dial counts and phase timings come from the cold request that used the connection
        // (`record_request`); dials only contribute metadata and failures.
    }

    /// A connection whose handshake finished before any request waited on it: hyper's pool
    /// races a new dial against an idle connection and keeps the loser. It is a new connection
    /// (counted, with its handshake phases), but the request that later takes it saw it warm,
    /// so the phases are not charged to that request.
    ///
    /// During the warm-up it is one of the cold start's connections, like the ones the
    /// warm-up's operations waited for: every connection the client dials is counted once,
    /// in the phase it was dialed in (until 9.2 these were in no count, so a profile showed
    /// more connections in use than it said were dialed).
    pub fn record_unclaimed_dial(&self, dns_us: u64, tcp_us: u64, proxy_us: u64, tls_us: u64) {
        if !self.is_measuring() {
            if self.phase() == Phase::Warmup {
                let mut cold = self.cold.lock().unwrap_or_else(|e| e.into_inner());
                rec(&mut cold.0, dns_us + tcp_us + proxy_us + tls_us);
            }
            return;
        }
        self.with_current_bucket(|b| b.dials += 1);
        let mut a = self.agg();
        a.conn.dialed += 1;
        rec(&mut a.dns, dns_us);
        rec(&mut a.tcp, tcp_us);
        if proxy_us > 0 {
            rec(&mut a.proxy, proxy_us);
        }
        if tls_us > 0 {
            rec(&mut a.tls, tls_us);
        }
    }

    /// Hand a request of an operation in progress to the exporter, as it is captured.
    pub fn export_request(&self, r: &RequestRecord) {
        if let Some(e) = self.exporter.get() {
            e.on_request(r);
        }
    }

    /// Request issued outside any operation scope (rare: background OpenDAL work).
    pub fn record_unscoped_request(&self, r: &RequestRecord) {
        crate::scope::note_backend_request();
        // The capture boundary: an error text never keeps a URL's credentials.
        let redacted;
        let r = match r.error.as_deref().and_then(crate::util::redacted) {
            Some(error) => {
                redacted = RequestRecord {
                    error: Some(error),
                    ..r.clone()
                };
                &redacted
            }
            None => r,
        };
        if let Some(g) = self.guard.get() {
            g.add(1, r.bytes_up + r.bytes_down);
        }
        if let Some(e) = self.exporter.get() {
            e.on_request(r);
        }
        if self.is_measuring() {
            let now_us;
            {
                let (mut a, window_start) = self.agg_at();
                // Unscoped: its offset is not relative to an operation; it ended just now.
                now_us = window_start.elapsed().as_micros() as i64;
                let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
                let ended = event_bucket(now_us as u64, frozen);
                let sent = event_bucket((now_us as u64).saturating_sub(r.timing.total_us), frozen);
                a.record_request(r);
                self.ensure_bucket(&mut a, ended.max(sent), window_start);
                a.bucket(sent).requests += 1;
                if dialed(r) {
                    a.bucket(sent).dials += 1;
                }
                if r.is_throttle() {
                    a.bucket(ended).throttled += 1;
                }
            }
            let mut row = req_row(r.clone(), 0, 0, 1);
            row.t_us = now_us - r.timing.total_us as i64;
            if let Some(sink) = self.row_sink.get() {
                sink.on_unscoped(&row);
            }
            // Spread over every shard (each holds its share of the row budget).
            let n = self.unscoped_seq.fetch_add(1, Ordering::Relaxed) as usize;
            self.trace[n % TRACE_SHARDS]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_unscoped(row);
        }
    }

    /// A retry outside any operation scope (an embedder's retry layer with no operation to
    /// attribute it to): counted in the resilience totals and the interval it was issued in.
    pub fn record_unscoped_retry(&self, backoff: std::time::Duration) {
        if !self.is_measuring() {
            return;
        }
        let (mut a, window_start) = self.agg_at();
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        let issued = window_start.elapsed() + backoff;
        let idx = event_bucket(issued.as_micros() as u64, frozen);
        a.res.retries += 1;
        a.res.retry_backoff_ms += backoff.as_secs_f64() * 1000.0;
        self.ensure_bucket(&mut a, idx, window_start);
        a.bucket(idx).retries += 1;
    }

    pub fn record_integrity(&self, ok: bool) {
        let mut a = self.agg();
        if ok {
            a.integrity.verified += 1;
        } else {
            a.integrity.corrupted += 1;
        }
    }

    pub fn set_open_loop(&self, s: OpenLoopStats) {
        self.agg().open_loop = Some(s);
    }

    /// An open-loop arrival due at `intended` that never started (dropped behind schedule, or
    /// still waiting when the window ended): its latency is unknown but at least
    /// `lower_bound_us`. It is counted in the latency distribution as that censored value (so
    /// the tail cannot hide a backlog: no coordinated omission) and in the second it was due.
    pub fn record_missed_arrival(&self, intended: Instant, lower_bound_us: u64) {
        if !self.is_measuring() {
            return;
        }
        let (mut a, window_start) = self.agg_at();
        let idx = bucket_index(intended.saturating_duration_since(window_start).as_secs());
        a.censored += 1;
        rec(&mut a.all_latency, lower_bound_us.max(1));
        self.ensure_bucket(&mut a, idx, window_start);
        a.bucket(idx).missed += 1;
    }

    pub fn op_started(&self, op_id: u64) {
        self.op_started_at(op_id, Instant::now());
    }

    /// An operation started at `at` (its intended start in open-loop runs).
    pub fn op_started_at(&self, op_id: u64, at: Instant) {
        self.inflight_ops.fetch_add(1, Ordering::Relaxed);
        self.live_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op_id, at);
    }

    /// Warmup operations that dialed: the cold start (connection setup is otherwise absorbed
    /// by warmup and missing from short runs).
    fn record_cold_start(&self, c: &OpCompletion) {
        let cold: Vec<u64> = c
            .data
            .requests
            .iter()
            .filter(|r| !r.reused && r.protocol != Protocol::InProcess && r.timing.connect_us() > 0)
            .map(|r| r.timing.connect_us())
            .collect();
        if cold.is_empty() {
            return;
        }
        let mut g = self.cold.lock().unwrap_or_else(|e| e.into_inner());
        for us in cold {
            rec(&mut g.0, us);
        }
        rec(&mut g.1, c.latency_us);
    }

    /// Measured observer cost per operation (µs): the recorder's bookkeeping of one completed
    /// operation, timed on one operation in [`OBSERVER_SAMPLE_EVERY`].
    pub fn observer_per_op_us(&self) -> Option<f64> {
        let n = self.observer_samples.load(Ordering::Relaxed);
        (n > 0).then(|| self.observer_ns.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0)
    }

    /// Record a completed logical operation (only if it started inside the window).
    pub fn complete_op(&self, measured: bool, mut c: OpCompletion) {
        // The capture boundary, as for requests: no URL credentials in an error text.
        if let Some(clean) = c.error.as_deref().and_then(crate::util::redacted) {
            c.error = Some(clean);
        }
        let n = self.completions.fetch_add(1, Ordering::Relaxed);
        if n.is_multiple_of(OBSERVER_SAMPLE_EVERY) {
            let t = Instant::now();
            self.complete_op_inner(measured, c);
            self.observer_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.observer_samples.fetch_add(1, Ordering::Relaxed);
        } else {
            self.complete_op_inner(measured, c);
        }
    }

    fn complete_op_inner(&self, measured: bool, c: OpCompletion) {
        self.inflight_ops.fetch_sub(1, Ordering::Relaxed);
        if let Some(e) = self.exporter.get() {
            let first = c.data.exported_requests.min(c.data.requests.len());
            for r in &c.data.requests[first..] {
                e.on_request(r);
            }
            if c.data.cache_lookups > 0 {
                e.on_cache(c.data.cache_lookups, c.data.cache_hits);
            }
            e.on_op(c.verb, c.latency_us, c.error.is_some());
        }
        if let Some(g) = self.guard.get() {
            let bytes: u64 = c
                .data
                .requests
                .iter()
                .map(|r| r.bytes_up + r.bytes_down)
                .sum();
            g.add(
                c.data.requests.len() as u64 + c.data.dropped_requests,
                bytes,
            );
        }
        let forget_live = || {
            self.live_ops
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&c.op_id);
        };
        let tree = if self.trees_on.load(Ordering::Acquire) {
            self.trees
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&c.op_id)
        } else {
            None
        };
        if !measured || !self.is_measuring() {
            forget_live();
            if self.phase() == Phase::Warmup {
                self.record_cold_start(&c);
            }
            return;
        }
        let mut bd = breakdown_us(&c.data, c.latency_us);
        if let Some(s) = c.schedule_us {
            // Waiting for an open-loop slot is queueing (an in-flight cap), not client work:
            // no request of the operation runs before it starts.
            let moved = s.min(bd[4]);
            bd[4] -= moved;
            bd[0] += moved;
        }
        let ok = c.error.is_none();
        let throttled = c.data.throttled;
        let throttled_requests = c
            .data
            .requests
            .iter()
            .filter(|r| r.is_throttle())
            .count() as u64;
        let bytes = c.logical_read + c.logical_write;
        let req_count = c.data.requests.len() as u64 + c.data.dropped_requests;

        let (mut a, window_start) = self.agg_at();
        let at_s = c
            .started
            .saturating_duration_since(window_start)
            .as_secs_f64();
        // Leave the live set only under the aggregate lock: until then this operation keeps
        // its second open, so a concurrent completion cannot close that second before this
        // sample is in it.
        forget_live();
        for r in &c.data.requests {
            a.record_request(r);
        }
        a.ops += 1;
        a.dims.record(&crate::dims::DimOp {
            verb: c.verb,
            key: &c.key,
            bytes,
            latency_us: c.latency_us,
            requests: req_count,
            throttled_requests,
            failed: !ok,
        });
        a.logical_read += c.logical_read;
        a.logical_write += c.logical_write;
        a.cache_lookups += c.data.cache_lookups;
        a.cache_hits += c.data.cache_hits;
        a.cache_lookup_us += c.data.cache_lookup_us;
        a.bytes.cache_hit_bytes += c.data.cache_hit_bytes;
        a.res.retries += c.data.retries;
        a.res.retry_backoff_ms += c.data.retry_backoff_us as f64 / 1000.0;
        if let Some(ref e) = c.error {
            a.failed_ops += 1;
            *a.res.error_classes.entry(e.clone()).or_default() += 1;
        }
        rec(&mut a.all_latency, c.latency_us);
        if let Some(s) = c.schedule_us {
            rec(&mut a.schedule, s);
        }
        {
            let v = a.verbs.entry(c.verb).or_insert_with(VerbAgg::new);
            rec(&mut v.latency, c.latency_us);
            v.count += 1;
            if !ok {
                v.errors += 1;
            }
            if throttled {
                v.throttled += 1;
            }
            v.bytes += bytes;
            v.requests += req_count;
            v.retries += c.data.retries;
            v.queue_us += bd[0];
            v.cache_us += bd[1];
            v.backend_us += bd[2];
            v.retry_us += bd[3];
            v.client_us += bd[4];
        }
        // The interval of each event: the operation's start for its latency, its end for a
        // failure, each request's send and response for requests and throttling, each
        // retry's issue (design 6.3 A1: events land in the interval they happened in).
        let start_us = c
            .started
            .saturating_duration_since(window_start)
            .as_micros() as u64;
        // An open-loop operation's latency also counts how late it started (`schedule_us`,
        // before `started`): it ends that much sooner after its start.
        let end_us = start_us + c.latency_us.saturating_sub(c.schedule_us.unwrap_or(0));
        let frozen = self.frozen_elapsed_us.load(Ordering::Acquire);
        let at = |us: u64| event_bucket(us, frozen);
        let idx = at(start_us);
        let end_idx = at(end_us);
        // The start interval first: a later one may close it, and its quantiles must hold
        // this operation.
        self.ensure_bucket(&mut a, idx, window_start);
        let heat = heat_index(c.latency_us);
        {
            let b = a.bucket(idx);
            b.ops += 1;
            b.bytes += bytes;
            if ok {
                b.ok_ops += 1;
            }
            if throttled {
                b.throttled_ops += 1;
            }
            // Requests whose rows were dropped (more than the scope keeps) count at the start.
            b.requests += c.data.dropped_requests;
            if b.open {
                rec(b.latency.get_or_insert_with(hist), c.latency_us);
            } else {
                // A late completion in an interval closed while it ran: its maximum stays exact.
                b.q_us[4] = b.q_us[4].max(c.latency_us.min(HIST_MAX_US));
            }
            b.heat[heat] += 1;
            let v = b.verb(c.verb);
            v.ops += 1;
            v.ok += u64::from(ok);
            v.latency_us += c.latency_us;
            v.heat[heat] += 1;
        }
        self.ensure_bucket(&mut a, end_idx, window_start);
        if !ok {
            let b = a.bucket(end_idx);
            b.errors += 1;
            b.verb(c.verb).errors += 1;
        }
        for r in &c.data.requests {
            let sent = start_us + r.start_offset_us;
            let i = at(sent);
            self.ensure_bucket(&mut a, i, window_start);
            a.bucket(i).requests += 1;
            // A new connection lands where the request that dialed it was sent.
            if dialed(r) {
                a.bucket(i).dials += 1;
            }
            if r.is_throttle() {
                let i = at(sent + r.timing.total_us);
                self.ensure_bucket(&mut a, i, window_start);
                let b = a.bucket(i);
                b.throttled += 1;
                b.verb(c.verb).throttled += 1;
            }
        }
        // Retries: when the next attempt was issued (the end of its backoff); retries beyond
        // the kept backoff intervals count when the operation ended.
        for &(_, issued) in &c.data.retry_waits {
            let i = at(start_us + issued);
            self.ensure_bucket(&mut a, i, window_start);
            a.bucket(i).retries += 1;
        }
        let rest = c
            .data
            .retries
            .saturating_sub(c.data.retry_waits.len() as u64);
        a.bucket(end_idx).retries += rest;

        // Keep the slowest operations with their full request timelines.
        let lat_ms = c.latency_us as f64 / 1000.0;
        let qualifies = a.slow.len() < SLOW_OPS_KEPT
            || a.slow.last().map(|s| lat_ms > s.latency_ms).unwrap_or(true);
        if qualifies {
            let ms = |v: u64| v as f64 / 1000.0;
            a.slow.push(SlowOp {
                op_id: c.op_id,
                key: c.key.clone(),
                verb: c.verb.to_owned(),
                latency_ms: lat_ms,
                bytes,
                at_s,
                breakdown: OpBreakdown {
                    queue_ms: ms(bd[0]),
                    cache_ms: ms(bd[1]),
                    backend_ms: ms(bd[2]),
                    retry_ms: ms(bd[3]),
                    client_ms: ms(bd[4]),
                },
                retries: c.data.retries,
                requests: c.data.requests.clone(),
                await_tree: tree.clone(),
                error: c.error.clone(),
            });
            a.slow.sort_by(|x, y| y.latency_ms.total_cmp(&x.latency_ms));
            a.slow.truncate(SLOW_OPS_KEPT);
        }
        // The row's trace shard, taken before the aggregate lock is released: a snapshot that
        // resets the window (under the aggregate lock) takes the rows after this one is in, so
        // an operation's row is in the same window as its aggregates.
        let mut shard = self.trace(c.op_id);
        drop(a);

        // Trace rows (moved out of the scope data: no per-request allocation).
        let op_t_us = signed_offset_us(window_start, c.started);
        let att = attempts(&c.data.requests);
        let att = |i: usize| att.as_ref().map_or(1, |a| a[i]);
        let mut row = op_row(
            c.op_id,
            c.verb,
            c.key,
            op_t_us,
            c.latency_us,
            bytes,
            c.error,
            throttled,
            c.data.retries,
            bd,
            req_count,
        );
        // The call stack captured while the operation was slow, on its own row: a run file
        // streams rows as operations complete (`RowSink`), so a stack patched in afterwards
        // from the slow-op samples never reached the file.
        row.await_tree = tree;
        let reqs: Vec<_> = c
            .data
            .requests
            .into_iter()
            .enumerate()
            .map(|(i, r)| req_row(r, c.op_id, op_t_us, att(i)))
            .collect();
        if let Some(sink) = self.row_sink.get() {
            sink.on_op(&row, &reqs);
        }
        // Whole operations: kept with all their requests, or not at all.
        shard.push_whole_op(row, reqs);
    }

    /// Make interval `idx` exist. A new interval closes the earlier ones that are complete:
    /// an interval is complete once every operation that started in it has finished (the
    /// intervals before the oldest operation still running). `live_ops` is never held while
    /// taking `agg`, so taking it here (under `agg`) cannot deadlock.
    fn ensure_bucket(&self, a: &mut Agg, idx: usize, window_start: Instant) {
        if a.base + a.buckets.len() > idx {
            return;
        }
        let oldest_live = self
            .live_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .min()
            .copied();
        // A long-lived operation (a writer or lister open for hours, a stream never drained)
        // holds at most `MAX_OPEN_INTERVALS` open: older intervals close anyway, so the
        // timeline can still be archived and stays bounded. Its completion then lands in the
        // closed (or archived) interval's counters, without its latency histogram.
        let first_open = oldest_live
            .map(|t| bucket_index(t.saturating_duration_since(window_start).as_secs()))
            .unwrap_or(idx)
            .max(idx.saturating_sub(MAX_OPEN_INTERVALS))
            .min(idx);
        for b in a.buckets.iter_mut().take(first_open.saturating_sub(a.base)) {
            b.close();
        }
        // An idle gap: the intervals more than `MAX_OPEN_INTERVALS` back saw nothing, and
        // events land there no more (they go to the oldest live interval). One archived span
        // stands for all of them, so waking up after 30 days of silence costs the same as
        // after 10 seconds (9.3.2 pushed one interval per 10 s of the gap, each with a
        // histogram: about 47 GiB after a month). The recent ones stay: the operation being
        // recorded may have events in them (a retry, a throttled answer).
        let end = a.base + a.buckets.len();
        let keep_from = idx.saturating_sub(MAX_OPEN_INTERVALS);
        if keep_from > end {
            let base = a.base;
            a.archive
                .extend(a.buckets.drain(..).enumerate().map(|(i, mut b)| {
                    b.close();
                    let (t_s, len_s) = bucket_span(base + i);
                    Archived { t_s, len_s, b }
                }));
            let (t_s, _) = bucket_span(end);
            let (t_end, _) = bucket_span(keep_from);
            let mut gap = Bucket::new();
            gap.close();
            a.archive.push(Archived {
                t_s,
                len_s: t_end - t_s,
                b: gap,
            });
            a.base = keep_from;
        }
        // At most `MAX_OPEN_INTERVALS + 1` new intervals, none with a histogram yet.
        while a.base + a.buckets.len() <= idx {
            a.buckets.push(Bucket::new());
        }
        a.compact();
    }

    /// The per-interval series probe (optional; `oprobe run` calls it every few ms): the
    /// requests in flight now land in the current interval. Requests and dials also fill their
    /// interval as they start ([`Self::request_started`], [`Self::complete_op`]), so an
    /// embedded layer without a probe has the same series.
    pub fn sample_series(&self) {
        let inflight = self.inflight_requests.load(Ordering::Relaxed);
        self.note_interval_peak(inflight);
    }

    /// Capture await-tree call stacks of operations running longer than `threshold_us`.
    pub fn sample_trees(&self, threshold_us: u64) {
        let reg = self
            .tree_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(reg) = reg else { return };
        let old: Vec<u64> = self
            .live_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, t)| t.elapsed().as_micros() as u64 >= threshold_us)
            .map(|(id, _)| *id)
            .collect();
        if old.is_empty() {
            return;
        }
        let subs = reg.collect::<crate::scope::SubTaskKey>();
        let mut trees = self.trees.lock().unwrap_or_else(|e| e.into_inner());
        for id in old {
            if let Some(t) = reg.get(id) {
                let mut dump = t.to_string().trim_end().to_owned();
                for (k, st) in subs.iter().filter(|(k, _)| k.0 == id) {
                    dump.push_str(&format!(
                        "\n  [sub-task {}] {}",
                        k.1,
                        st.to_string().trim_end().replace('\n', "\n  ")
                    ));
                }
                // Keep the most informative capture of this operation.
                let keep = trees
                    .get(&id)
                    .map(|prev| dump.lines().count() >= prev.lines().count())
                    .unwrap_or(true);
                if keep {
                    trees.insert(id, dump);
                }
            }
        }
    }

    /// Current overall p90 latency in microseconds (used as slow-op capture threshold).
    pub fn current_p90_us(&self) -> u64 {
        let a = self.agg();
        if a.all_latency.is_empty() {
            0
        } else {
            a.all_latency.value_at_quantile(0.90)
        }
    }

    pub fn update_client(&self, cpu_pct: f64, rss_mib: f64) {
        let mut c = self.client.lock().unwrap_or_else(|e| e.into_inner());
        c.cpu_pct = if c.cpu_pct == 0.0 {
            cpu_pct
        } else {
            c.cpu_pct * 0.7 + cpu_pct * 0.3
        };
        if self.is_measuring() {
            c.cpu_pct_peak = c.cpu_pct_peak.max(cpu_pct);
        }
        c.rss_mib = rss_mib;
        drop(c);
        if self.window_open() {
            let (mut a, window_start) = self.agg_at();
            let idx = bucket_index(window_start.elapsed().as_secs());
            self.ensure_bucket(&mut a, idx, window_start);
            a.bucket(idx).cpu_pct = Some(cpu_pct);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_with(false, None)
    }

    /// The aggregates so far, and a fresh window from now on (design v7 P2): every aggregate
    /// starts again from zero, atomically with the snapshot, so successive snapshots cover
    /// disjoint windows. Operations in flight are counted in the window they complete in.
    pub fn snapshot_and_reset(&self) -> Snapshot {
        self.snapshot_with(true, None)
    }

    /// [`Recorder::snapshot_and_reset`] with the trace rows of the same window
    /// ([`Recorder::take_trace`]): taken in the same critical section, so every row's operation
    /// is in the snapshot's aggregates, and every operation in the aggregates whose row was
    /// kept has its row here.
    pub fn snapshot_and_reset_with_rows(&self) -> (Snapshot, TraceRows) {
        let mut rows = TraceRows::default();
        let snap = self.snapshot_with(true, Some(&mut rows));
        (snap, rows)
    }

    fn snapshot_with(&self, reset: bool, rows: Option<&mut TraceRows>) -> Snapshot {
        let elapsed = self.elapsed_s().max(1e-3);
        let mut a = self.agg();
        let packed = self.interval_peak.load(Ordering::Acquire);
        self.fold_interval_peak(&mut a, packed, self.window_start());
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);

        let verbs: Vec<VerbStats> = a
            .verbs
            .iter()
            .map(|(name, v)| {
                let n = v.count.max(1) as f64;
                VerbStats {
                    verb: (*name).to_owned(),
                    count: v.count,
                    errors: v.errors,
                    throttled: v.throttled,
                    ops_per_s: v.count as f64 / elapsed,
                    mib_per_s: mib(v.bytes) / elapsed,
                    latency: quantiles(&v.latency),
                    breakdown: OpBreakdown {
                        queue_ms: v.queue_us as f64 / n / 1000.0,
                        cache_ms: v.cache_us as f64 / n / 1000.0,
                        backend_ms: v.backend_us as f64 / n / 1000.0,
                        retry_ms: v.retry_us as f64 / n / 1000.0,
                        client_ms: v.client_us as f64 / n / 1000.0,
                    },
                    requests_per_op: v.requests as f64 / n,
                    bytes_per_op: v.bytes as f64 / n,
                    retries: v.retries,
                }
            })
            .collect();

        let apis: Vec<ApiStats> = a
            .apis
            .iter()
            .map(|(api, s)| ApiStats {
                api: *api,
                count: s.count,
                errors: s.errors,
                throttled: s.throttled,
                status: s.status.clone(),
                reused_pct: crate::util::pct(s.reused as f64, s.connected as f64),
                bytes_up: s.bytes_up,
                bytes_down: s.bytes_down,
                stages: StageStats {
                    queue: quantiles(&s.stages.queue),
                    connect: quantiles(&s.stages.connect),
                    send: quantiles(&s.stages.send),
                    ttfb: quantiles(&s.stages.ttfb),
                    body: quantiles(&s.stages.body),
                    stall: quantiles(&s.stages.stall),
                    total: quantiles(&s.stages.total),
                },
            })
            .collect();

        let connections = ConnStats {
            dialed: a.conn.dialed,
            dial_failures: a.conn.failures,
            requests: a.requests,
            reused_requests: a.reused_requests,
            reuse_pct: crate::util::pct(
                a.reused_requests as f64,
                (a.h1_requests + a.h2_requests) as f64,
            ),
            http1_requests: a.h1_requests,
            http2_requests: a.h2_requests,
            alpn: a.conn.alpn.clone(),
            tls_version: a.conn.tls_version.clone(),
            cipher: a.conn.cipher.clone(),
            proxy: self.proxy.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            remote_addrs: a.conn.remote_addrs.clone(),
            dns: quantiles(&a.dns),
            tcp: quantiles(&a.tcp),
            proxy_connect: quantiles(&a.proxy),
            tls: quantiles(&a.tls),
            peak_inflight_requests: self.peak_inflight_requests.load(Ordering::Relaxed),
            cold_start: {
                let g = self.cold.lock().unwrap_or_else(|e| e.into_inner());
                (!g.0.is_empty()).then(|| ColdStartStats {
                    connections_count: g.0.len(),
                    connect: quantiles(&g.0),
                    first_ops: quantiles(&g.1),
                })
            },
        };

        let mut resilience = a.res.clone();
        resilience.failed_ops = a.failed_ops;

        let mut bytes = a.bytes.clone();
        bytes.logical_read = a.logical_read;
        bytes.logical_write = a.logical_write;
        bytes.wire_rx = self.wire_rx.load(Ordering::Relaxed);
        bytes.wire_tx = self.wire_tx.load(Ordering::Relaxed);
        bytes.read_amplification =
            crate::util::ratio(bytes.backend_down as f64, bytes.logical_read as f64);
        let uncached = bytes.logical_read.saturating_sub(bytes.cache_hit_bytes);
        bytes.overread_ratio = crate::util::ratio(bytes.backend_down as f64, uncached as f64);
        bytes.write_amplification =
            crate::util::ratio(bytes.backend_up as f64, bytes.logical_write as f64);
        bytes.request_amplification = crate::util::ratio(a.requests as f64, a.ops as f64);

        let cache = CacheStats {
            enabled: self.cache_enabled.load(Ordering::Relaxed),
            lookups: a.cache_lookups,
            hits: a.cache_hits,
            hit_ratio: crate::util::ratio(a.cache_hits as f64, a.cache_lookups as f64),
            lookup_ms_mean: crate::util::ratio(a.cache_lookup_us as f64, a.cache_lookups as f64)
                / 1000.0,
        };

        // The last interval is usually partial; scale its rates by its covered fraction.
        let nb = a.buckets.len();
        let ms = |v: u64| v as f64 / 1000.0;
        let archived = a.archive.iter().map(|x| (x.t_s, x.len_s, false, &x.b));
        let live = a.buckets.iter().enumerate().map(|(i, b)| {
            let (t_s, len_s) = bucket_span(a.base + i);
            (t_s, len_s, i + 1 == nb, b)
        });
        let timeline = archived
            .chain(live)
            .map(|(t_s, len_s, last, b)| {
                let width = if last {
                    (elapsed - t_s).clamp(0.05, len_s)
                } else {
                    len_s
                };
                let q = b.quantiles();
                TimelinePoint {
                    t_s,
                    ops_per_s: b.ops as f64 / width,
                    mib_per_s: mib(b.bytes) / width,
                    p50_ms: ms(q[0]),
                    p99_ms: ms(q[2]),
                    errors: b.errors,
                    throttled: b.throttled,
                    missed_arrivals: b.missed,
                    len_s,
                    p90_ms: ms(q[1]),
                    p999_ms: ms(q[3]),
                    max_ms: ms(q[4]),
                    inflight_peak: b.inflight_peak,
                    new_connections: b.dials,
                    cpu_pct: b.cpu_pct,
                    latency_rows_count: if b.ops > 0 {
                        b.heat.iter().map(|c| u64::from(*c)).collect()
                    } else {
                        Vec::new()
                    },
                    ops_count: b.ops,
                    ok_ops_count: b.ok_ops,
                    goodput_ops_per_s: b.ok_ops as f64 / width,
                    throttled_ops: b.throttled_ops,
                    requests_count: b.requests,
                    requests_per_s: b.requests as f64 / width,
                    retries_count: b.retries,
                    verbs: b
                        .verbs
                        .iter()
                        .map(|v| VerbInterval {
                            verb: v.verb.to_owned(),
                            ops_count: v.ops,
                            ok_ops_count: v.ok,
                            errors_count: v.errors,
                            throttled_count: v.throttled,
                            latency_sum_ms: ms(v.latency_us),
                            latency_rows_count: if v.ops > 0 {
                                v.heat.iter().map(|c| u64::from(*c)).collect()
                            } else {
                                Vec::new()
                            },
                        })
                        .collect(),
                }
            })
            .collect();
        // Only a closed window has an end to fold a sliver into.
        let closed = self.frozen_elapsed_us.load(Ordering::Acquire) != WINDOW_OPEN;
        let timeline = if closed {
            fold_end_sliver(timeline, elapsed)
        } else {
            timeline
        };

        let throttled_ops: u64 = verbs.iter().map(|v| v.throttled).sum();
        let summary = Summary {
            ops: a.ops,
            ops_per_s: a.ops.saturating_sub(a.failed_ops) as f64 / elapsed,
            attempted_ops_per_s: a.ops as f64 / elapsed,
            failed_ops: a.failed_ops,
            error_rate: crate::util::ratio(a.failed_ops as f64, a.ops as f64),
            throttled_request_ratio: crate::util::ratio(
                a.res.throttled_responses as f64,
                a.requests as f64,
            ),
            throttled_op_ratio: crate::util::ratio(throttled_ops as f64, a.ops as f64),
            mib_per_s: mib(a.logical_read + a.logical_write) / elapsed,
            read_mib_per_s: mib(a.logical_read) / elapsed,
            write_mib_per_s: mib(a.logical_write) / elapsed,
            latency: quantiles(&a.all_latency),
            inflight_ops: self.inflight_ops.load(Ordering::Relaxed),
        };

        let snap = Snapshot {
            phase: self.phase(),
            elapsed_s: elapsed,
            summary,
            verbs,
            apis,
            connections,
            resilience,
            bytes,
            cache,
            integrity: a.integrity.clone(),
            client: self
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            open_loop: a.open_loop.clone().map(|mut o| {
                o.censored_count = a.censored;
                if !a.schedule.is_empty() {
                    o.schedule = Some(quantiles(&a.schedule));
                }
                o
            }),
            interrupted: false,
            effective_knobs: EffectiveKnobs::default(),
            timeline,
            slow_ops: a.slow.clone(),
            self_obs: None,
            dimensions: Some(a.dims.stats()).filter(|d| !d.prefixes.is_empty()),
            latency_histogram: LatencyHistogram::from_counts(
                a.all_latency
                    .iter_recorded()
                    .map(|v| (v.value_iterated_to(), v.count_at_value())),
            ),
        };
        if let Some(rows) = rows {
            // Under the aggregate lock: a completion counted above holds its trace shard until
            // its row is in (see `complete_op_inner`), so taking every shard waits for it.
            *rows = self.take_trace();
        }
        if reset {
            // Under the aggregate lock: nothing recorded between the snapshot and the reset is
            // lost. The connection details (ALPN, TLS, remotes) describe the pool, which lives on.
            let conn = std::mem::take(&mut a.conn);
            *a = Agg::new();
            a.conn = ConnInfo {
                dialed: 0,
                failures: 0,
                ..conn
            };
            self.interval_peak.store(0, Ordering::Release);
            *self.window_start.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            self.wire_rx.store(0, Ordering::Relaxed);
            self.wire_tx.store(0, Ordering::Relaxed);
            self.peak_inflight_requests.store(
                self.inflight_requests.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        snap
    }
}

/// `at - origin` in µs, negative when `at` is earlier.
fn signed_offset_us(origin: Instant, at: Instant) -> i64 {
    match at.checked_duration_since(origin) {
        Some(d) => d.as_micros() as i64,
        None => -(origin.duration_since(at).as_micros() as i64),
    }
}

/// Exact split of one op's wall time into `[queue, cache, backend, retry, client]`
/// microseconds using a sweep over its request intervals and retry backoff sleeps: an instant
/// counts as *backend* if any request is on the network, else *queue* if any request is
/// waiting for a permit/connection, else *retry* if the retry layer is sleeping before the
/// next attempt, else *client*. Cache lookup time is carved out of the client share.
pub fn breakdown_us(d: &OpScopeData, total_us: u64) -> [u64; 5] {
    // events: (time, queue delta, network delta, retry delta)
    let mut ev: Vec<(u64, i32, i32, i32)> =
        Vec::with_capacity(d.requests.len() * 4 + d.retry_waits.len() * 2);
    for r in &d.requests {
        let s = r.start_offset_us;
        let q_end = s + r.timing.queue_us;
        let n_end = (s + r.timing.total_us)
            .saturating_sub(r.timing.stall_us)
            .max(q_end);
        if r.timing.queue_us > 0 {
            ev.push((s, 1, 0, 0));
            ev.push((q_end, -1, 0, 0));
        }
        if n_end > q_end {
            ev.push((q_end, 0, 1, 0));
            ev.push((n_end, 0, -1, 0));
        }
    }
    for &(s, e) in &d.retry_waits {
        if e > s {
            ev.push((s, 0, 0, 1));
            ev.push((e, 0, 0, -1));
        }
    }
    // Ends before starts at the same instant.
    ev.sort_by_key(|e| (e.0, e.1.min(e.2).min(e.3)));
    let (mut q, mut n, mut w) = (0i32, 0i32, 0i32);
    let (mut queue, mut net, mut retry) = (0u64, 0u64, 0u64);
    let mut last = 0u64;
    for (t, dq, dn, dw) in ev {
        let t = t.min(total_us);
        if t > last {
            if n > 0 {
                net += t - last;
            } else if q > 0 {
                queue += t - last;
            } else if w > 0 {
                retry += t - last;
            }
            last = t;
        }
        q += dq;
        n += dn;
        w += dw;
    }
    let rest = total_us.saturating_sub(queue + net + retry);
    let cache = d.cache_lookup_us.min(rest);
    [queue, cache, net, retry, rest - cache]
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::*;

    /// A month of traffic in an always-on recorder (one operation every 30 s): the timeline
    /// stays bounded and keeps every operation, in contiguous intervals (design v7 P2).
    #[test]
    fn a_month_long_timeline_stays_bounded() {
        let rec = Recorder::always_on(0);
        let start = rec.window_start();
        let month_s: u64 = 30 * 86_400;
        let mut n = 0u64;
        for s in (0..month_s).step_by(30) {
            rec.complete_op(
                true,
                OpCompletion {
                    verb: "stat",
                    op_id: n + 1,
                    started: start + std::time::Duration::from_secs(s),
                    latency_us: 1_000 + (s % 7) * 100,
                    logical_read: 0,
                    logical_write: 0,
                    error: None,
                    key: String::new(),
                    data: Default::default(),
                    schedule_us: None,
                },
            );
            n += 1;
            let a = rec.agg();
            assert!(
                a.archive.len() + a.buckets.len() <= MAX_INTERVALS,
                "at {s} s"
            );
        }
        let t = rec.snapshot().timeline;
        assert!(t.len() <= MAX_INTERVALS, "{}", t.len());
        assert!(t.len() > MAX_INTERVALS / 2, "{}", t.len());
        assert_eq!(t.iter().map(|p| p.ops_count).sum::<u64>(), n);
        for w in t.windows(2) {
            assert!(
                (w[0].t_s + w[0].len_s - w[1].t_s).abs() < 1e-6,
                "{} + {} vs {}",
                w[0].t_s,
                w[0].len_s,
                w[1].t_s
            );
        }
        // The oldest intervals are the coarsest; the recent ones keep their resolution.
        assert!(t[0].len_s > t[t.len() - 1].len_s);
        // Merged quantiles stay within the operations' latencies (1.0 to 1.6 ms).
        assert!(
            t.iter()
                .filter(|p| p.ops_count > 0)
                .all(|p| p.p50_ms >= 1.0 && p.max_ms <= 1.7)
        );
        assert_eq!(t[t.len() - 1].len_s, COARSE_S as f64);
    }

    /// One operation left running (a writer open for days, a stream never drained) does not
    /// keep every later interval open: the timeline stays bounded, and the operation lands in
    /// the timeline when it finally completes.
    #[test]
    fn a_long_lived_operation_does_not_hold_the_timeline_open() {
        let rec = Recorder::always_on(0);
        let start = rec.window_start();
        let op = |id: u64, s: u64, latency_us: u64| OpCompletion {
            verb: "stat",
            op_id: id,
            started: start + std::time::Duration::from_secs(s),
            latency_us,
            logical_read: 0,
            logical_write: 0,
            error: None,
            key: String::new(),
            data: Default::default(),
            schedule_us: None,
        };
        // The long-lived operation starts at second 5 and stays open.
        rec.op_started_at(u64::MAX, start + std::time::Duration::from_secs(5));
        let span_s: u64 = 10 * 86_400;
        let mut n = 0u64;
        for s in (0..span_s).step_by(10) {
            n += 1;
            rec.op_started_at(n, start + std::time::Duration::from_secs(s));
            rec.complete_op(true, op(n, s, 1_000));
            let a = rec.agg();
            assert!(
                a.archive.len() + a.buckets.len() <= MAX_INTERVALS,
                "at {s} s: {} archived + {} live",
                a.archive.len(),
                a.buckets.len()
            );
        }
        // It completes at last: counted once, in the oldest interval the timeline still holds.
        rec.complete_op(true, op(u64::MAX, 5, (span_s - 5) * 1_000_000));
        let snap = rec.snapshot();
        let t = &snap.timeline;
        assert!(t.len() <= MAX_INTERVALS, "{}", t.len());
        assert_eq!(t.iter().map(|p| p.ops_count).sum::<u64>(), n + 1);
        assert_eq!(snap.summary.ops, n + 1);
        assert!(t.iter().any(|p| p.max_ms > 1_000.0));
    }

    /// An embedded operator idle for 30 days wakes up: the recorder neither allocates one
    /// interval (and histogram) per 10 s of silence nor keeps them, and the timeline still
    /// covers the gap contiguously (9.3.2: about 260 000 histograms, ~47 GiB).
    #[test]
    fn a_thirty_day_idle_gap_costs_constant_memory() {
        fn histograms(a: &Agg) -> usize {
            a.buckets.iter().filter(|b| b.latency.is_some()).count()
                + a.archive.iter().filter(|x| x.b.latency.is_some()).count()
        }
        for held in [false, true] {
            let rec = Recorder::always_on(0);
            let start = rec.window_start();
            let at = |s: u64| start + std::time::Duration::from_secs(s);
            let op = |id: u64, s: u64| OpCompletion {
                verb: "stat",
                op_id: id,
                started: at(s),
                latency_us: 2_000,
                logical_read: 0,
                logical_write: 0,
                error: None,
                key: String::new(),
                data: Default::default(),
                schedule_us: None,
            };
            if held {
                // A writer left open across the whole gap.
                rec.op_started_at(u64::MAX, at(3));
            }
            rec.op_started_at(1, at(1));
            rec.complete_op(true, op(1, 1));
            let month_s: u64 = 30 * 86_400;
            rec.op_started_at(2, at(month_s));
            rec.complete_op(true, op(2, month_s));
            {
                let a = rec.agg();
                let n = a.archive.len() + a.buckets.len();
                assert!(n <= MAX_OPEN_INTERVALS + 8, "{n} intervals after the gap");
                assert!(histograms(&a) <= 2, "{} histograms", histograms(&a));
            }
            rec.op_started_at(3, at(month_s + 20));
            rec.complete_op(true, op(3, month_s + 20));
            let t = rec.snapshot().timeline;
            assert_eq!(t.iter().map(|p| p.ops_count).sum::<u64>(), 3);
            for w in t.windows(2) {
                assert!((w[0].t_s + w[0].len_s - w[1].t_s).abs() < 1e-6);
            }
            let last = t.last().unwrap();
            assert!(last.t_s + last.len_s >= (month_s + 20) as f64);
        }
    }

    /// T1: completions racing `snapshot_and_reset` land in the new window's intervals. 9.3.3
    /// read the window start before taking the aggregate lock, so an operation placed against
    /// the old start went into the new window: a window a few ms old held intervals up to
    /// 3 600 s, and almost every later operation landed in one of them.
    #[test]
    fn a_reset_racing_completions_keeps_the_new_timeline() {
        let rec = Recorder::always_on(10_000_000);
        let stop = Arc::new(AtomicBool::new(false));
        let next = Arc::new(AtomicU64::new(1));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (rec, stop, next) = (rec.clone(), stop.clone(), next.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let id = next.fetch_add(1, Ordering::Relaxed);
                        rec.op_started(id);
                        rec.complete_op(
                            true,
                            OpCompletion {
                                verb: "stat",
                                op_id: id,
                                started: Instant::now(),
                                latency_us: 10,
                                logical_read: 0,
                                logical_write: 0,
                                error: None,
                                key: String::new(),
                                data: Default::default(),
                                schedule_us: None,
                            },
                        );
                    }
                })
            })
            .collect();
        // Each window is over a second old when it is reset, so an operation placed against
        // its start lands a whole interval (or more) into the fresh window.
        let mut bad = Vec::new();
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            rec.snapshot_and_reset();
            std::thread::sleep(std::time::Duration::from_millis(20));
            let snap = rec.snapshot();
            let last = snap.timeline.iter().map(|p| p.t_s).fold(0.0, f64::max);
            if snap.timeline.len() > 1 || last >= 1.0 {
                bad.push((snap.elapsed_s, snap.timeline.len(), last));
            }
        }
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        assert!(
            bad.is_empty(),
            "fresh windows (age s, intervals, last interval s): {bad:?}"
        );
    }

    /// Windows saved while operations complete on other threads: each window's rows are the
    /// rows of the operations in its aggregates, never one window's aggregates with the next
    /// window's rows.
    #[test]
    fn a_reset_window_keeps_its_rows_with_its_aggregates() {
        let rec = Recorder::always_on(10_000_000);
        let stop = Arc::new(AtomicBool::new(false));
        let next = Arc::new(AtomicU64::new(1));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (rec, stop, next) = (rec.clone(), stop.clone(), next.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let id = next.fetch_add(1, Ordering::Relaxed);
                        rec.op_started(id);
                        rec.complete_op(
                            true,
                            OpCompletion {
                                verb: "stat",
                                op_id: id,
                                started: Instant::now(),
                                latency_us: 10,
                                logical_read: 0,
                                logical_write: 0,
                                error: None,
                                key: String::new(),
                                data: Default::default(),
                                schedule_us: None,
                            },
                        );
                    }
                })
            })
            .collect();
        let (mut ops, mut rows) = (0u64, 0u64);
        for _ in 0..5000 {
            let (snap, r) = rec.snapshot_and_reset_with_rows();
            assert_eq!(
                r.ops.len() as u64,
                snap.summary.ops,
                "a window's rows and aggregates disagree"
            );
            ops += snap.summary.ops;
            rows += r.ops.len() as u64;
        }
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        let (snap, r) = rec.snapshot_and_reset_with_rows();
        assert_eq!(r.ops.len() as u64, snap.summary.ops);
        assert!(ops > 0);
        assert_eq!(ops + snap.summary.ops, rows + r.ops.len() as u64);
    }

    fn req(start: u64, queue: u64, total: u64) -> RequestRecord {
        RequestRecord {
            operation: None,
            api: Api::GetObject,
            method: "GET".into(),
            status: 200,
            protocol: Protocol::Http1,
            reused: true,
            conn_id: 1,
            start_offset_us: start,
            timing: RequestTiming {
                queue_us: queue,
                total_us: total,
                ..Default::default()
            },
            bytes_up: 0,
            bytes_down: 0,
            request_id: None,
            retry_after_ms: None,
            error: None,
            remote: None,
            server: None,
            service_op: None,
            service: None,
            tls: None,
            tcp: None,
            detail: None,
        }
    }

    #[test]
    fn breakdown_merges_parallel_requests() {
        let d = OpScopeData {
            requests: vec![req(100, 0, 1000), req(500, 200, 1000)],
            cache_lookup_us: 50,
            ..Default::default()
        };
        // network covers [100,1500) = 1400us; queue [500,700) overlaps network -> 0
        let b = breakdown_us(&d, 2000);
        assert_eq!(b, [0, 50, 1400, 0, 550]);
        assert_eq!(b.iter().sum::<u64>(), 2000);
    }

    #[test]
    fn closed_window_freezes_rates_but_records_draining_ops() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let start = r.window_start();
        assert!(r.window_open());
        r.close_window(start + std::time::Duration::from_millis(500));
        assert!(!r.window_open());
        assert!(r.is_measuring());
        // A second close (e.g. another worker) does not move the end.
        r.close_window(start + std::time::Duration::from_secs(5));
        assert!((r.elapsed_s() - 0.5).abs() < 1e-6, "{}", r.elapsed_s());
        // An op that started inside the window completes during the drain.
        r.op_started_at(7, start);
        r.complete_op(
            true,
            OpCompletion {
                verb: "stat",
                op_id: 7,
                started: start,
                latency_us: 900_000,
                logical_read: 0,
                logical_write: 0,
                error: None,
                key: String::new(),
                data: OpScopeData::default(),
                schedule_us: None,
            },
        );
        r.set_phase(Phase::Verify);
        let s = r.snapshot();
        assert_eq!(s.summary.ops, 1);
        assert!((s.elapsed_s - 0.5).abs() < 1e-6);
        assert!((s.summary.ops_per_s - 2.0).abs() < 1e-6);
    }

    fn completion(
        r: &Recorder,
        id: u64,
        started: Instant,
        latency_us: u64,
        error: Option<&str>,
        data: OpScopeData,
    ) {
        r.op_started_at(id, started);
        r.complete_op(
            true,
            OpCompletion {
                verb: if id.is_multiple_of(2) { "read" } else { "stat" },
                op_id: id,
                started,
                latency_us,
                logical_read: 0,
                logical_write: 0,
                error: error.map(str::to_owned),
                key: String::new(),
                data,
                schedule_us: None,
            },
        );
    }

    /// The interval's in-flight peak is kept off the lock; the last interval's is folded when
    /// measurement ends, before the final snapshot (which runs after `Phase::Verify`).
    #[test]
    fn the_last_intervals_inflight_peak_survives_the_end_of_measurement() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let t0 = r.window_start();
        for _ in 0..3 {
            r.request_started();
        }
        for _ in 0..3 {
            r.request_finished();
        }
        let data = OpScopeData {
            requests: vec![req(0, 0, 1000)],
            ..Default::default()
        };
        completion(&r, 1, t0, 1_000, None, data);
        r.set_phase(Phase::Verify);
        let s = r.snapshot();
        assert_eq!(s.timeline[0].inflight_peak, 3, "{:#?}", s.timeline);
    }

    #[test]
    fn events_land_in_the_interval_they_happened_in() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let t0 = r.window_start();
        // Started at 0 s: a throttled attempt answered at +2.5 s, a retry issued at +3.2 s,
        // the second attempt sent then and the operation failing at +7.4 s.
        let mut thr = req(2_000_000, 0, 500_000);
        thr.status = 503;
        let mut last = req(3_200_000, 0, 4_200_000);
        last.status = 500;
        let data = OpScopeData {
            requests: vec![req(0, 0, 1000), thr, last],
            retries: 1,
            retry_waits: vec![(2_500_000, 3_200_000)],
            throttled: true,
            ..Default::default()
        };
        completion(&r, 1, t0, 7_400_000, Some("server_error"), data);
        let s = r.snapshot();
        let tl = &s.timeline;
        assert_eq!(tl.len(), 8, "{tl:#?}");
        // The operation and its latency belong to its start.
        assert_eq!(tl[0].ops_count, 1);
        assert_eq!(tl[0].throttled_ops, 1);
        assert!((tl[0].p99_ms - 7400.0).abs() < 10.0);
        // Requests when sent, the throttled response when it arrived, the retry when issued,
        // the failure when the operation ended.
        let per = |f: fn(&TimelinePoint) -> u64| tl.iter().map(f).collect::<Vec<_>>();
        assert_eq!(per(|p| p.requests_count), [1, 0, 1, 1, 0, 0, 0, 0]);
        assert_eq!(per(|p| p.throttled), [0, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(per(|p| p.retries_count), [0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(per(|p| p.errors), [0, 0, 0, 0, 0, 0, 0, 1]);
        // The verb's own counters follow the same rules.
        let verb = |i: usize| tl[i].verbs.iter().find(|v| v.verb == "stat").cloned();
        assert_eq!(verb(0).unwrap().ops_count, 1);
        assert_eq!(verb(2).unwrap().throttled_count, 1);
        assert_eq!(verb(7).unwrap().errors_count, 1);
    }

    #[test]
    fn goodput_excludes_failures_and_throttled_counts_responses() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let t0 = r.window_start();
        for id in 0..10u64 {
            // Three failures; every operation's first attempt is throttled twice over.
            let mut a = req(0, 0, 100);
            a.status = 429;
            let mut b = req(200, 0, 100);
            b.status = 503;
            let data = OpScopeData {
                requests: vec![a, b, req(400, 0, 100)],
                retries: 2,
                throttled: true,
                ..Default::default()
            };
            let err = (id < 3).then_some("rate_limited");
            completion(&r, id + 1, t0, 600, err, data);
        }
        r.close_window(t0 + std::time::Duration::from_secs(1));
        let s = r.snapshot();
        let p = &s.timeline[0];
        assert_eq!(p.ops_count, 10);
        assert_eq!(p.ok_ops_count, 7);
        assert!((p.ops_per_s - 10.0).abs() < 1e-9, "{}", p.ops_per_s);
        assert!((p.goodput_ops_per_s - 7.0).abs() < 1e-9);
        assert_eq!(p.errors, 3);
        // 20 throttled responses from 10 throttled operations.
        assert_eq!(p.throttled, 20);
        assert_eq!(p.throttled_ops, 10);
        assert_eq!(p.requests_count, 30);
        assert!((p.requests_per_s - 30.0).abs() < 1e-9);
        // Retries without backoff intervals count when the operation ended.
        assert_eq!(p.retries_count, 20);
        assert_eq!(s.resilience.throttled_responses, 20);
        // The verbs add up to the interval.
        let ops: u64 = p.verbs.iter().map(|v| v.ops_count).sum();
        let ok: u64 = p.verbs.iter().map(|v| v.ok_ops_count).sum();
        assert_eq!((ops, ok), (10, 7));
    }

    #[test]
    fn a_sliver_after_the_window_folds_into_the_last_interval() {
        let pt = |t_s: f64, ops: u64, requests: u64, throttled: u64| TimelinePoint {
            t_s,
            len_s: 1.0,
            ops_count: ops,
            requests_count: requests,
            requests_per_s: requests as f64,
            throttled,
            ..Default::default()
        };
        // `-d 2s` closed at 2.00008 s: the third interval holds draining requests only.
        let t = fold_end_sliver(
            vec![pt(0.0, 10, 10, 0), pt(1.0, 10, 10, 1), pt(2.0, 0, 3, 2)],
            2.00008,
        );
        assert_eq!(t.len(), 2);
        assert_eq!((t[1].requests_count, t[1].throttled), (13, 3));
        assert_eq!(t[1].requests_per_s, 13.0);
        // A window closed at 1.98 s with retries draining for two more seconds: both drain
        // intervals fold into the window's last one.
        let t = fold_end_sliver(
            vec![
                pt(0.0, 10, 10, 0),
                pt(1.0, 10, 10, 0),
                pt(2.0, 0, 4, 1),
                pt(3.0, 0, 2, 1),
            ],
            1.98,
        );
        assert_eq!(t.len(), 2);
        assert_eq!((t[1].requests_count, t[1].throttled), (16, 2));
        // A real last interval (operations started in it, or most of it inside the window) stays.
        assert_eq!(
            fold_end_sliver(vec![pt(0.0, 10, 10, 0), pt(1.0, 2, 3, 0)], 1.00008).len(),
            2
        );
        assert_eq!(
            fold_end_sliver(vec![pt(0.0, 10, 10, 0), pt(1.0, 0, 3, 0)], 1.5).len(),
            2
        );
    }

    #[test]
    fn draining_operations_stay_in_the_window() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let t0 = r.window_start();
        r.close_window(t0 + std::time::Duration::from_millis(2500));
        // Started at 2.4 s, failing at 5.4 s, after the window closed.
        completion(
            &r,
            3,
            t0 + std::time::Duration::from_millis(2400),
            3_000_000,
            Some("timeout"),
            OpScopeData::default(),
        );
        let s = r.snapshot();
        assert_eq!(s.timeline.len(), 3);
        assert_eq!(s.timeline[2].errors, 1);
        assert_eq!(s.timeline[2].ops_count, 1);
    }

    /// An operation's error text is captured without the credentials of a URL in it.
    #[test]
    fn a_failed_operations_error_never_keeps_a_urls_credentials() {
        let r = Recorder::always_on(16);
        let error = "read https://b.example/k?sv=2024&sig=SASSECRET: connection reset";
        completion(&r, 1, Instant::now(), 1000, Some(error), OpScopeData::default());
        let rows = r.take_trace();
        assert_eq!(
            rows.ops[0].error.as_deref(),
            Some("read https://b.example/k?sv=2024: connection reset")
        );
        assert!(!format!("{:?}{rows:?}", r.snapshot()).contains("SASSECRET"));
    }

    #[test]
    fn the_latency_histogram_holds_every_operation() {
        let r = Recorder::new();
        r.set_phase(Phase::Measure);
        let t0 = r.window_start();
        // 900 operations at 1 ms, 100 at 10 ms.
        for id in 0..1000u64 {
            let us = if id < 900 { 1000 } else { 10_000 };
            completion(&r, id + 1, t0, us, None, OpScopeData::default());
        }
        let s = r.snapshot();
        let h = &s.latency_histogram;
        assert_eq!(h.total(), 1000);
        assert_eq!(h.counts, vec![900, 100]);
        assert!(h.lower_ms[0] <= 1.0 && 1.0 < h.upper_ms[0], "{h:?}");
        assert!(h.lower_ms[1] <= 10.0 && 10.0 < h.upper_ms[1], "{h:?}");
        assert_eq!(h.quantile_ms(0.5), Some(h.upper_ms[0]));
        assert!((h.quantile_ms(0.99).unwrap() - 10.0).abs() < 0.1);
        assert!((h.max_ms - 10.0).abs() < 0.05);
        let back: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.latency_histogram, s.latency_histogram);
    }

    #[test]
    fn breakdown_counts_pure_queue_time() {
        let d = OpScopeData {
            requests: vec![req(0, 300, 1000)],
            ..Default::default()
        };
        assert_eq!(breakdown_us(&d, 1000), [300, 0, 700, 0, 0]);
    }

    #[test]
    fn breakdown_attributes_retry_backoff_to_retry_not_client() {
        // Attempt 1 [0, 1000) is throttled, the retry layer sleeps [1100, 5100), attempt 2
        // runs [5150, 6150); 50us of client time before the retry and 50us after the sleep.
        let mut ok = req(5150, 0, 1000);
        ok.status = 200;
        let mut throttled = req(0, 0, 1000);
        throttled.status = 503;
        let d = OpScopeData {
            requests: vec![throttled, ok],
            retries: 1,
            retry_backoff_us: 4000,
            retry_waits: vec![(1100, 5100)],
            ..Default::default()
        };
        let b = breakdown_us(&d, 6200);
        assert_eq!(b, [0, 0, 2000, 4000, 200]);
        // A backoff overlapping another request's network time is backend time.
        let d = OpScopeData {
            requests: vec![req(0, 0, 3000)],
            retry_waits: vec![(1000, 4000)],
            ..Default::default()
        };
        assert_eq!(breakdown_us(&d, 4000), [0, 0, 3000, 1000, 0]);
    }

    #[test]
    fn scope_records_retry_waits() {
        let s = crate::OpScope::new(true);
        s.record_retry(std::time::Duration::from_millis(5), true);
        let d = s.take();
        assert_eq!(d.retries, 1);
        assert_eq!(d.retry_backoff_us, 5000);
        assert_eq!(d.retry_waits.len(), 1);
        assert_eq!(d.retry_waits[0].1 - d.retry_waits[0].0, 5000);
        assert!(d.throttled);
    }

    /// A request answered on a connection nobody observed (a transport the layer only wraps:
    /// `conn_id` 0) is neither warm nor a dial: reuse is counted over the requests whose
    /// connection is known (9.2 counted every answered request as warm, so a kept transport
    /// read "100 % reused").
    #[test]
    fn requests_on_unobserved_connections_are_neither_warm_nor_dials() {
        let answered = |conn_id: u64| RequestRecord {
            conn_id,
            status: 200,
            ..req(0, 0, 1000)
        };
        let kept = Recorder::always_on(8);
        for _ in 0..3 {
            kept.record_unscoped_request(&answered(0));
        }
        let s = kept.snapshot();
        let c = &s.connections;
        assert_eq!((c.requests, c.http1_requests), (3, 3), "{c:?}");
        assert_eq!((c.reused_requests, c.reuse_pct, c.dialed), (0, 0.0, 0), "{c:?}");
        assert!(!c.observed(), "{c:?}");
        assert_eq!(s.apis[0].reused_pct, 0.0);

        let own = Recorder::always_on(8);
        own.record_unscoped_request(&RequestRecord {
            reused: false,
            ..answered(1)
        });
        for _ in 0..3 {
            own.record_unscoped_request(&answered(1));
        }
        let s = own.snapshot();
        let c = &s.connections;
        assert_eq!((c.reused_requests, c.reuse_pct, c.dialed), (3, 75.0, 1), "{c:?}");
        assert!(c.observed(), "{c:?}");
        assert_eq!(s.apis[0].reused_pct, 75.0);
        // Nothing over HTTP: nothing to observe.
        assert!(Recorder::always_on(8).snapshot().connections.observed());
    }

    /// Every connection the client dials is counted once, in the phase it was dialed in: one
    /// that no request waited for (hyper races a dial against a connection coming idle) is
    /// one of the cold start's during the warm-up, as it is one of the window's during the
    /// window (until 9.2 the warm-up dropped it: a profile showed 24 connections in use and
    /// "22 dialed during warm-up · 0 in the window").
    #[test]
    fn a_warm_up_dial_nobody_waited_for_is_one_of_the_cold_starts() {
        let r = Recorder::new();
        r.set_phase(Phase::Warmup);
        let mut dialing = req(0, 0, 1000);
        dialing.reused = false;
        dialing.status = 200;
        dialing.timing.tcp_us = 400;
        r.op_started(1);
        r.complete_op(
            false,
            OpCompletion {
                verb: "read",
                op_id: 1,
                started: Instant::now(),
                latency_us: 1000,
                logical_read: 0,
                logical_write: 0,
                error: None,
                key: String::new(),
                data: OpScopeData {
                    requests: vec![dialing],
                    ..Default::default()
                },
                schedule_us: None,
            },
        );
        r.record_unclaimed_dial(0, 300, 0, 0);
        r.set_phase(Phase::Measure);
        r.record_unclaimed_dial(0, 200, 0, 0);
        let c = r.snapshot().connections;
        let cold = c.cold_start.as_ref().expect("the warm-up dialed");
        assert_eq!(cold.connections_count, 2, "{cold:?}");
        // The operations that waited for a dial: one.
        assert_eq!(cold.first_ops.count, 1, "{cold:?}");
        assert_eq!(c.dialed, 1, "{c:?}");
    }
}
