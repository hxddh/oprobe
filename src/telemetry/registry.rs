//! Lock-light white-box telemetry aggregation registry, task-local operation trace context,
//! per-stage & per-verb HDR histograms, and live `await-tree` slow-request call-stack profiler.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::net_probe::EndpointHandshakeSummary;

use super::{
    BottleneckDiagnosis, CleanupSummary, SlowRequestTrace, StageQuantiles, StageWaterfall,
    TelemetryFrame, TimeSeriesPoint, TransportAndHostSnapshot, VerbMetricsRow,
};

tokio::task_local! {
    static ACTIVE_OP_SCOPE: Arc<OpTraceScope>;
}

/// Execution lifecycle phase used to prevent fixture seeding, warmup, or cleanup
/// from polluting measured benchmark latency and amplification metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MeasurementPhase {
    Seed = 0,
    Warmup = 1,
    Measure = 2,
    Verify = 3,
    Cleanup = 4,
}

impl MeasurementPhase {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Seed,
            1 => Self::Warmup,
            2 => Self::Measure,
            3 => Self::Verify,
            _ => Self::Cleanup,
        }
    }
}

/// Per-operation trace context propagated via `tokio::task_local!` across OpenDAL layers and HTTP transport.
#[derive(Default)]
pub struct OpTraceScope {
    pub sem_wait_us: AtomicU64,
    pub cache_lookup_us: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub sub_rpcs: AtomicU64,
    pub storage_read_bytes: AtomicU64,
    pub multipart_merge_us: AtomicU64,
    pub retries: AtomicU64,
    pub throttled_429_503: AtomicBool,
}

impl OpTraceScope {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Inspect and mutate the current task's `OpTraceScope`, if executing inside a traced operation.
    pub fn with_current<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&Arc<OpTraceScope>) -> R,
    {
        ACTIVE_OP_SCOPE.try_with(f).ok()
    }

    /// Run an async operation within this `OpTraceScope`.
    pub async fn run<F, T>(self: &Arc<Self>, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        ACTIVE_OP_SCOPE.scope(self.clone(), fut).await
    }
}

/// RAII guard tracking an in-flight root task inside `TelemetryRegistry` for `await-tree` slow-span profiling.
pub struct OpTraceGuard {
    registry: Arc<TelemetryRegistry>,
    task_id: u64,
}

impl Drop for OpTraceGuard {
    fn drop(&mut self) {
        self.registry.unregister_active_task(self.task_id);
    }
}

struct VerbBucket {
    hist_us: Histogram<u64>,
    count: u64,
    errors: u64,
    logical_bytes: u64,
    throttled: u64,
    retries: u64,
}

impl VerbBucket {
    fn new() -> Self {
        Self {
            hist_us: Histogram::<u64>::new_with_bounds(1, 300_000_000, 3)
                .expect("valid HDR histogram bounds"),
            count: 0,
            errors: 0,
            logical_bytes: 0,
            throttled: 0,
            retries: 0,
        }
    }
}

struct StageHistograms {
    dns_us: Histogram<u64>,
    tcp_us: Histogram<u64>,
    tls_us: Histogram<u64>,
    ttfb_us: Histogram<u64>,
    body_us: Histogram<u64>,
    merge_us: Histogram<u64>,
}

impl StageHistograms {
    fn new() -> Self {
        let mk = || {
            Histogram::<u64>::new_with_bounds(1, 300_000_000, 3)
                .expect("valid stage HDR histogram bounds")
        };
        Self {
            dns_us: mk(),
            tcp_us: mk(),
            tls_us: mk(),
            ttfb_us: mk(),
            body_us: mk(),
            merge_us: mk(),
        }
    }

    fn clear(&mut self) {
        self.dns_us.reset();
        self.tcp_us.reset();
        self.tls_us.reset();
        self.ttfb_us.reset();
        self.body_us.reset();
        self.merge_us.reset();
    }
}

/// Thread-safe white-box telemetry registry shared across OpenDAL layers, Hyper connector/transport, and workload runners.
pub struct TelemetryRegistry {
    phase: AtomicU8,
    measure_started_at: Mutex<Instant>,

    // Stage 1 & 2: Client Queueing & Local Cache
    pub semaphore_wait_us: AtomicU64,
    pub cache_lookup_us: AtomicU64,
    pub cache_lookups: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub cache_bytes_served: AtomicU64,

    // Stage 3, 4, 5: Real Socket / TLS / HTTP Transport Counters
    pub total_http_requests: AtomicU64,
    pub cold_connections_dialed: AtomicU64,
    pub reused_connections: AtomicU64,
    pub active_inflight: AtomicUsize,
    pub peak_inflight: AtomicUsize,
    pub class_a_rpcs: AtomicU64,
    pub class_b_rpcs: AtomicU64,
    pub multipart_initiations: AtomicU64,
    pub multipart_parts_uploaded: AtomicU64,
    pub multipart_completions: AtomicU64,

    pub sum_dns_us: AtomicU64,
    pub sum_tcp_us: AtomicU64,
    pub sum_tls_us: AtomicU64,
    pub sum_pool_checkout_us: AtomicU64,
    pub sum_remote_ttfb_us: AtomicU64,
    pub sum_body_stream_us: AtomicU64,
    pub sum_in_process_io_us: AtomicU64,

    // Stage 6 & 7: Multipart Merge & Checksum Verify
    pub sum_multipart_merge_us: AtomicU64,
    pub sum_verify_crc_us: AtomicU64,

    // HTTP Status & Resilience Counters
    pub status_200: AtomicU64,
    pub status_206: AtomicU64,
    pub status_403: AtomicU64,
    pub status_404: AtomicU64,
    pub status_429: AtomicU64,
    pub status_500: AtomicU64,
    pub status_503: AtomicU64,
    pub status_other_err: AtomicU64,
    pub network_errors: AtomicU64,
    pub retry_attempts: AtomicU64,
    pub retry_backoff_us: AtomicU64,
    pub tail_cut_cancels: AtomicU64,

    // Byte & Amplification Accounting
    pub top_level_ops: AtomicU64,
    pub backend_storage_rpcs: AtomicU64,
    pub logical_read_bytes: AtomicU64,
    pub logical_write_bytes: AtomicU64,
    pub uncached_logical_read_bytes: AtomicU64,
    pub storage_read_bytes: AtomicU64,
    pub coalesced_gap_waste_bytes: AtomicU64,
    pub wire_bytes_rx: AtomicU64,
    pub wire_bytes_tx: AtomicU64,

    // Data Integrity
    pub crc_verified_ok: AtomicU64,
    pub crc_corrupted: AtomicU64,

    // Protocol & Request Metadata
    negotiated_meta: Mutex<(String, String, String)>,
    recent_request_ids: Mutex<VecDeque<String>>,
    stage_hists: Mutex<StageHistograms>,
    verbs: Mutex<BTreeMap<&'static str, VerbBucket>>,
    timeseries: Mutex<VecDeque<TimeSeriesPoint>>,
    handshake: Mutex<EndpointHandshakeSummary>,
    cleanup: Mutex<CleanupSummary>,

    // AwaitTree Slow-Request Call-Stack Tracking
    await_registry: await_tree::Registry,
    active_tasks: Mutex<HashMap<u64, (String, Instant)>>,
    slowest_traces: Mutex<Vec<SlowRequestTrace>>,
}

impl TelemetryRegistry {
    pub fn new(await_registry: await_tree::Registry, scheme: &str) -> Arc<Self> {
        let default_proto = if matches!(scheme, "memory" | "fs") {
            ("in-process".to_owned(), "none".to_owned(), "none".to_owned())
        } else {
            ("http/1.1".to_owned(), "none".to_owned(), "none".to_owned())
        };
        Arc::new(Self {
            phase: AtomicU8::new(MeasurementPhase::Seed as u8),
            measure_started_at: Mutex::new(Instant::now()),
            semaphore_wait_us: AtomicU64::new(0),
            cache_lookup_us: AtomicU64::new(0),
            cache_lookups: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            cache_bytes_served: AtomicU64::new(0),
            total_http_requests: AtomicU64::new(0),
            cold_connections_dialed: AtomicU64::new(0),
            reused_connections: AtomicU64::new(0),
            active_inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            class_a_rpcs: AtomicU64::new(0),
            class_b_rpcs: AtomicU64::new(0),
            multipart_initiations: AtomicU64::new(0),
            multipart_parts_uploaded: AtomicU64::new(0),
            multipart_completions: AtomicU64::new(0),
            sum_dns_us: AtomicU64::new(0),
            sum_tcp_us: AtomicU64::new(0),
            sum_tls_us: AtomicU64::new(0),
            sum_pool_checkout_us: AtomicU64::new(0),
            sum_remote_ttfb_us: AtomicU64::new(0),
            sum_body_stream_us: AtomicU64::new(0),
            sum_in_process_io_us: AtomicU64::new(0),
            sum_multipart_merge_us: AtomicU64::new(0),
            sum_verify_crc_us: AtomicU64::new(0),
            status_200: AtomicU64::new(0),
            status_206: AtomicU64::new(0),
            status_403: AtomicU64::new(0),
            status_404: AtomicU64::new(0),
            status_429: AtomicU64::new(0),
            status_500: AtomicU64::new(0),
            status_503: AtomicU64::new(0),
            status_other_err: AtomicU64::new(0),
            network_errors: AtomicU64::new(0),
            retry_attempts: AtomicU64::new(0),
            retry_backoff_us: AtomicU64::new(0),
            tail_cut_cancels: AtomicU64::new(0),
            top_level_ops: AtomicU64::new(0),
            backend_storage_rpcs: AtomicU64::new(0),
            logical_read_bytes: AtomicU64::new(0),
            logical_write_bytes: AtomicU64::new(0),
            uncached_logical_read_bytes: AtomicU64::new(0),
            storage_read_bytes: AtomicU64::new(0),
            coalesced_gap_waste_bytes: AtomicU64::new(0),
            wire_bytes_rx: AtomicU64::new(0),
            wire_bytes_tx: AtomicU64::new(0),
            crc_verified_ok: AtomicU64::new(0),
            crc_corrupted: AtomicU64::new(0),
            negotiated_meta: Mutex::new(default_proto),
            recent_request_ids: Mutex::new(VecDeque::with_capacity(12)),
            stage_hists: Mutex::new(StageHistograms::new()),
            verbs: Mutex::new(BTreeMap::new()),
            timeseries: Mutex::new(VecDeque::with_capacity(80)),
            handshake: Mutex::new(EndpointHandshakeSummary::local_in_process(scheme)),
            cleanup: Mutex::new(CleanupSummary::default()),
            await_registry,
            active_tasks: Mutex::new(HashMap::new()),
            slowest_traces: Mutex::new(Vec::new()),
        })
    }

    #[inline]
    pub fn phase(&self) -> MeasurementPhase {
        MeasurementPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    #[inline]
    pub fn is_measuring(&self) -> bool {
        self.phase() == MeasurementPhase::Measure
    }

    pub fn set_phase(&self, next: MeasurementPhase) {
        self.phase.store(next as u8, Ordering::Release);
        if next == MeasurementPhase::Measure {
            if let Ok(mut guard) = self.measure_started_at.lock() {
                *guard = Instant::now();
            }
        }
    }

    /// Reset all measurement-window counters after warmup while preserving endpoint handshake metadata.
    pub fn reset_measurement_counters(&self) {
        for atom in [
            &self.semaphore_wait_us,
            &self.cache_lookup_us,
            &self.cache_lookups,
            &self.cache_hits,
            &self.cache_misses,
            &self.cache_bytes_served,
            &self.total_http_requests,
            &self.cold_connections_dialed,
            &self.reused_connections,
            &self.class_a_rpcs,
            &self.class_b_rpcs,
            &self.multipart_initiations,
            &self.multipart_parts_uploaded,
            &self.multipart_completions,
            &self.sum_dns_us,
            &self.sum_tcp_us,
            &self.sum_tls_us,
            &self.sum_pool_checkout_us,
            &self.sum_remote_ttfb_us,
            &self.sum_body_stream_us,
            &self.sum_in_process_io_us,
            &self.sum_multipart_merge_us,
            &self.sum_verify_crc_us,
            &self.status_200,
            &self.status_206,
            &self.status_403,
            &self.status_404,
            &self.status_429,
            &self.status_500,
            &self.status_503,
            &self.status_other_err,
            &self.network_errors,
            &self.retry_attempts,
            &self.retry_backoff_us,
            &self.tail_cut_cancels,
            &self.top_level_ops,
            &self.backend_storage_rpcs,
            &self.logical_read_bytes,
            &self.logical_write_bytes,
            &self.uncached_logical_read_bytes,
            &self.storage_read_bytes,
            &self.coalesced_gap_waste_bytes,
            &self.wire_bytes_rx,
            &self.wire_bytes_tx,
        ] {
            atom.store(0, Ordering::Relaxed);
        }
        self.peak_inflight.store(0, Ordering::Relaxed);
        if let Ok(mut h) = self.stage_hists.lock() {
            h.clear();
        }
        if let Ok(mut v) = self.verbs.lock() {
            v.clear();
        }
        if let Ok(mut ts) = self.timeseries.lock() {
            ts.clear();
        }
        if let Ok(mut st) = self.slowest_traces.lock() {
            st.clear();
        }
        if let Ok(mut t0) = self.measure_started_at.lock() {
            *t0 = Instant::now();
        }
    }

    pub fn set_handshake_summary(&self, summary: EndpointHandshakeSummary) {
        if let Ok(mut meta) = self.negotiated_meta.lock() {
            *meta = (
                summary.negotiated_alpn.clone(),
                summary.tls_version.clone(),
                summary.cipher_suite.clone(),
            );
        }
        if let Ok(mut slot) = self.handshake.lock() {
            *slot = summary;
        }
    }

    pub fn set_cleanup_summary(&self, summary: CleanupSummary) {
        if let Ok(mut slot) = self.cleanup.lock() {
            *slot = summary;
        }
    }

    pub fn update_negotiated_protocol(&self, alpn: &str, tls_ver: &str, cipher: &str) {
        if let Ok(mut meta) = self.negotiated_meta.lock() {
            *meta = (alpn.to_owned(), tls_ver.to_owned(), cipher.to_owned());
        }
    }

    pub fn record_inflight_enter(&self) -> usize {
        let cur = self.active_inflight.fetch_add(1, Ordering::AcqRel) + 1;
        if self.is_measuring() {
            self.peak_inflight.fetch_max(cur, Ordering::Relaxed);
        }
        cur
    }

    pub fn record_inflight_exit(&self) {
        self.active_inflight.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn push_request_id(&self, entry: String) {
        if !self.is_measuring() {
            return;
        }
        if let Ok(mut q) = self.recent_request_ids.lock() {
            if q.len() >= 10 {
                q.pop_front();
            }
            q.push_back(entry);
        }
    }

    pub fn record_cold_handshake(&self, dns_us: u64, tcp_us: u64, tls_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.cold_connections_dialed.fetch_add(1, Ordering::Relaxed);
        self.sum_dns_us.fetch_add(dns_us, Ordering::Relaxed);
        self.sum_tcp_us.fetch_add(tcp_us, Ordering::Relaxed);
        self.sum_tls_us.fetch_add(tls_us, Ordering::Relaxed);
        if let Ok(mut h) = self.stage_hists.lock() {
            let _ = h.dns_us.record(dns_us.clamp(1, 300_000_000));
            let _ = h.tcp_us.record(tcp_us.clamp(1, 300_000_000));
            if tls_us > 0 {
                let _ = h.tls_us.record(tls_us.clamp(1, 300_000_000));
            }
        }
    }

    pub fn record_warm_reuse(&self, pool_checkout_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.reused_connections.fetch_add(1, Ordering::Relaxed);
        self.sum_pool_checkout_us
            .fetch_add(pool_checkout_us, Ordering::Relaxed);
    }

    pub fn record_gateway_ttfb(&self, ttfb_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.sum_remote_ttfb_us
            .fetch_add(ttfb_us, Ordering::Relaxed);
        if let Ok(mut h) = self.stage_hists.lock() {
            let _ = h.ttfb_us.record(ttfb_us.clamp(1, 300_000_000));
        }
    }

    pub fn record_body_stream(&self, body_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.sum_body_stream_us
            .fetch_add(body_us, Ordering::Relaxed);
        if let Ok(mut h) = self.stage_hists.lock() {
            let _ = h.body_us.record(body_us.clamp(1, 300_000_000));
        }
    }

    pub fn record_multipart_merge(&self, merge_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.sum_multipart_merge_us
            .fetch_add(merge_us, Ordering::Relaxed);
        let _ = OpTraceScope::with_current(|s| {
            s.multipart_merge_us.fetch_add(merge_us, Ordering::Relaxed);
        });
        if let Ok(mut h) = self.stage_hists.lock() {
            let _ = h.merge_us.record(merge_us.clamp(1, 300_000_000));
        }
    }

    pub fn record_semaphore_wait(&self, wait_us: u64) {
        if !self.is_measuring() {
            return;
        }
        self.semaphore_wait_us.fetch_add(wait_us, Ordering::Relaxed);
        let _ = OpTraceScope::with_current(|s| {
            s.sem_wait_us.fetch_add(wait_us, Ordering::Relaxed);
        });
    }

    pub fn record_cache_lookup(&self, lookup_us: u64, hit: bool, hit_bytes: u64) {
        if !self.is_measuring() {
            return;
        }
        self.cache_lookup_us.fetch_add(lookup_us, Ordering::Relaxed);
        self.cache_lookups.fetch_add(1, Ordering::Relaxed);
        if hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.cache_bytes_served
                .fetch_add(hit_bytes, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
        let _ = OpTraceScope::with_current(|s| {
            s.cache_lookup_us.fetch_add(lookup_us, Ordering::Relaxed);
            if hit {
                s.cache_hits.fetch_add(1, Ordering::Relaxed);
            } else {
                s.cache_misses.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    pub fn record_backend_rpc(&self, is_class_a: bool) {
        if !self.is_measuring() {
            return;
        }
        self.backend_storage_rpcs.fetch_add(1, Ordering::Relaxed);
        if is_class_a {
            self.class_a_rpcs.fetch_add(1, Ordering::Relaxed);
        } else {
            self.class_b_rpcs.fetch_add(1, Ordering::Relaxed);
        }
        let _ = OpTraceScope::with_current(|s| {
            s.sub_rpcs.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub fn record_storage_read_bytes(&self, bytes: u64) {
        if !self.is_measuring() {
            return;
        }
        self.storage_read_bytes.fetch_add(bytes, Ordering::Relaxed);
        let _ = OpTraceScope::with_current(|s| {
            s.storage_read_bytes.fetch_add(bytes, Ordering::Relaxed);
        });
    }

    pub fn register_active_task(
        self: &Arc<Self>,
        task_id: u64,
        label: String,
    ) -> (await_tree::TreeRoot, OpTraceGuard) {
        let root = self.await_registry.register(task_id, label.clone());
        if let Ok(mut map) = self.active_tasks.lock() {
            map.insert(task_id, (label, Instant::now()));
        }
        (
            root,
            OpTraceGuard {
                registry: self.clone(),
                task_id,
            },
        )
    }

    fn unregister_active_task(&self, task_id: u64) {
        if let Ok(mut map) = self.active_tasks.lock() {
            if let Some((label, started)) = map.remove(&task_id) {
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                if elapsed_ms >= 2.0 && self.is_measuring() {
                    if let Some(tree) = self.await_registry.get(task_id) {
                        self.upsert_slow_trace(SlowRequestTrace {
                            task_id,
                            root_span: label,
                            observed_ms: elapsed_ms,
                            tree_dump: tree.to_string(),
                        });
                    }
                }
            }
        }
    }

    /// Sample all currently active `await-tree` futures (invoked every ~10ms by background profiler).
    pub fn sample_in_flight_trees(&self) {
        if !self.is_measuring() {
            return;
        }
        let active_snapshot: Vec<(u64, String, f64)> = match self.active_tasks.lock() {
            Ok(map) => map
                .iter()
                .map(|(&id, (lbl, t0))| (id, lbl.clone(), t0.elapsed().as_secs_f64() * 1000.0))
                .collect(),
            Err(_) => return,
        };

        for (task_id, label, elapsed_ms) in active_snapshot {
            if elapsed_ms >= 1.0 {
                if let Some(tree) = self.await_registry.get(task_id) {
                    self.upsert_slow_trace(SlowRequestTrace {
                        task_id,
                        root_span: label,
                        observed_ms: elapsed_ms,
                        tree_dump: tree.to_string(),
                    });
                }
            }
        }
    }

    fn upsert_slow_trace(&self, candidate: SlowRequestTrace) {
        let Ok(mut traces) = self.slowest_traces.lock() else {
            return;
        };
        if let Some(existing) = traces.iter_mut().find(|t| t.task_id == candidate.task_id) {
            if candidate.observed_ms >= existing.observed_ms {
                *existing = candidate;
            }
        } else if traces.len() < 6 {
            traces.push(candidate);
        } else if let Some(min_slot) = traces
            .iter_mut()
            .min_by(|a, b| a.observed_ms.partial_cmp(&b.observed_ms).unwrap_or(std::cmp::Ordering::Equal))
        {
            if candidate.observed_ms > min_slot.observed_ms {
                *min_slot = candidate;
            }
        }
        traces.sort_by(|a, b| {
            b.observed_ms
                .partial_cmp(&a.observed_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        traces.truncate(5);
    }

    /// Record completion of a top-level OpenDAL workload verb.
    pub fn record_verb_completion(
        &self,
        verb: &'static str,
        elapsed: Duration,
        logical_read: u64,
        logical_write: u64,
        scope: &OpTraceScope,
        succeeded: bool,
    ) {
        if !self.is_measuring() {
            return;
        }
        let us = (elapsed.as_micros() as u64).clamp(1, 300_000_000);
        let ops_done = self.top_level_ops.fetch_add(1, Ordering::Relaxed) + 1;
        self.logical_read_bytes
            .fetch_add(logical_read, Ordering::Relaxed);
        self.logical_write_bytes
            .fetch_add(logical_write, Ordering::Relaxed);

        let cache_hit = scope.cache_hits.load(Ordering::Relaxed) > 0
            && scope.cache_misses.load(Ordering::Relaxed) == 0;
        if logical_read > 0 && !cache_hit {
            self.uncached_logical_read_bytes
                .fetch_add(logical_read, Ordering::Relaxed);
        }

        // For non-HTTP backends (`memory` / `fs`), record exact local kernel/memory I/O duration
        if self.total_http_requests.load(Ordering::Relaxed) == 0 {
            let overhead = scope
                .sem_wait_us
                .load(Ordering::Relaxed)
                .saturating_add(scope.cache_lookup_us.load(Ordering::Relaxed))
                .saturating_add(scope.multipart_merge_us.load(Ordering::Relaxed));
            self.sum_in_process_io_us
                .fetch_add(us.saturating_sub(overhead), Ordering::Relaxed);
        }

        let throttled = scope.throttled_429_503.load(Ordering::Relaxed);
        let retries = scope.retries.load(Ordering::Relaxed);
        let op_bytes = logical_read + logical_write;

        let mut latest_p99_ms = us as f64 / 1000.0;
        if let Ok(mut map) = self.verbs.lock() {
            let bucket = map.entry(verb).or_insert_with(VerbBucket::new);
            let _ = bucket.hist_us.record(us);
            bucket.count += 1;
            if !succeeded {
                bucket.errors += 1;
            }
            bucket.logical_bytes += op_bytes;
            if throttled {
                bucket.throttled += 1;
            }
            bucket.retries += retries;
            latest_p99_ms = bucket.hist_us.value_at_quantile(0.99) as f64 / 1000.0;
        }

        let elapsed_ms = self.measure_elapsed().as_millis() as u64;
        let elapsed_s = (elapsed_ms as f64 / 1000.0).max(0.005);
        let total_logical = (self.logical_read_bytes.load(Ordering::Relaxed)
            + self.logical_write_bytes.load(Ordering::Relaxed)) as f64;
        let mib_s = (total_logical / (1024.0 * 1024.0)) / elapsed_s;
        let inflight = self.active_inflight.load(Ordering::Relaxed);

        if let Ok(mut ts) = self.timeseries.lock() {
            let should_push = ts
                .back()
                .map(|last| elapsed_ms.saturating_sub(last.elapsed_ms) >= 50)
                .unwrap_or(true);
            if should_push {
                if ts.len() >= 80 {
                    ts.pop_front();
                }
                ts.push_back(TimeSeriesPoint {
                    elapsed_ms,
                    throughput_mib_s: mib_s,
                    p99_ms: latest_p99_ms,
                    inflight,
                    ops_completed: ops_done,
                });
            }
        }
    }

    /// Construct a `RetryLayer` notification closure that records exact `event.retry_after` backoff & `429/503` causes.
    pub fn make_retry_notify(
        registry: Arc<Self>,
    ) -> impl Fn(opendal::layers::RetryEvent<'_>) + Send + Sync + 'static {
        move |event: opendal::layers::RetryEvent<'_>| {
            if !registry.is_measuring() {
                return;
            }
            registry.retry_attempts.fetch_add(1, Ordering::Relaxed);
            let backoff_us = event.retry_after.as_micros() as u64;
            registry
                .retry_backoff_us
                .fetch_add(backoff_us, Ordering::Relaxed);

            let err_msg = event.err.to_string();
            let is_tail_cut = err_msg.contains("cancelled by tail cut");
            if is_tail_cut {
                registry.tail_cut_cancels.fetch_add(1, Ordering::Relaxed);
            }
            let is_rate_limit = event.err.kind() == opendal::ErrorKind::RateLimited
                || err_msg.contains("429")
                || err_msg.contains("503")
                || err_msg.contains("SlowDown");

            let _ = OpTraceScope::with_current(|s| {
                s.retries.fetch_add(1, Ordering::Relaxed);
                if is_rate_limit {
                    s.throttled_429_503.store(true, Ordering::Relaxed);
                }
            });
        }
    }

    pub fn measure_elapsed(&self) -> Duration {
        self.measure_started_at
            .lock()
            .map(|t0| t0.elapsed())
            .unwrap_or_else(|_| Duration::from_millis(1))
    }

    /// Build a complete white-box `TelemetryFrame` snapshot at the current instant.
    pub fn snapshot(&self, cpu_cores: usize, process_rss_mib: u64) -> TelemetryFrame {
        self.sample_in_flight_trees();

        let elapsed_s = self.measure_elapsed().as_secs_f64().max(0.001);
        let top_ops = self.top_level_ops.load(Ordering::Relaxed);
        let op_denom = top_ops.max(1);
        let http_reqs = self.total_http_requests.load(Ordering::Relaxed);

        let dns_avg = self
            .sum_dns_us
            .load(Ordering::Relaxed)
            .checked_div(http_reqs)
            .unwrap_or(0);
        let tcp_avg = self
            .sum_tcp_us
            .load(Ordering::Relaxed)
            .checked_div(http_reqs)
            .unwrap_or(0);
        let tls_avg = self
            .sum_tls_us
            .load(Ordering::Relaxed)
            .checked_div(http_reqs)
            .unwrap_or(0);
        let pool_avg = self
            .sum_pool_checkout_us
            .load(Ordering::Relaxed)
            .checked_div(http_reqs)
            .unwrap_or(0);

        let body_us = self
            .sum_body_stream_us
            .load(Ordering::Relaxed)
            .checked_div(http_reqs)
            .unwrap_or_else(|| self.sum_in_process_io_us.load(Ordering::Relaxed) / op_denom);

        let wf = StageWaterfall {
            client_queue_us: self.semaphore_wait_us.load(Ordering::Relaxed) / op_denom,
            local_cache_us: self.cache_lookup_us.load(Ordering::Relaxed) / op_denom,
            dns_tcp_tls_us: dns_avg + tcp_avg + tls_avg + pool_avg,
            dns_lookup_us: dns_avg,
            tcp_connect_us: tcp_avg,
            tls_handshake_us: tls_avg,
            pool_checkout_us: pool_avg,
            remote_ttfb_us: self
                .sum_remote_ttfb_us
                .load(Ordering::Relaxed)
                .checked_div(http_reqs)
                .unwrap_or(0),
            body_stream_us: body_us,
            multipart_merge_us: self.sum_multipart_merge_us.load(Ordering::Relaxed) / op_denom,
            verify_crc_us: self.sum_verify_crc_us.load(Ordering::Relaxed) / op_denom,
        };

        let stage_quantiles = if let Ok(h) = self.stage_hists.lock() {
            let q_ms = |hist: &Histogram<u64>, q: f64| -> f64 {
                if hist.is_empty() {
                    0.0
                } else {
                    hist.value_at_quantile(q) as f64 / 1000.0
                }
            };
            StageQuantiles {
                cold_dns_p50_ms: q_ms(&h.dns_us, 0.50),
                cold_dns_p99_ms: q_ms(&h.dns_us, 0.99),
                cold_tcp_p50_ms: q_ms(&h.tcp_us, 0.50),
                cold_tcp_p99_ms: q_ms(&h.tcp_us, 0.99),
                cold_tls_p50_ms: q_ms(&h.tls_us, 0.50),
                cold_tls_p99_ms: q_ms(&h.tls_us, 0.99),
                gateway_ttfb_p50_ms: q_ms(&h.ttfb_us, 0.50),
                gateway_ttfb_p95_ms: q_ms(&h.ttfb_us, 0.95),
                gateway_ttfb_p99_ms: q_ms(&h.ttfb_us, 0.99),
                body_stream_p50_ms: q_ms(&h.body_us, 0.50),
                body_stream_p99_ms: q_ms(&h.body_us, 0.99),
                multipart_merge_p50_ms: q_ms(&h.merge_us, 0.50),
                multipart_merge_p99_ms: q_ms(&h.merge_us, 0.99),
            }
        } else {
            StageQuantiles::default()
        };

        let mut verb_rows = Vec::new();
        if let Ok(map) = self.verbs.lock() {
            for (name, b) in map.iter() {
                let cnt = b.count.max(1) as f64;
                let mean_us = b.hist_us.mean();
                let mean_s = (mean_us / 1_000_000.0).max(0.000_001);
                let total_mib = (b.logical_bytes as f64) / (1024.0 * 1024.0);
                let avg_bytes_mib = total_mib / cnt;
                verb_rows.push(VerbMetricsRow {
                    verb: (*name).to_owned(),
                    count: b.count,
                    errors: b.errors,
                    ops_per_sec: b.count as f64 / elapsed_s,
                    mean_ms: mean_us / 1000.0,
                    p50_ms: b.hist_us.value_at_quantile(0.50) as f64 / 1000.0,
                    p90_ms: b.hist_us.value_at_quantile(0.90) as f64 / 1000.0,
                    p95_ms: b.hist_us.value_at_quantile(0.95) as f64 / 1000.0,
                    p99_ms: b.hist_us.value_at_quantile(0.99) as f64 / 1000.0,
                    p999_ms: b.hist_us.value_at_quantile(0.999) as f64 / 1000.0,
                    max_ms: b.hist_us.max() as f64 / 1000.0,
                    stddev_ms: b.hist_us.stdev() / 1000.0,
                    throughput_mib_s: total_mib / elapsed_s,
                    single_stream_mib_s: avg_bytes_mib / mean_s,
                    throttled_count: b.throttled,
                    throttle_429_503_pct: (b.throttled as f64 / cnt) * 100.0,
                    retry_count: b.retries,
                });
            }
        }

        let logical_read = self.logical_read_bytes.load(Ordering::Relaxed);
        let logical_write = self.logical_write_bytes.load(Ordering::Relaxed);
        let logical_total = logical_read + logical_write;
        let cache_served = self.cache_bytes_served.load(Ordering::Relaxed);
        let uncached_read = logical_read.saturating_sub(cache_served);
        let storage_read = self.storage_read_bytes.load(Ordering::Relaxed);
        let wire_rx = self.wire_bytes_rx.load(Ordering::Relaxed);
        let wire_tx = self.wire_bytes_tx.load(Ordering::Relaxed);
        let physical_wire = if wire_rx + wire_tx > 0 {
            wire_rx + wire_tx
        } else {
            storage_read + logical_write
        };

        let read_amp = if uncached_read > 0 && storage_read > 0 {
            (storage_read as f64 / uncached_read as f64).max(1.0)
        } else {
            1.0
        };

        let effective_read_phys = if wire_rx > 0 { wire_rx } else { storage_read };
        let net_wire_read_ratio = if logical_read > 0 {
            effective_read_phys as f64 / logical_read as f64
        } else {
            1.0
        };

        let write_amp = if logical_write > 0 && wire_tx > 0 {
            (wire_tx as f64 / logical_write as f64).max(1.0)
        } else {
            1.0
        };

        let backend_rpcs = http_reqs.max(self.backend_storage_rpcs.load(Ordering::Relaxed));
        let rpc_amp = if top_ops > 0 && backend_rpcs > 0 {
            backend_rpcs as f64 / top_ops as f64
        } else {
            1.0
        };

        let socket_reuse_pct = if http_reqs > 0 {
            (self.reused_connections.load(Ordering::Relaxed) as f64 / http_reqs as f64) * 100.0
        } else {
            100.0
        };

        let lookups = self.cache_lookups.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let cache_hit_pct = if lookups > 0 {
            (hits as f64 / lookups as f64) * 100.0
        } else {
            0.0
        };

        let agg_mib_s = (logical_total as f64 / (1024.0 * 1024.0)) / elapsed_s;
        let wire_mib_s = (physical_wire as f64 / (1024.0 * 1024.0)) / elapsed_s;

        let (negotiated_alpn, tls_version, cipher_suite) = self
            .negotiated_meta
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let recent_ids = self
            .recent_request_ids
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();

        let tp = TransportAndHostSnapshot {
            socket_reuse_pct,
            cold_connections_dialed: self.cold_connections_dialed.load(Ordering::Relaxed),
            reused_connections: self.reused_connections.load(Ordering::Relaxed),
            negotiated_alpn,
            tls_version,
            cipher_suite,
            active_inflight: self.active_inflight.load(Ordering::Relaxed),
            peak_inflight: self.peak_inflight.load(Ordering::Relaxed),
            total_http_requests: backend_rpcs,
            class_a_rpcs: self.class_a_rpcs.load(Ordering::Relaxed),
            class_b_rpcs: self.class_b_rpcs.load(Ordering::Relaxed),
            multipart_initiations: self.multipart_initiations.load(Ordering::Relaxed),
            multipart_parts_uploaded: self.multipart_parts_uploaded.load(Ordering::Relaxed),
            multipart_completions: self.multipart_completions.load(Ordering::Relaxed),
            http_200_count: self.status_200.load(Ordering::Relaxed),
            http_206_range_count: self.status_206.load(Ordering::Relaxed),
            http_403_auth_count: self.status_403.load(Ordering::Relaxed),
            http_404_count: self.status_404.load(Ordering::Relaxed),
            http_429_count: self.status_429.load(Ordering::Relaxed),
            http_500_count: self.status_500.load(Ordering::Relaxed),
            http_503_slowdown_count: self.status_503.load(Ordering::Relaxed),
            http_other_err_count: self.status_other_err.load(Ordering::Relaxed),
            network_err_count: self.network_errors.load(Ordering::Relaxed),
            retry_attempts: self.retry_attempts.load(Ordering::Relaxed),
            retry_backoff_ms: self.retry_backoff_us.load(Ordering::Relaxed) as f64 / 1000.0,
            tail_cut_cancels: self.tail_cut_cancels.load(Ordering::Relaxed),
            cache_lookups: lookups,
            cache_hits: hits,
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            cache_hit_ratio_pct: cache_hit_pct,
            cache_bytes_served: self.cache_bytes_served.load(Ordering::Relaxed),
            logical_ops: top_ops,
            logical_read_bytes: logical_read,
            logical_write_bytes: logical_write,
            logical_bytes: logical_total,
            storage_read_bytes: storage_read,
            physical_wire_bytes: physical_wire,
            wire_bytes_rx: wire_rx,
            wire_bytes_tx: wire_tx,
            coalesced_gap_waste_bytes: self.coalesced_gap_waste_bytes.load(Ordering::Relaxed),
            read_amplification: read_amp,
            net_wire_read_ratio,
            write_amplification: write_amp,
            rpc_amplification: rpc_amp,
            crc32c_verified_ok: self.crc_verified_ok.load(Ordering::Relaxed),
            crc32c_corrupted: self.crc_corrupted.load(Ordering::Relaxed),
            elapsed_secs: elapsed_s,
            aggregate_mib_s: agg_mib_s,
            wire_mib_s,
            mib_per_cpu_core: agg_mib_s / (cpu_cores.max(1) as f64),
            cpu_cores: cpu_cores.max(1),
            process_rss_mib,
            recent_request_ids: recent_ids,
        };

        let diagnosis = BottleneckDiagnosis::evaluate(&wf, &tp, &verb_rows);

        let slowest_traces: Vec<SlowRequestTrace> = self
            .slowest_traces
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();

        // Build await_tree_dump: show live in-flight futures first, plus Top Captured Slowest Call Stacks
        let mut dump_sections = Vec::new();
        let live: Vec<(u64, await_tree::Tree)> =
            self.await_registry.collect::<u64>().into_iter().take(4).collect();
        if !live.is_empty() {
            dump_sections.push("=== Live In-Flight Futures ===".to_owned());
            for (tid, tree) in live {
                dump_sections.push(format!("[in-flight task#{tid}]\n{}", tree.to_string().trim_end()));
            }
        }
        if !slowest_traces.is_empty() {
            dump_sections.push("=== Top Captured Slow-Request Call Stacks ===".to_owned());
            for (idx, tr) in slowest_traces.iter().enumerate() {
                dump_sections.push(format!(
                    "#{} (peak {:.2} ms, task#{}) {}\n{}",
                    idx + 1,
                    tr.observed_ms,
                    tr.task_id,
                    tr.root_span,
                    tr.tree_dump.trim_end()
                ));
            }
        }
        let await_tree_dump = if dump_sections.is_empty() {
            "No slow async spans (>1.0 ms) observed; all operations completed sub-millisecond."
                .to_owned()
        } else {
            dump_sections.join("\n\n")
        };

        let ts_vec = self
            .timeseries
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();

        let hs = self
            .handshake
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let cleanup = self
            .cleanup
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        TelemetryFrame {
            waterfall: wf,
            stage_quantiles,
            handshake: hs,
            verbs: verb_rows,
            timeseries: ts_vec,
            transport: tp,
            diagnosis,
            slowest_traces,
            await_tree_dump,
            cleanup,
        }
    }
}
