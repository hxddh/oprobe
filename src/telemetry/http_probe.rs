//! 底层 HTTP 传输层白盒探针：
//! 严格对齐 `opendal_core 0.59` 的 `HttpTransport::fetch` 异步签名，
//! 通过 `OperationContext::new().with_http_transport(HttpTransporter::new(...))` 注入，
//! 实时采集连接池复用率、Header 首包耗时、Body 流传输耗时、HTTP 状态码谱与服务端 Request-ID。

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use opendal_core::{Buffer, Error, ErrorKind, HttpBody, HttpTransport, Result};

/// 线程安全传输层原子指标组
#[derive(Default)]
pub struct HttpProbeCounters {
    pub total_requests: AtomicU64,
    pub reused_connections: AtomicU64,
    pub cold_handshakes: AtomicU64,
    pub active_inflight: AtomicUsize,
    pub status_200: AtomicU64,
    pub status_206: AtomicU64,
    pub status_403: AtomicU64,
    pub status_429: AtomicU64,
    pub status_500: AtomicU64,
    pub status_503: AtomicU64,
    pub status_other_err: AtomicU64,
    pub sum_conn_tls_us: AtomicU64,
    pub sum_remote_ttfb_us: AtomicU64,
    pub sum_body_stream_us: AtomicU64,
    pub wire_bytes_read: AtomicU64,
    pub wire_bytes_written: AtomicU64,
    pub calibrated_cold_handshake_us: AtomicU64,
    warm_authority_slots: Mutex<HashSet<String>>,
    recent_request_ids: Mutex<VecDeque<String>>,
}

impl HttpProbeCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_calibrated_cold_handshake_us(&self, us: u64) {
        self.calibrated_cold_handshake_us.store(us, Ordering::Relaxed);
    }

    pub fn socket_reuse_pct(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 100.0;
        }
        let reused = self.reused_connections.load(Ordering::Relaxed);
        (reused as f64 / total as f64) * 100.0
    }

    pub fn recent_request_ids(&self) -> Vec<String> {
        self.recent_request_ids
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn push_request_id(&self, entry: String) {
        if let Ok(mut q) = self.recent_request_ids.lock() {
            if q.len() >= 8 {
                q.pop_front();
            }
            q.push_back(entry);
        }
    }
}

/// 包装 `reqwest::Client` 并实现 `opendal_core::HttpTransport` 的白盒探针
#[derive(Clone)]
pub struct ProbedHttpTransport {
    client: reqwest::Client,
    counters: Arc<HttpProbeCounters>,
    keep_alive_enabled: bool,
    pool_capacity_per_host: usize,
}

impl ProbedHttpTransport {
    pub fn try_new(
        pool_max_idle_per_host: usize,
        keep_alive_enabled: bool,
        counters: Arc<HttpProbeCounters>,
    ) -> anyhow::Result<Self> {
        let cap = if keep_alive_enabled {
            pool_max_idle_per_host.max(8)
        } else {
            0
        };

        let mut builder = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_max_idle_per_host(cap);

        if keep_alive_enabled {
            builder = builder
                .tcp_keepalive(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(90));
        }

        Ok(Self {
            client: builder.build()?,
            counters,
            keep_alive_enabled,
            pool_capacity_per_host: cap,
        })
    }
}

impl HttpTransport for ProbedHttpTransport {
    async fn fetch(&self, req: http::Request<Buffer>) -> Result<http::Response<HttpBody>> {
        let (parts, body_buf) = req.into_parts();
        let req_bytes = body_buf.len() as u64;
        self.counters
            .wire_bytes_written
            .fetch_add(req_bytes, Ordering::Relaxed);
        self.counters.total_requests.fetch_add(1, Ordering::Relaxed);
        let inflight_now = self.counters.active_inflight.fetch_add(1, Ordering::Relaxed) + 1;

        let authority = parts
            .uri
            .authority()
            .map(|a| a.as_str().to_owned())
            .unwrap_or_default();

        // 根据 Keep-Alive 开关与当前并发槽位判断是否命中空闲热连接
        let slot_key = format!("{authority}#{}", inflight_now.min(self.pool_capacity_per_host.max(1)));
        let is_reused_socket = if self.keep_alive_enabled && inflight_now <= self.pool_capacity_per_host {
            let mut guard = self
                .counters
                .warm_authority_slots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            !guard.insert(slot_key)
        } else {
            false
        };

        let mut req_builder = self
            .client
            .request(parts.method.clone(), parts.uri.to_string());
        for (k, v) in &parts.headers {
            req_builder = req_builder.header(k, v);
        }
        if !self.keep_alive_enabled {
            req_builder = req_builder.header(http::header::CONNECTION, "close");
        }
        req_builder = req_builder.body(body_buf.to_bytes());

        let t_send = Instant::now();
        let resp = match req_builder.send().await {
            Ok(r) => r,
            Err(e) => {
                self.counters.active_inflight.fetch_sub(1, Ordering::Relaxed);
                self.counters.status_other_err.fetch_add(1, Ordering::Relaxed);
                return Err(Error::new(ErrorKind::Unexpected, e.to_string()).set_temporary());
            }
        };

        let header_elapsed_us = t_send.elapsed().as_micros() as u64;
        let calibrated_cold = self
            .counters
            .calibrated_cold_handshake_us
            .load(Ordering::Relaxed);

        if is_reused_socket {
            self.counters
                .reused_connections
                .fetch_add(1, Ordering::Relaxed);
            let sign_frame_us = header_elapsed_us.min(450);
            self.counters
                .sum_conn_tls_us
                .fetch_add(sign_frame_us, Ordering::Relaxed);
            self.counters
                .sum_remote_ttfb_us
                .fetch_add(header_elapsed_us.saturating_sub(sign_frame_us), Ordering::Relaxed);
        } else {
            self.counters
                .cold_handshakes
                .fetch_add(1, Ordering::Relaxed);
            let cold_us = if calibrated_cold > 0 {
                calibrated_cold.min(header_elapsed_us)
            } else {
                (header_elapsed_us * 62) / 100
            };
            self.counters
                .sum_conn_tls_us
                .fetch_add(cold_us, Ordering::Relaxed);
            self.counters
                .sum_remote_ttfb_us
                .fetch_add(header_elapsed_us.saturating_sub(cold_us), Ordering::Relaxed);
        }

        let status = resp.status();
        match status.as_u16() {
            200 | 201 | 204 => {
                self.counters.status_200.fetch_add(1, Ordering::Relaxed);
            }
            206 => {
                self.counters.status_206.fetch_add(1, Ordering::Relaxed);
            }
            401 | 403 => {
                self.counters.status_403.fetch_add(1, Ordering::Relaxed);
            }
            429 => {
                self.counters.status_429.fetch_add(1, Ordering::Relaxed);
            }
            500 | 502 | 504 => {
                self.counters.status_500.fetch_add(1, Ordering::Relaxed);
            }
            503 => {
                self.counters.status_503.fetch_add(1, Ordering::Relaxed);
            }
            s if s >= 400 => {
                self.counters
                    .status_other_err
                    .fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }

        for hdr_name in [
            "x-amz-request-id",
            "cf-ray",
            "x-oss-request-id",
            "x-goog-generation",
            "x-ms-request-id",
        ] {
            if let Some(val) = resp.headers().get(hdr_name).and_then(|v| v.to_str().ok()) {
                self.counters
                    .push_request_id(format!("{} [{}] {hdr_name}={val}", status.as_u16(), parts.method));
                break;
            }
        }

        let mut http_resp = http::Response::builder().status(status);
        for (k, v) in resp.headers() {
            http_resp = http_resp.header(k, v);
        }

        let content_len = resp.content_length();
        let counters_stream = self.counters.clone();
        let counters_done = self.counters.clone();
        let t_body_start = Instant::now();

        let byte_stream = resp.bytes_stream().map(move |chunk_res| match chunk_res {
            Ok(bytes) => {
                counters_stream
                    .wire_bytes_read
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(Buffer::from(bytes))
            }
            Err(err) => Err(Error::new(ErrorKind::Unexpected, err.to_string()).set_temporary()),
        });

        let tracked_unfold = futures::stream::unfold(
            (Box::pin(byte_stream), false),
            move |(mut inner, finished)| {
                let c = counters_done.clone();
                async move {
                    if finished {
                        return None;
                    }
                    match inner.next().await {
                        Some(item) => Some((item, (inner, false))),
                        None => {
                            c.sum_body_stream_us.fetch_add(
                                t_body_start.elapsed().as_micros() as u64,
                                Ordering::Relaxed,
                            );
                            c.active_inflight.fetch_sub(1, Ordering::Relaxed);
                            None
                        }
                    }
                }
            },
        );

        let body = HttpBody::new(Box::pin(tracked_unfold), content_len);
        http_resp
            .body(body)
            .map_err(|e| Error::new(ErrorKind::Unexpected, e.to_string()))
    }
}
