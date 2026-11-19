//! `oprobe` v1.0.0 — Production White-Box Cloud Storage Stack Microscope, TCP/TLS/HTTP
//! Profiler, Multi-Cloud FinOps Analyzer, and Closed-Loop SLA Auto-Tuner powered by Apache OpenDAL.
//!
//! ## Core Architecture
//! - [`transport`]: Pure-Rust `HyperWhiteboxTransport` + `ProbedHttpConnector` measuring true per-socket
//!   DNS lookup, TCP SYN/ACK RTT, TLS 1.2/1.3 + ALPN handshake, connection pool reuse vs cold dials,
//!   post-write gateway TTFB, body streaming, and kernel-level wire RX/TX bytes without heuristics;
//! - [`layers`]: OpenDAL middleware instrumentation (`ProbedSemaphore`, pure-Rust `WhiteboxCacheLayer`
//!   backed by `moka` W-TinyLFU, and `WhiteboxObserveLayer` capturing `CompleteMultipartUpload` merge time);
//! - [`telemetry`]: 7-stage latency waterfall, isolated per-verb HDR histograms, live 10ms `await-tree`
//!   slow-request async call-stack capture, root-cause diagnosis, and human-friendly error hints;
//! - [`workload`]: Anti-deduplication Xoshiro256++ payloads, CRC32C verification, deterministic &
//!   time-window streaming load generation, and guaranteed remote test object auto-cleanup;
//! - [`cost`]: 5-cloud FinOps TCO engine (AWS S3, Cloudflare R2, Google GCS, Alibaba OSS, Backblaze B2);
//! - [`autotune`]: Closed-loop `429/503`-resilient SLA auto-tuner with binary cliff refinement & codegen;
//! - [`report`]: JSON/Markdown/CSV/Terminal exporters and `oprobe diff` CI regression gate;
//! - [`testbed`]: Built-in S3-compatible HTTP/1.1 + HTTP/2 + TLS 1.3 object storage & chaos server.

pub mod autotune;
pub mod cli;
pub mod cost;
pub mod layers;
pub mod net_probe;
pub mod report;
pub mod telemetry;
pub mod testbed;
pub mod tls;
pub mod transport;
pub mod tui;
pub mod workload;
