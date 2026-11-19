//! Thread-safe aggregation of trace records into histograms, timelines and slow-op samples.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hdrhistogram::Histogram;

use super::model::{Api, Protocol, RequestRecord};
use super::scope::OpScopeData;
use super::snapshot::*;

const HIST_MAX_US: u64 = 3_600_000_000;
const SLOW_OPS_KEPT: usize = 8;

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_US, 3).expect("valid histogram bounds")
}

fn small_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_US, 2).expect("valid histogram bounds")
}

fn rec(h: &mut Histogram<u64>, us: u64) {
    let _ = h.record(us.min(HIST_MAX_US));
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
            client_us: 0,
        }
    }
}

struct Bucket {
    ops: u64,
    bytes: u64,
    errors: u64,
    throttled: u64,
    latency: Histogram<u64>,
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
    verbs: BTreeMap<String, VerbAgg>,
    apis: BTreeMap<Api, ApiAgg>,
    buckets: Vec<Bucket>,
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
}

impl Agg {
    fn new() -> Self {
        Self {
            all_latency: hist(),
            verbs: BTreeMap::new(),
            apis: BTreeMap::new(),
            buckets: Vec::new(),
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
        }
    }

    fn record_request(&mut self, r: &RequestRecord) {
        self.requests += 1;
        if r.reused {
            self.reused_requests += 1;
        }
        match r.protocol {
            Protocol::Http1 => self.h1_requests += 1,
            Protocol::Http2 => self.h2_requests += 1,
            Protocol::InProcess => {}
        }
        self.bytes.backend_down += r.bytes_down;
        self.bytes.backend_up += r.bytes_up;
        match r.status {
            0 => self.res.network_errors += 1,
            429 => self.res.status_429 += 1,
            503 => self.res.status_503 += 1,
            s if s >= 500 => self.res.server_errors += 1,
            s if s >= 400 && s != 404 => self.res.client_errors += 1,
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
        if r.is_error() && r.status != 404 {
            a.errors += 1;
        }
        if r.is_throttle() {
            a.throttled += 1;
        }
        if r.reused {
            a.reused += 1;
        }
        *a.status.entry(r.status).or_default() += 1;
        a.bytes_up += r.bytes_up;
        a.bytes_down += r.bytes_down;
        let t = &r.timing;
        rec(&mut a.stages.queue, t.queue_us);
        if !r.reused && r.protocol != Protocol::InProcess {
            rec(&mut a.stages.connect, t.connect_us());
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
    pub started: Instant,
    /// Latency from the *intended* start (open-loop) or actual start (closed-loop).
    pub latency_us: u64,
    pub logical_read: u64,
    pub logical_write: u64,
    pub error: Option<String>,
    pub data: OpScopeData,
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

/// Central sink shared by the transport, OpenDAL layers and workload workers.
pub struct Recorder {
    phase: AtomicU8,
    measuring: AtomicBool,
    frozen_elapsed_ms: AtomicU64,
    window_start: Mutex<Instant>,
    agg: Mutex<Agg>,
    inflight_ops: AtomicU64,
    inflight_requests: AtomicU64,
    peak_inflight_requests: AtomicU64,
    wire_rx: AtomicU64,
    wire_tx: AtomicU64,
    cache_enabled: AtomicBool,
    proxy: Mutex<Option<String>>,
    // await-tree slow-path capture
    tree_registry: Mutex<Option<await_tree::Registry>>,
    live_ops: Mutex<HashMap<u64, Instant>>,
    trees: Mutex<HashMap<u64, String>>,
    client: Mutex<ClientStats>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(0),
            measuring: AtomicBool::new(false),
            frozen_elapsed_ms: AtomicU64::new(0),
            window_start: Mutex::new(Instant::now()),
            agg: Mutex::new(Agg::new()),
            inflight_ops: AtomicU64::new(0),
            inflight_requests: AtomicU64::new(0),
            peak_inflight_requests: AtomicU64::new(0),
            wire_rx: AtomicU64::new(0),
            wire_tx: AtomicU64::new(0),
            cache_enabled: AtomicBool::new(false),
            proxy: Mutex::new(None),
            tree_registry: Mutex::new(None),
            live_ops: Mutex::new(HashMap::new()),
            trees: Mutex::new(HashMap::new()),
            client: Mutex::new(ClientStats {
                cpu_cores: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
                ..Default::default()
            }),
        })
    }

    fn agg(&self) -> std::sync::MutexGuard<'_, Agg> {
        self.agg.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_cache_enabled(&self, on: bool) {
        self.cache_enabled.store(on, Ordering::Relaxed);
    }

    pub fn set_proxy(&self, proxy: Option<String>) {
        *self.proxy.lock().unwrap_or_else(|e| e.into_inner()) = proxy;
    }

    pub fn set_tree_registry(&self, reg: await_tree::Registry) {
        *self.tree_registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(reg);
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
            drop(a);
            self.wire_rx.store(0, Ordering::Relaxed);
            self.wire_tx.store(0, Ordering::Relaxed);
            self.peak_inflight_requests.store(
                self.inflight_requests.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            *self.window_start.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            self.frozen_elapsed_ms.store(0, Ordering::Relaxed);
            self.measuring.store(true, Ordering::Release);
        } else if self.measuring.load(Ordering::Acquire) {
            // Freeze the window length at the moment measurement ends.
            self.measuring.store(false, Ordering::Release);
            let elapsed = self.elapsed_s();
            self.frozen_elapsed_ms
                .store((elapsed * 1000.0) as u64, Ordering::Relaxed);
        }
        self.phase.store(code, Ordering::Release);
    }

    #[inline]
    pub fn is_measuring(&self) -> bool {
        self.measuring.load(Ordering::Acquire)
    }

    pub fn window_start(&self) -> Instant {
        *self.window_start.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn elapsed_s(&self) -> f64 {
        if !self.is_measuring() {
            let frozen = self.frozen_elapsed_ms.load(Ordering::Relaxed);
            if frozen > 0 {
                return frozen as f64 / 1000.0;
            }
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
        }
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
        if d.proxy.is_some() {
            *self.proxy.lock().unwrap_or_else(|e| e.into_inner()) = d.proxy;
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
        if !measuring {
            return;
        }
        a.conn.dialed += 1;
        rec(&mut a.dns, d.dns_us);
        rec(&mut a.tcp, d.tcp_us);
        if d.proxy_us > 0 {
            rec(&mut a.proxy, d.proxy_us);
        }
        if d.tls_us > 0 {
            rec(&mut a.tls, d.tls_us);
        }
    }

    /// Request issued outside any operation scope (rare: background OpenDAL work).
    pub fn record_unscoped_request(&self, r: &RequestRecord) {
        if self.is_measuring() {
            self.agg().record_request(r);
        }
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

    pub fn op_started(&self, op_id: u64) {
        self.inflight_ops.fetch_add(1, Ordering::Relaxed);
        self.live_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op_id, Instant::now());
    }

    /// Record a completed logical operation (only if it started inside the window).
    pub fn complete_op(&self, measured: bool, c: OpCompletion) {
        self.inflight_ops.fetch_sub(1, Ordering::Relaxed);
        self.live_ops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&c.op_id);
        let tree = self
            .trees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&c.op_id);
        if !measured || !self.is_measuring() {
            return;
        }
        let window_start = self.window_start();
        let at_s = c
            .started
            .saturating_duration_since(window_start)
            .as_secs_f64();
        let bd = breakdown_us(&c.data, c.latency_us);
        let ok = c.error.is_none();
        let throttled = c.data.throttled;
        let bytes = c.logical_read + c.logical_write;
        let req_count = c.data.requests.len() as u64 + c.data.dropped_requests;

        let mut a = self.agg();
        for r in &c.data.requests {
            a.record_request(r);
        }
        a.ops += 1;
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
        {
            let v = a
                .verbs
                .entry(c.verb.to_owned())
                .or_insert_with(VerbAgg::new);
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
            v.client_us += bd[3];
        }
        let idx = at_s.max(0.0) as usize;
        while a.buckets.len() <= idx {
            a.buckets.push(Bucket {
                ops: 0,
                bytes: 0,
                errors: 0,
                throttled: 0,
                latency: small_hist(),
            });
        }
        let b = &mut a.buckets[idx];
        b.ops += 1;
        b.bytes += bytes;
        if !ok {
            b.errors += 1;
        }
        if throttled {
            b.throttled += 1;
        }
        rec(&mut b.latency, c.latency_us);

        // Keep the slowest operations with their full request timelines.
        let lat_ms = c.latency_us as f64 / 1000.0;
        let qualifies = a.slow.len() < SLOW_OPS_KEPT
            || a.slow.last().map(|s| lat_ms > s.latency_ms).unwrap_or(true);
        if qualifies {
            let ms = |v: u64| v as f64 / 1000.0;
            a.slow.push(SlowOp {
                verb: c.verb.to_owned(),
                latency_ms: lat_ms,
                at_s,
                breakdown: OpBreakdown {
                    queue_ms: ms(bd[0]),
                    cache_ms: ms(bd[1]),
                    backend_ms: ms(bd[2]),
                    client_ms: ms(bd[3]),
                },
                retries: c.data.retries,
                requests: c.data.requests,
                await_tree: tree,
                error: c.error,
            });
            a.slow.sort_by(|x, y| y.latency_ms.total_cmp(&x.latency_ms));
            a.slow.truncate(SLOW_OPS_KEPT);
        }
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
        let subs = reg.collect::<super::scope::SubTaskKey>();
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
    }

    pub fn snapshot(&self) -> Snapshot {
        let elapsed = self.elapsed_s().max(1e-3);
        let a = self.agg();
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);

        let verbs: Vec<VerbStats> = a
            .verbs
            .iter()
            .map(|(name, v)| {
                let n = v.count.max(1) as f64;
                VerbStats {
                    verb: name.clone(),
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
                reused_pct: crate::util::pct(s.reused as f64, s.count as f64),
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

        // The last bucket is usually partial; scale its rates by its covered fraction.
        let nb = a.buckets.len();
        let timeline = a
            .buckets
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let width = if i + 1 == nb {
                    (elapsed - i as f64).clamp(0.05, 1.0)
                } else {
                    1.0
                };
                TimelinePoint {
                    t_s: i as f64 + 1.0,
                    ops_per_s: b.ops as f64 / width,
                    mib_per_s: mib(b.bytes) / width,
                    p50_ms: if b.latency.is_empty() {
                        0.0
                    } else {
                        b.latency.value_at_quantile(0.5) as f64 / 1000.0
                    },
                    p99_ms: if b.latency.is_empty() {
                        0.0
                    } else {
                        b.latency.value_at_quantile(0.99) as f64 / 1000.0
                    },
                    errors: b.errors,
                    throttled: b.throttled,
                }
            })
            .collect();

        let summary = Summary {
            ops: a.ops,
            ops_per_s: a.ops as f64 / elapsed,
            failed_ops: a.failed_ops,
            error_rate: crate::util::ratio(a.failed_ops as f64, a.ops as f64),
            mib_per_s: mib(a.logical_read + a.logical_write) / elapsed,
            read_mib_per_s: mib(a.logical_read) / elapsed,
            write_mib_per_s: mib(a.logical_write) / elapsed,
            latency: quantiles(&a.all_latency),
            inflight_ops: self.inflight_ops.load(Ordering::Relaxed),
        };

        Snapshot {
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
            open_loop: a.open_loop.clone(),
            timeline,
            slow_ops: a.slow.clone(),
        }
    }
}

/// Exact split of one op's wall time into `[queue, cache, backend, client]` microseconds using a
/// sweep over its request intervals: an instant counts as *backend* if any request is on the
/// network, else *queue* if any request is waiting for a permit/connection, else *client*.
/// Cache lookup time is carved out of the client share.
pub fn breakdown_us(d: &OpScopeData, total_us: u64) -> [u64; 4] {
    // events: (time, kind_delta) kind 0 = queue, 1 = network
    let mut ev: Vec<(u64, i32, i32)> = Vec::with_capacity(d.requests.len() * 4);
    for r in &d.requests {
        let s = r.start_offset_us;
        let q_end = s + r.timing.queue_us;
        let n_end = (s + r.timing.total_us)
            .saturating_sub(r.timing.stall_us)
            .max(q_end);
        if r.timing.queue_us > 0 {
            ev.push((s, 1, 0));
            ev.push((q_end, -1, 0));
        }
        if n_end > q_end {
            ev.push((q_end, 0, 1));
            ev.push((n_end, 0, -1));
        }
    }
    ev.sort_by_key(|e| (e.0, e.1.min(e.2)));
    let (mut q, mut n) = (0i32, 0i32);
    let (mut queue, mut net) = (0u64, 0u64);
    let mut last = 0u64;
    for (t, dq, dn) in ev {
        let t = t.min(total_us);
        if t > last {
            if n > 0 {
                net += t - last;
            } else if q > 0 {
                queue += t - last;
            }
            last = t;
        }
        q += dq;
        n += dn;
    }
    let rest = total_us.saturating_sub(queue + net);
    let cache = d.cache_lookup_us.min(rest);
    [queue, cache, net, rest - cache]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::model::*;

    fn req(start: u64, queue: u64, total: u64) -> RequestRecord {
        RequestRecord {
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
        assert_eq!(b, [0, 50, 1400, 550]);
        assert_eq!(b.iter().sum::<u64>(), 2000);
    }

    #[test]
    fn breakdown_counts_pure_queue_time() {
        let d = OpScopeData {
            requests: vec![req(0, 300, 1000)],
            ..Default::default()
        };
        assert_eq!(breakdown_us(&d, 1000), [300, 0, 700, 0]);
    }
}
