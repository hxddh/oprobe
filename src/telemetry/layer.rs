//! 存储语义层汇聚中心：
//! - 按独立操作动词维护 `hdrhistogram::Histogram<u64>`（P50 / P90 / P95 / P99 / P99.9 / Max / StdDev）；
//! - 维护 60 点实时吞吐与 P99 滑动窗口序列；
//! - 对接 `opendal::layers::RetryEvent` 统计重试次数，采集 `await_tree::Registry` 慢协程树。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::net_probe::EndpointHandshakeSummary;

use super::http_probe::HttpProbeCounters;
use super::{
    BottleneckDiagnosis, StageWaterfall, TelemetryFrame, TimeSeriesPoint, TransportAndHostSnapshot,
    VerbMetricsRow,
};

struct VerbBucket {
    hist_us: Histogram<u64>,
    count: u64,
    bytes: u64,
    throttled: u64,
}

impl VerbBucket {
    fn new() -> Self {
        Self {
            hist_us: Histogram::<u64>::new_with_bounds(1, 120_000_000, 3)
                .expect("valid HDR histogram bounds"),
            count: 0,
            bytes: 0,
            throttled: 0,
        }
    }
}

/// 跨线程全链路遥测汇聚中心
pub struct TelemetryRegistry {
    pub http: Arc<HttpProbeCounters>,
    pub retry_attempts: AtomicU64,
    pub retry_backoff_us: AtomicU64,
    pub semaphore_wait_us: AtomicU64,
    pub cache_lookup_us: AtomicU64,
    pub synthetic_body_us: AtomicU64,
    pub finalize_crc_us: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_lookups: AtomicU64,
    pub logical_bytes_requested: AtomicU64,
    pub physical_storage_bytes: AtomicU64,
    pub coalesced_gap_waste_bytes: AtomicU64,
    pub sub_requests_dispatched: AtomicU64,
    pub top_level_ops: AtomicU64,
    pub crc_verified_ok: AtomicU64,
    pub crc_corrupted: AtomicU64,
    verbs: Mutex<BTreeMap<&'static str, VerbBucket>>,
    timeseries: Mutex<VecDeque<TimeSeriesPoint>>,
    handshake: Mutex<EndpointHandshakeSummary>,
    await_registry: await_tree::Registry,
    started_at: Instant,
}

impl TelemetryRegistry {
    pub fn new(await_registry: await_tree::Registry, scheme: &str) -> Arc<Self> {
        Arc::new(Self {
            http: HttpProbeCounters::new(),
            retry_attempts: AtomicU64::new(0),
            retry_backoff_us: AtomicU64::new(0),
            semaphore_wait_us: AtomicU64::new(0),
            cache_lookup_us: AtomicU64::new(0),
            synthetic_body_us: AtomicU64::new(0),
            finalize_crc_us: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_lookups: AtomicU64::new(0),
            logical_bytes_requested: AtomicU64::new(0),
            physical_storage_bytes: AtomicU64::new(0),
            coalesced_gap_waste_bytes: AtomicU64::new(0),
            sub_requests_dispatched: AtomicU64::new(0),
            top_level_ops: AtomicU64::new(0),
            crc_verified_ok: AtomicU64::new(0),
            crc_corrupted: AtomicU64::new(0),
            verbs: Mutex::new(BTreeMap::new()),
            timeseries: Mutex::new(VecDeque::with_capacity(64)),
            handshake: Mutex::new(EndpointHandshakeSummary::local_in_process(scheme)),
            await_registry,
            started_at: Instant::now(),
        })
    }

    pub fn set_handshake_summary(&self, summary: EndpointHandshakeSummary) {
        self.http
            .set_calibrated_cold_handshake_us(summary.estimated_cold_handshake_us());
        if let Ok(mut slot) = self.handshake.lock() {
            *slot = summary;
        }
    }

    pub fn record_verb_sample(
        &self,
        verb: &'static str,
        elapsed: Duration,
        logical_bytes: u64,
        physical_bytes: u64,
        sub_rpcs: u64,
        was_throttled: bool,
    ) {
        let us = (elapsed.as_micros() as u64).clamp(1, 120_000_000);
        self.top_level_ops.fetch_add(1, Ordering::Relaxed);
        self.logical_bytes_requested
            .fetch_add(logical_bytes, Ordering::Relaxed);
        self.physical_storage_bytes
            .fetch_add(physical_bytes, Ordering::Relaxed);
        self.sub_requests_dispatched
            .fetch_add(sub_rpcs.max(1), Ordering::Relaxed);
        self.synthetic_body_us.fetch_add(us, Ordering::Relaxed);

        let mut latest_p99_ms = us as f64 / 1000.0;
        if let Ok(mut map) = self.verbs.lock() {
            let bucket = map.entry(verb).or_insert_with(VerbBucket::new);
            let _ = bucket.hist_us.record(us);
            bucket.count += 1;
            bucket.bytes += physical_bytes.max(logical_bytes);
            if was_throttled {
                bucket.throttled += 1;
            }
            latest_p99_ms = bucket.hist_us.value_at_quantile(0.99) as f64 / 1000.0;
        }

        // 追加滑动时序采样点（最多保留最近 60 个点供 Sparkline 渲染）
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        let elapsed_s = (elapsed_ms as f64 / 1000.0).max(0.01);
        let phys_all = self.physical_storage_bytes.load(Ordering::Relaxed) as f64;
        let mib_s = (phys_all / (1024.0 * 1024.0)) / elapsed_s;
        let inflight = self.http.active_inflight.load(Ordering::Relaxed);

        if let Ok(mut ts) = self.timeseries.lock() {
            let should_push = ts
                .back()
                .map(|last| elapsed_ms.saturating_sub(last.elapsed_ms) >= 40)
                .unwrap_or(true);
            if should_push {
                if ts.len() >= 60 {
                    ts.pop_front();
                }
                ts.push_back(TimeSeriesPoint {
                    elapsed_ms,
                    throughput_mib_s: mib_s,
                    p99_ms: latest_p99_ms,
                    inflight,
                });
            }
        }
    }

    /// 构造带 `RetryEvent<'_>` 通知闭包的 `RetryLayer`（严格对齐 OpenDAL 0.59 签名）
    pub fn make_retry_notify(
        registry: Arc<Self>,
        min_delay_ms: u64,
    ) -> impl Fn(opendal::layers::RetryEvent<'_>) + Send + Sync + 'static {
        move |_event: opendal::layers::RetryEvent<'_>| {
            let attempt = registry.retry_attempts.fetch_add(1, Ordering::Relaxed) + 1;
            let est_backoff_us = (min_delay_ms * 1000).saturating_mul(1_u64 << attempt.min(5));
            registry
                .retry_backoff_us
                .fetch_add(est_backoff_us, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self, cpu_cores: usize, process_rss_mib: u64) -> TelemetryFrame {
        let elapsed_s = self.started_at.elapsed().as_secs_f64().max(0.005);
        let top_ops = self.top_level_ops.load(Ordering::Relaxed).max(1);
        let http_reqs = self.http.total_requests.load(Ordering::Relaxed);
        let sub_rpcs = self.sub_requests_dispatched.load(Ordering::Relaxed).max(top_ops);

        let body_us = if http_reqs > 0 {
            self.http.sum_body_stream_us.load(Ordering::Relaxed) / http_reqs
        } else {
            self.synthetic_body_us.load(Ordering::Relaxed) / top_ops
        };

        let wf = StageWaterfall {
            semaphore_wait_us: self.semaphore_wait_us.load(Ordering::Relaxed) / top_ops,
            cache_lookup_us: self.cache_lookup_us.load(Ordering::Relaxed) / top_ops,
            conn_tls_sign_us: if http_reqs > 0 {
                self.http.sum_conn_tls_us.load(Ordering::Relaxed) / http_reqs
            } else {
                0
            },
            remote_ttfb_us: if http_reqs > 0 {
                self.http.sum_remote_ttfb_us.load(Ordering::Relaxed) / http_reqs
            } else {
                0
            },
            body_stream_us: body_us,
            finalize_crc_us: self.finalize_crc_us.load(Ordering::Relaxed) / top_ops,
        };

        let mut verb_rows = Vec::new();
        if let Ok(map) = self.verbs.lock() {
            for (name, b) in map.iter() {
                let cnt = b.count.max(1) as f64;
                let mean_s = (b.hist_us.mean() / 1_000_000.0).max(0.000_001);
                let avg_bytes_mib = (b.bytes as f64 / cnt) / (1024.0 * 1024.0);
                verb_rows.push(VerbMetricsRow {
                    verb: (*name).to_owned(),
                    count: b.count,
                    ops_per_sec: b.count as f64 / elapsed_s,
                    p50_ms: b.hist_us.value_at_quantile(0.50) as f64 / 1000.0,
                    p90_ms: b.hist_us.value_at_quantile(0.90) as f64 / 1000.0,
                    p95_ms: b.hist_us.value_at_quantile(0.95) as f64 / 1000.0,
                    p99_ms: b.hist_us.value_at_quantile(0.99) as f64 / 1000.0,
                    p999_ms: b.hist_us.value_at_quantile(0.999) as f64 / 1000.0,
                    max_ms: b.hist_us.max() as f64 / 1000.0,
                    stddev_ms: b.hist_us.stdev() / 1000.0,
                    single_stream_mib_s: avg_bytes_mib / mean_s,
                    throttle_429_503_pct: (b.throttled as f64 / cnt) * 100.0,
                });
            }
        }

        let wire_read = self.http.wire_bytes_read.load(Ordering::Relaxed);
        let wire_written = self.http.wire_bytes_written.load(Ordering::Relaxed);
        let logical = self.logical_bytes_requested.load(Ordering::Relaxed).max(1);
        let tracked_phys = self.physical_storage_bytes.load(Ordering::Relaxed);
        let physical = (wire_read + wire_written).max(tracked_phys).max(logical);

        let total_mib = (physical as f64) / (1024.0 * 1024.0);
        let agg_mib_s = total_mib / elapsed_s;

        let lookups = self.cache_lookups.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let cache_hit_pct = if lookups > 0 {
            (hits as f64 / lookups as f64) * 100.0
        } else {
            0.0
        };

        let read_amp = (physical as f64 / logical as f64).max(1.0);
        let effective_rpcs = http_reqs.max(sub_rpcs);
        let rpc_amp = (effective_rpcs as f64 / top_ops as f64).max(1.0);

        let tp = TransportAndHostSnapshot {
            socket_reuse_pct: self.http.socket_reuse_pct(),
            active_inflight: self.http.active_inflight.load(Ordering::Relaxed),
            total_http_requests: effective_rpcs,
            http_200_count: self.http.status_200.load(Ordering::Relaxed),
            http_206_range_count: self.http.status_206.load(Ordering::Relaxed),
            http_403_auth_count: self.http.status_403.load(Ordering::Relaxed),
            http_429_count: self.http.status_429.load(Ordering::Relaxed),
            http_500_count: self.http.status_500.load(Ordering::Relaxed),
            http_503_slowdown_count: self.http.status_503.load(Ordering::Relaxed),
            retry_attempts: self.retry_attempts.load(Ordering::Relaxed),
            retry_backoff_ms: self.retry_backoff_us.load(Ordering::Relaxed) as f64 / 1000.0,
            cache_hit_ratio_pct: cache_hit_pct,
            logical_bytes: logical,
            physical_wire_bytes: physical,
            coalesced_gap_waste_bytes: self.coalesced_gap_waste_bytes.load(Ordering::Relaxed),
            read_amplification: read_amp,
            rpc_amplification: rpc_amp,
            crc32c_verified_ok: self.crc_verified_ok.load(Ordering::Relaxed),
            crc32c_corrupted: self.crc_corrupted.load(Ordering::Relaxed),
            aggregate_mib_s: agg_mib_s,
            mib_per_cpu_core: agg_mib_s / (cpu_cores.max(1) as f64),
            cpu_cores: cpu_cores.max(1),
            process_rss_mib,
            recent_request_ids: self.http.recent_request_ids(),
        };

        let diagnosis = BottleneckDiagnosis::evaluate(&wf, &tp);

        let mut tree_lines = Vec::new();
        for (task_id, tree) in self.await_registry.collect::<u64>().into_iter().take(4) {
            tree_lines.push(format!("[task#{task_id}]\n{tree}"));
        }
        let await_tree_dump = if tree_lines.is_empty() {
            "All instrumented OpenDAL async futures completed cleanly (0 stalled in-flight spans)."
                .to_owned()
        } else {
            tree_lines.join("\n")
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

        TelemetryFrame {
            waterfall: wf,
            handshake: hs,
            verbs: verb_rows,
            timeseries: ts_vec,
            transport: tp,
            diagnosis,
            await_tree_dump,
        }
    }
}
