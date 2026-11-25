//! `oprobe lab`: a local S3-compatible server (HTTP/1.1, HTTP/2, TLS 1.3) with programmable
//! faults — added latency, stragglers, bandwidth caps, slow multipart completion and real
//! `429`/`503 SlowDown` throttling — for reproducing storage problems without a cloud account.
//!
//! Supports PutObject (incl. user metadata, content type, CRC32C checksums and conditional
//! `If-None-Match: *` / `If-Match` writes), CopyObject, GetObject (incl. `Range`, suffix and
//! open-ended ranges, `416` when unsatisfiable, conditional reads), HeadObject, DeleteObject,
//! DeleteObjects, ListObjectsV2 / ListObjects (prefix, delimiter, `max-keys` pagination with
//! continuation tokens / markers), presigned GETs (signatures are not checked) and the
//! multipart upload API (with S3's minimum part size when configured). Not supported:
//! versioning, redirects, ACLs, SSE.
//!
//! Faults: added latency, stragglers, bandwidth caps, slow multipart completion, `429`/`503`
//! throttling (periodic or above a concurrency), `500` errors and connection resets.
//!
//! [`Scenario`] names fault presets; `lab://NAME` targets start one in-process ([`shared`]).

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use http::{Method, Request, Response, StatusCode};
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoServerBuilder;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub mod faults;

pub use faults::{KeyRateLimitFault, QueueFault, RateLimitFault};

use crate::config::{DatasetSpec, Preset, Profile};
use crate::net::tls::build_tls_acceptor;

pub mod caps;
pub use caps::LabLimits;

struct LabTls {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

/// A CA and a `localhost` server certificate generated once per process: valid from yesterday for
/// 30 days, ECDSA P-256, SANs `localhost` / `127.0.0.1` / `::1`, server-auth EKU — what strict
/// verifiers (macOS Security.framework, the platform verifier reqwest uses) accept.
fn lab_tls() -> &'static LabTls {
    static TLS: std::sync::OnceLock<LabTls> = std::sync::OnceLock::new();
    TLS.get_or_init(|| generate_lab_tls().expect("generate lab certificates"))
}

fn generate_lab_tls() -> std::result::Result<LabTls, rcgen::Error> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose, date_time_ymd,
    };
    let today = crate::util::utc_today();
    let (y0, m0, d0) = crate::util::civil_from_days(today - 1);
    let (y1, m1, d1) = crate::util::civil_from_days(today + 30);
    let validity = |p: &mut CertificateParams| {
        p.not_before = date_time_ymd(y0 as i32, m0 as u8, d0 as u8);
        p.not_after = date_time_ymd(y1 as i32, m1 as u8, d1 as u8);
    };

    let ca_key = KeyPair::generate()?;
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name
        .push(DnType::CommonName, "oprobe lab CA (generated)");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    validity(&mut ca);
    let ca_cert = ca.self_signed(&ca_key)?;

    let key = KeyPair::generate()?;
    let mut leaf = CertificateParams::new(vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ])?;
    leaf.distinguished_name
        .push(DnType::CommonName, "localhost");
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    validity(&mut leaf);
    let issuer = Issuer::from_params(&ca, &ca_key);
    let cert = leaf.signed_by(&key, &issuer)?;
    Ok(LabTls {
        ca_pem: ca_cert.pem(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// PEM of the CA that signs the lab's TLS certificate in this process (trust it as `ca_cert`).
pub fn lab_ca_pem() -> &'static str {
    &lab_tls().ca_pem
}

/// The protocol a lab server speaks (`lab://NAME?proto=…`, design v5 §3, M6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LabProto {
    /// Plain HTTP/1.1 and HTTP/2 with prior knowledge (h2c), detected per connection (the
    /// default: `--http h1` and `--http h2` both work).
    #[default]
    H2c,
    /// Plain HTTP/1.1 only.
    H1,
    /// TLS 1.3 with ALPN offering `h2` and `http/1.1`.
    Https,
    /// TLS 1.3 with ALPN preferring `h2` (`http/1.1` stays offered: oprobe's own setup
    /// traffic — preflight, dataset, cleanup — uses OpenDAL's default HTTP/1.1 client).
    /// `LabConfig::h2_only` makes a server that refuses every other protocol.
    H2,
}

impl LabProto {
    pub const ALL: [LabProto; 4] = [LabProto::H2c, LabProto::H1, LabProto::Https, LabProto::H2];

    pub fn name(self) -> &'static str {
        match self {
            LabProto::H2c => "h2c",
            LabProto::H1 => "h1",
            LabProto::Https => "https",
            LabProto::H2 => "h2",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, crate::errors::OprobeError> {
        let l = s.trim().to_ascii_lowercase();
        LabProto::ALL
            .into_iter()
            .find(|p| p.name() == l)
            .ok_or_else(|| {
                crate::input::invalid(format!("unknown lab protocol `{}`", s.trim()))
                    .with_hint("use proto=h1, h2c (plain), https or h2 (TLS with ALPN)")
            })
    }

    pub fn tls(self) -> bool {
        matches!(self, LabProto::Https | LabProto::H2)
    }

    /// The server settings of this protocol.
    pub fn apply(self, c: &mut LabConfig) {
        c.enable_tls = self.tls();
        c.enable_http2 = !matches!(self, LabProto::H1);
    }
}

/// Fault profile for the lab server.
#[derive(Clone, Debug, Default)]
pub struct LabConfig {
    pub enable_tls: bool,
    pub enable_http2: bool,
    /// With TLS: offer only `h2` in ALPN (clients that cannot speak HTTP/2 fail the handshake).
    pub h2_only: bool,
    pub base_ttfb_delay: Duration,
    pub multipart_merge_delay: Duration,
    pub straggler_every_n: usize,
    pub straggler_delay: Duration,
    pub max_inflight_before_503: Option<usize>,
    pub throttle_every_n_requests: Option<usize>,
    pub throttle_status_code: u16,
    /// Answer a throttled 503 with `ServiceUnavailable` ("reduce your request rate", as S3
    /// also does) instead of `SlowDown`.
    pub throttle_service_unavailable: bool,
    pub stream_bandwidth_kib_s: Option<u64>,
    /// Answer throttled requests immediately and close the connection without reading the
    /// request body, as real gateways do. Clients then often see a reset or abort while still
    /// uploading instead of the 503 (always on Windows), and must retry the upload.
    pub throttle_early_close: bool,
    /// Answer every n-th request with `500 InternalError`.
    pub error_every_n: Option<usize>,
    /// Drop the connection (no response) on every n-th request.
    pub reset_every_n: Option<usize>,
    /// Reject multipart uploads whose parts (except the last) are smaller than this, like S3
    /// (`EntityTooSmall`, 5 MiB). 0 = any size.
    pub min_part_size: usize,
    /// A token bucket over every request (a rate limit; optionally time-varying).
    pub rate_limit: Option<RateLimitFault>,
    /// One token bucket per key or prefix (a hot key throttles, the others are served).
    pub key_rate_limit: Option<KeyRateLimitFault>,
    /// An M/M/c queue in front of the handler (latency grows with the requests in flight).
    pub queue: Option<QueueFault>,
    /// Cap the bandwidth of all response bodies together (KiB/s, shared by every stream).
    pub aggregate_bandwidth_kib_s: Option<u64>,
    /// Answer this share of requests with a random `503 SlowDown` (half) or `500` (half),
    /// unrelated to the load.
    pub server_error_ratio: Option<f64>,
}

/// Ask Windows for 1 ms timer resolution while a lab runs in this process (once; it lasts for
/// the process). An idle Tokio runtime otherwise parks with the default 15.6 ms resolution while
/// a busy one fires timers promptly, so the lab's delays — and with them its capacity — would
/// depend on how many workers keep the runtime busy (an M/M/4 queue then seemed to gain goodput
/// from more workers). Elsewhere a no-op.
fn fine_timers() {
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn timeBeginPeriod(period_ms: u32) -> u32;
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        // SAFETY: `timeBeginPeriod` takes a plain integer and has no memory-safety
        // preconditions; a failure (TIMERR_NOCANDO) leaves the default resolution.
        ONCE.call_once(|| unsafe {
            timeBeginPeriod(1);
        });
    }
}

/// Named fault presets for `lab://NAME` targets, `oprobe lab --scenario` and `oprobe demo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// No faults (S3 semantics only).
    #[default]
    Clean,
    /// Modest latency, throttling above a concurrency and ~2.5% TTFB stragglers.
    Demo,
    /// `503 SlowDown` above 8 concurrent requests.
    Throttle,
    /// ~3% of requests wait 400 ms before answering.
    Stragglers,
    /// 20 ms latency and a 4 MiB/s cap per response stream.
    Lossy,
    /// 4% `500 InternalError` and 2% connection resets.
    Flaky,
    /// The demo's limit without stragglers, for the parquet preset (HEAD, footer, then 4
    /// parallel column reads): 503 above 24 in flight, so 6 parquet workers fit and 8 throttle.
    Parquet,
    /// A token bucket: 300 requests/s (burst 20), whatever the concurrency.
    RateLimit,
    /// One token bucket per key (40 requests/s): zipf reads make the hottest keys throttle.
    HotKey,
    /// An aggregate bandwidth cap: 16 MiB/s shared by every response stream.
    Bandwidth,
    /// An M/M/4 queue (10 ms mean service): latency grows with the requests in flight.
    Queueing,
    /// A rate limit that drops from 600 to 150 requests/s 1.5 s after the first request.
    TimeVarying,
    /// 5% random 5xx unrelated to the load (half 503 SlowDown, half 500 InternalError).
    Noisy,
}

impl Scenario {
    pub const ALL: [Scenario; 13] = [
        Scenario::Clean,
        Scenario::Demo,
        Scenario::Throttle,
        Scenario::Stragglers,
        Scenario::Lossy,
        Scenario::Flaky,
        Scenario::Parquet,
        Scenario::RateLimit,
        Scenario::HotKey,
        Scenario::Bandwidth,
        Scenario::Queueing,
        Scenario::TimeVarying,
        Scenario::Noisy,
    ];

    /// The scenarios `oprobe lab` and the app offer; the others stay runnable by name (the
    /// tests use them) but are not listed.
    pub const LISTED: [Scenario; 6] = [
        Scenario::Demo,
        Scenario::Throttle,
        Scenario::Stragglers,
        Scenario::Bandwidth,
        Scenario::Flaky,
        Scenario::RateLimit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Clean => "clean",
            Scenario::Demo => "demo",
            Scenario::Throttle => "throttle",
            Scenario::Stragglers => "stragglers",
            Scenario::Lossy => "lossy",
            Scenario::Flaky => "flaky",
            Scenario::Parquet => "parquet",
            Scenario::RateLimit => "ratelimit",
            Scenario::HotKey => "hotkey",
            Scenario::Bandwidth => "bandwidth",
            Scenario::Queueing => "queueing",
            Scenario::TimeVarying => "timevarying",
            Scenario::Noisy => "noisy",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Scenario::Clean => "no faults: S3 semantics only",
            Scenario::Demo => {
                "8 ms latency, 2.5% stragglers (+250 ms), 503 SlowDown above 40 concurrent requests; \
                 default workload: small-files, 64 × 64 KiB, 28 workers, 0.5 s warmup, 8 s"
            }
            Scenario::Throttle => "5 ms latency, 503 SlowDown above 8 concurrent requests",
            Scenario::Stragglers => "5 ms latency, 3% of requests wait 400 ms",
            Scenario::Lossy => "20 ms latency, responses capped at 4 MiB/s per stream",
            Scenario::Flaky => "5 ms latency, 4% 500 InternalError, 2% connection resets",
            Scenario::Parquet => {
                "8 ms latency, 503 SlowDown above 24 concurrent requests, no stragglers; \
                 default workload: the parquet preset, 8 workers"
            }
            Scenario::RateLimit => {
                "5 ms latency, 503 SlowDown above 300 requests/s (token bucket, burst 20); \
                 default workload: whole-object reads of 64 × 16 KiB, 16 workers"
            }
            Scenario::HotKey => {
                "5 ms latency, 503 SlowDown above 40 requests/s per key; \
                 default workload: zipf(1.2) reads of 64 × 16 KiB, 16 workers"
            }
            Scenario::Bandwidth => {
                "5 ms latency, all response bodies share 16 MiB/s; \
                 default workload: reads of 32 × 512 KiB, 8 workers"
            }
            Scenario::Queueing => {
                "M/M/4 queue, 10 ms mean service time (latency grows with the requests in flight); \
                 default workload: reads of 64 × 16 KiB, 16 workers"
            }
            Scenario::TimeVarying => {
                "5 ms latency, 503 SlowDown above 600 requests/s, dropping to 150 requests/s \
                 1.5 s after the first request; default workload: reads of 64 × 16 KiB, 16 workers"
            }
            Scenario::Noisy => {
                "5 ms latency, 5% random 5xx (half 503 SlowDown, half 500) unrelated to the load; \
                 default workload: reads of 64 × 16 KiB, 8 workers"
            }
        }
    }

    /// Typed: see [`crate::input::lab_scenario`].
    pub fn parse(s: &str) -> Result<Self> {
        Ok(crate::input::lab_scenario(s)?)
    }

    /// The server configuration of this scenario (plain HTTP/1.1 + h2c, S3 part-size rules).
    pub fn config(self) -> LabConfig {
        let ms = Duration::from_millis;
        let base = LabConfig {
            enable_http2: true,
            throttle_status_code: 503,
            min_part_size: 5 * 1024 * 1024,
            ..Default::default()
        };
        match self {
            Scenario::Clean => base,
            Scenario::Demo => LabConfig {
                base_ttfb_delay: ms(8),
                max_inflight_before_503: Some(DEMO_THROTTLE_ABOVE),
                straggler_every_n: DEMO_STRAGGLER_EVERY,
                straggler_delay: ms(DEMO_STRAGGLER_MS),
                ..base
            },
            Scenario::Throttle => LabConfig {
                base_ttfb_delay: ms(5),
                max_inflight_before_503: Some(8),
                ..base
            },
            Scenario::Stragglers => LabConfig {
                base_ttfb_delay: ms(5),
                straggler_every_n: 33,
                straggler_delay: ms(400),
                ..base
            },
            Scenario::Lossy => LabConfig {
                base_ttfb_delay: ms(20),
                stream_bandwidth_kib_s: Some(4096),
                ..base
            },
            Scenario::Flaky => LabConfig {
                base_ttfb_delay: ms(5),
                error_every_n: Some(25),
                reset_every_n: Some(50),
                ..base
            },
            Scenario::Parquet => LabConfig {
                base_ttfb_delay: ms(8),
                max_inflight_before_503: Some(24),
                ..base
            },
            Scenario::RateLimit => LabConfig {
                base_ttfb_delay: ms(5),
                rate_limit: Some(RateLimitFault {
                    requests_per_s: 300.0,
                    burst: 20,
                    after: None,
                }),
                ..base
            },
            Scenario::HotKey => LabConfig {
                base_ttfb_delay: ms(5),
                key_rate_limit: Some(KeyRateLimitFault {
                    requests_per_s: 40.0,
                    burst: 4,
                    by_prefix: false,
                }),
                ..base
            },
            Scenario::Bandwidth => LabConfig {
                base_ttfb_delay: ms(5),
                aggregate_bandwidth_kib_s: Some(16 * 1024),
                ..base
            },
            Scenario::Queueing => LabConfig {
                queue: Some(QueueFault {
                    servers: 4,
                    service: ms(10),
                }),
                ..base
            },
            Scenario::TimeVarying => LabConfig {
                base_ttfb_delay: ms(5),
                rate_limit: Some(RateLimitFault {
                    requests_per_s: 600.0,
                    burst: 20,
                    after: Some((ms(1500), 150.0)),
                }),
                ..base
            },
            Scenario::Noisy => LabConfig {
                base_ttfb_delay: ms(5),
                server_error_ratio: Some(0.05),
                ..base
            },
        }
    }
}

/// Requests in flight above which `lab://demo` answers `503 SlowDown`. Above its workers with
/// room for the stragglers' re-issues: the server still counts a request the client cut until it
/// answers, so the top finding's remedy (TailCutLayer) proves without throttling (design 7 A2:
/// a candidate over `--max-throttle` is not a proof).
pub const DEMO_THROTTLE_ABOVE: usize = 40;
/// Workers of the `lab://demo` default workload: below the lab's in-flight limit, so the profile
/// is about its stragglers, and the top finding proves on goodput.
pub const DEMO_CONCURRENCY: usize = 28;
/// Every Nth request of `lab://demo` is a straggler.
pub const DEMO_STRAGGLER_EVERY: usize = 40;
/// Extra delay of a `lab://demo` straggler.
pub const DEMO_STRAGGLER_MS: u64 = 250;
/// Workers of the `lab://parquet` default workload (4 requests in flight per op: 6 fit, 8 throttle).
pub const PARQUET_CONCURRENCY: usize = 8;

/// The workload a `lab://NAME` target runs when no flag says otherwise (design v4 §10): what
/// `oprobe demo` runs is exactly `oprobe run lab://demo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScenarioDefaults {
    pub preset: Option<Preset>,
    pub dataset: Option<DatasetSpec>,
    pub warmup: Option<Duration>,
    pub duration: Option<Duration>,
    pub concurrency: Option<usize>,
    /// Key popularity (`None`: the profile's).
    pub access: Option<crate::config::Access>,
    /// Operation mix (`None`: the preset's).
    pub mix: Option<Vec<crate::config::OpSpec>>,
}

impl Scenario {
    /// This scenario's default workload (empty: the profile defaults).
    pub fn defaults(self) -> ScenarioDefaults {
        match self {
            Scenario::Demo => ScenarioDefaults {
                preset: Some(Preset::SmallFiles),
                dataset: Some(DatasetSpec {
                    objects: 64,
                    object_size: crate::util::ByteSize::kib(64),
                    ..DatasetSpec::default()
                }),
                warmup: Some(Duration::from_millis(500)),
                duration: Some(crate::config::defaults().demo_duration.get()),
                concurrency: Some(DEMO_CONCURRENCY),
                ..Default::default()
            },
            Scenario::Parquet => ScenarioDefaults {
                preset: Some(Preset::RangeReads),
                concurrency: Some(PARQUET_CONCURRENCY),
                ..Default::default()
            },
            Scenario::RateLimit | Scenario::Queueing | Scenario::TimeVarying => {
                reads(64, crate::util::ByteSize::kib(16), 16, None)
            }
            Scenario::HotKey => reads(
                64,
                crate::util::ByteSize::kib(16),
                16,
                Some(crate::config::Access::Zipf(1.2)),
            ),
            Scenario::Noisy => reads(64, crate::util::ByteSize::kib(16), 8, None),
            Scenario::Bandwidth => reads(32, crate::util::ByteSize::kib(512), 8, None),
            _ => ScenarioDefaults::default(),
        }
    }

    /// Set this scenario's default workload on `p` (flags applied afterwards win).
    pub fn apply_defaults(self, p: &mut Profile) {
        let d = self.defaults();
        let w = &mut p.workload;
        if let Some(pr) = d.preset {
            w.preset = pr;
            w.mix.clear();
            w.dataset = None;
        }
        if let Some(ds) = d.dataset {
            w.dataset = Some(ds);
        }
        if let Some(v) = d.warmup {
            w.warmup = crate::util::HumanDuration(v);
        }
        if let Some(v) = d.duration {
            w.duration = crate::util::HumanDuration(v);
        }
        if let Some(a) = d.access {
            w.access = a;
        }
        if let Some(m) = d.mix {
            w.mix = m;
        }
        if let Some(c) = d.concurrency {
            p.workload.concurrency = c;
        }
    }

    /// The scenario a `lab://NAME[/root]` URL names (`None`: not a lab URL, or unknown).
    pub fn of_url(url: &str) -> Option<Scenario> {
        let lower = url.trim().to_ascii_lowercase();
        let rest = lower.strip_prefix("lab://")?;
        let name = rest.split(['/', '?', '#']).next().unwrap_or_default();
        crate::input::lab_scenario(name).ok()
    }
}

/// A read-only workload over `objects` objects of `size` (the limit scenarios' default).
fn reads(
    objects: usize,
    size: crate::util::ByteSize,
    concurrency: usize,
    access: Option<crate::config::Access>,
) -> ScenarioDefaults {
    ScenarioDefaults {
        preset: Some(Preset::SmallFiles),
        dataset: Some(DatasetSpec {
            objects,
            object_size: size,
            ..DatasetSpec::default()
        }),
        warmup: Some(Duration::from_millis(300)),
        duration: Some(Duration::from_secs(3)),
        concurrency: Some(concurrency),
        access,
        mix: Some(vec![crate::config::OpSpec {
            weight: 1,
            op: crate::config::OpKind::Read { size: None },
        }]),
    }
}

/// Bucket name of `lab://` targets.
pub const LAB_BUCKET: &str = "lab";

/// The process-wide lab server of `scenario`, started on first use on its own runtime thread
/// (it lives as long as the process; `lab://NAME` targets resolve to it).
pub fn shared(scenario: Scenario) -> Result<&'static LabHandle> {
    shared_with(scenario, LabProto::default())
}

/// [`shared`] speaking `proto` (one server per scenario and protocol).
pub fn shared_with(scenario: Scenario, proto: LabProto) -> Result<&'static LabHandle> {
    use std::sync::{Mutex, OnceLock};
    type Servers = HashMap<(Scenario, LabProto), &'static LabHandle>;
    static SERVERS: OnceLock<Mutex<Servers>> = OnceLock::new();
    let mut m = SERVERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(h) = m.get(&(scenario, proto)) {
        return Ok(h);
    }
    let mut config = scenario.config();
    proto.apply(&mut config);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("oprobe-lab-{}", scenario.name()))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("oprobe-lab")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::Error::from(e)));
                    return;
                }
            };
            rt.block_on(async move {
                let res = LabServer::spawn("127.0.0.1:0", config).await;
                let ok = res.is_ok();
                let _ = tx.send(res);
                if ok {
                    std::future::pending::<()>().await;
                }
            });
        })?;
    let handle = rx
        .recv()
        .map_err(|_| anyhow::anyhow!("the lab server thread exited"))??;
    let h: &'static LabHandle = Box::leak(Box::new(handle));
    m.insert((scenario, proto), h);
    Ok(h)
}

impl LabConfig {
    pub fn clean_https_h2() -> Self {
        Self {
            enable_tls: true,
            enable_http2: true,
            throttle_status_code: 503,
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct StoredObject {
    data: Bytes,
    etag: String,
    content_type: Option<String>,
    /// `x-amz-meta-*` headers (lower-case names, as received).
    meta: Vec<(String, String)>,
    /// Base64 CRC32C the client sent (`x-amz-checksum-crc32c`).
    checksum_crc32c: Option<String>,
    last_modified: std::time::SystemTime,
}

impl StoredObject {
    fn new(data: Bytes, etag: String) -> Self {
        Self {
            data,
            etag,
            content_type: None,
            meta: Vec::new(),
            checksum_crc32c: None,
            last_modified: std::time::SystemTime::now(),
        }
    }

    /// Object headers of GET/HEAD responses.
    fn headers(&self, mut b: http::response::Builder, checksum: bool) -> http::response::Builder {
        b = b
            .header("etag", &self.etag)
            .header("last-modified", httpdate::fmt_http_date(self.last_modified));
        if let Some(ct) = &self.content_type {
            b = b.header("content-type", ct);
        }
        for (k, v) in &self.meta {
            b = b.header(k.as_str(), v.as_str());
        }
        if checksum && let Some(c) = &self.checksum_crc32c {
            b = b.header("x-amz-checksum-crc32c", c);
        }
        b
    }
}

/// Base64 (standard alphabet, padded).
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// S3's `x-amz-checksum-crc32c` value of `data`.
pub fn crc32c_b64(data: &[u8]) -> String {
    base64(&crate::util::crc32c(data).to_be_bytes())
}

/// An `If-Match` / `If-None-Match` value matches `etag` (`*`, or a list of quoted tags).
fn etag_matches(cond: &str, etag: &str) -> bool {
    let bare = |s: &str| {
        s.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_owned()
    };
    cond.split(',')
        .any(|c| c.trim() == "*" || bare(c) == bare(etag))
}

fn s3_error(status: StatusCode, code: &str, msg: &str, req_id: &str) -> Response<BoxBody> {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Error><Code>{code}</Code><Message>{}</Message><RequestId>{req_id}</RequestId></Error>",
        xml_escape(msg)
    );
    Response::builder()
        .status(status)
        .header("content-type", "application/xml")
        .header("x-amz-request-id", req_id)
        .body(full_body(xml))
        .unwrap()
}

struct MultipartUploadState {
    bucket: String,
    key: String,
    parts: BTreeMap<usize, Bytes>,
    meta: Vec<(String, String)>,
    content_type: Option<String>,
}

const MAX_COMPLETED_UPLOADS: usize = 1024;

#[derive(Default)]
struct CompletedUploads {
    order: std::collections::VecDeque<String>,
    by_id: HashMap<String, String>,
}

impl CompletedUploads {
    fn insert(&mut self, id: String, xml: String) {
        if !self.by_id.contains_key(&id) {
            if self.order.len() >= MAX_COMPLETED_UPLOADS
                && let Some(evicted) = self.order.pop_front()
            {
                self.by_id.remove(&evicted);
            }
            self.order.push_back(id.clone());
        }
        self.by_id.insert(id, xml);
    }

    fn get(&self, id: &str) -> Option<&String> {
        self.by_id.get(id)
    }
}

struct S3Store {
    objects: RwLock<BTreeMap<String, StoredObject>>,
    uploads: RwLock<HashMap<String, MultipartUploadState>>,
    completed_uploads: RwLock<CompletedUploads>,
    req_seq: AtomicU64,
    upload_seq: AtomicU64,
    active_inflight: AtomicUsize,
    peak_inflight: AtomicUsize,
    emitted_throttles: AtomicU64,
    faults: RwLock<LabConfig>,
    limits: faults::LimitState,
    /// Body, store and connection caps (`caps`).
    caps: LabLimits,
    usage: caps::StoreUsage,
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full_body(bytes: impl Into<Bytes>) -> BoxBody {
    Full::new(bytes.into()).boxed()
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query_map(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(percent_decode(k), percent_decode(v));
            }
            None => {
                map.insert(percent_decode(pair), String::new());
            }
        }
    }
    map
}

fn xml_unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..semi];
            let decoded = match entity {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "amp" => Some('&'),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn extract_xml_tag_values(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        if let Some(end) = after.find(&close) {
            out.push(xml_unescape(after[..end].trim()));
            rest = &after[end + close.len()..];
        } else {
            break;
        }
    }
    out
}

fn parse_http_range_header(hdr: &str, total_len: usize) -> Option<(usize, usize)> {
    let spec = hdr.trim().strip_prefix("bytes=")?;
    let first_range = spec.split(',').next()?.trim();
    let (start_s, end_s) = first_range.split_once('-')?;
    if start_s.is_empty() {
        let suffix: usize = end_s.parse().ok()?;
        if suffix == 0 || total_len == 0 {
            return None;
        }
        let start = total_len.saturating_sub(suffix);
        Some((start, total_len - 1))
    } else {
        let start: usize = start_s.parse().ok()?;
        if start >= total_len {
            return None;
        }
        let end = if end_s.is_empty() {
            total_len - 1
        } else {
            end_s.parse::<usize>().ok()?.min(total_len - 1)
        };
        if start > end {
            None
        } else {
            Some((start, end))
        }
    }
}

struct ServerInflightGuard {
    store: Arc<S3Store>,
}

impl Drop for ServerInflightGuard {
    fn drop(&mut self) {
        self.store.active_inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn build_shaped_body(
    payload: Bytes,
    bandwidth_kib_s: Option<u64>,
    aggregate: Option<(Arc<S3Store>, u64)>,
) -> BoxBody {
    let rate_kib = bandwidth_kib_s.filter(|&r| r > 0);
    let aggregate = aggregate.filter(|a| a.1 > 0);
    if rate_kib.is_none() && aggregate.is_none() {
        return full_body(payload);
    }
    if payload.len() <= 16_384 && aggregate.is_none() {
        return full_body(payload);
    }
    let chunk_size = 32 * 1024_usize;
    let sleep_per_chunk_us = rate_kib.map_or(0, |rate_kib| {
        ((chunk_size as f64 / (rate_kib as f64 * 1024.0)) * 1_000_000.0) as u64
    });

    let stream = futures::stream::unfold((payload, 0_usize), move |(buf, pos)| {
        let aggregate = aggregate.clone();
        async move {
            if pos >= buf.len() {
                return None;
            }
            let end = (pos + chunk_size).min(buf.len());
            let slice = buf.slice(pos..end);
            let mut wait = Duration::ZERO;
            if pos > 0 && sleep_per_chunk_us > 0 {
                wait = Duration::from_micros(sleep_per_chunk_us);
            }
            if let Some((store, kib)) = &aggregate {
                // The shared budget paces every stream together: the wait is the later of
                // the two caps.
                let shared = store
                    .limits
                    .pace_bytes(slice.len(), *kib, std::time::Instant::now());
                wait = wait.max(shared);
            }
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            Some((Ok::<_, Infallible>(Frame::data(slice)), (buf, end)))
        }
    });
    StreamBody::new(stream).boxed()
}

/// One request; a straggler is delayed after its request was read and handled, right before
/// the response goes out: the delay is time to first byte, never upload time.
async fn serve_s3_request(
    req: Request<Incoming>,
    store: Arc<S3Store>,
) -> std::result::Result<Response<BoxBody>, std::io::Error> {
    // In flight until the response leaves (a sleeping straggler counts).
    let cur_inflight = store.active_inflight.fetch_add(1, Ordering::AcqRel) + 1;
    store
        .peak_inflight
        .fetch_max(cur_inflight, Ordering::Relaxed);
    let _inflight_guard = ServerInflightGuard {
        store: store.clone(),
    };
    let req_version = req.version();
    let upload_guard = match caps::admit(&store, req.method(), req.headers()) {
        Ok(guard) => guard,
        Err((status, code, msg)) => {
            let req_id = format!(
                "oprobe-s3-req-{:06}",
                store.req_seq.fetch_add(1, Ordering::Relaxed) + 1
            );
            let mut resp = s3_error(status, code, &msg, &req_id);
            // The body is not read: ask an HTTP/1.x client to close the connection (RFC 9113 §8.2.2
            // forbids connection-specific headers on HTTP/2+).
            if req_version < http::Version::HTTP_2 {
                resp.headers_mut().insert(
                    http::header::CONNECTION,
                    http::HeaderValue::from_static("close"),
                );
            }
            return Ok(resp);
        }
    };
    let mut straggle = Duration::ZERO;
    let resp = handle_s3_request(req, &store, upload_guard, cur_inflight, &mut straggle).await;
    if !straggle.is_zero() {
        tokio::time::sleep(straggle).await;
    }
    resp
}

async fn handle_s3_request<'a>(
    req: Request<Incoming>,
    store: &'a Arc<S3Store>,
    mut upload_guard: Option<caps::InflightGuard<'a>>,
    cur_inflight: usize,
    straggle: &mut Duration,
) -> std::result::Result<Response<BoxBody>, std::io::Error> {
    let seq = store.req_seq.fetch_add(1, Ordering::Relaxed) + 1;
    let req_id = format!("oprobe-s3-req-{seq:06}");
    let faults = store.faults.read().expect("faults lock").clone();

    let req_version = req.version();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let qmap = parse_query_map(uri.query());
    let is_delete_cleanup = method == Method::DELETE || qmap.contains_key("delete");

    // 1. Optional 429 / 503 SlowDown Rate-Limit Injection (never block cleanup DELETEs so teardown verify stays clean)
    if !is_delete_cleanup {
        let over_conc = faults
            .max_inflight_before_503
            .map(|limit| cur_inflight > limit)
            .unwrap_or(false);
        let periodic_throttle = faults
            .throttle_every_n_requests
            .filter(|&n| n > 0)
            .map(|n| seq.is_multiple_of(n as u64))
            .unwrap_or(false);

        // Limit faults (design v5 §4): token buckets, per key or prefix, and random 5xx.
        let now = std::time::Instant::now();
        let path = uri.path();
        let rate_limited = faults
            .rate_limit
            .as_ref()
            .is_some_and(|f| !store.limits.admit_global(f, now));
        let key_limited = !rate_limited
            && faults
                .key_rate_limit
                .as_ref()
                .is_some_and(|f| !store.limits.admit_key(f, path, now));
        let random_5xx = faults
            .server_error_ratio
            .filter(|r| *r > 0.0)
            .is_some_and(|r| store.limits.uniform() < r);
        if random_5xx && store.limits.uniform() < 0.5 {
            let _ = req.into_body().collect().await;
            return Ok(s3_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "We encountered an internal error. Please try again.",
                &req_id,
            ));
        }
        if over_conc || periodic_throttle || rate_limited || key_limited || random_5xx {
            store.emitted_throttles.fetch_add(1, Ordering::Relaxed);
            // By default read the request body first so every client sees the 503 itself;
            // `throttle_early_close` reproduces gateways that answer and hang up mid-upload.
            if !faults.throttle_early_close {
                let _ = req.into_body().collect().await;
            }
            let status_u16 = if faults.throttle_status_code == 429 {
                429
            } else {
                503
            };
            let code_str = if status_u16 == 429 {
                "TooManyRequests"
            } else if faults.throttle_service_unavailable {
                "ServiceUnavailable"
            } else {
                "SlowDown"
            };
            let err_xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <Error><Code>{code_str}</Code>\
                 <Message>Please reduce your request rate.</Message>\
                 <RequestId>{req_id}</RequestId></Error>"
            );
            let mut resp = Response::builder();
            if faults.throttle_early_close && req_version < http::Version::HTTP_2 {
                resp = resp.header("connection", "close");
            }
            return Ok(resp
                .status(StatusCode::from_u16(status_u16).unwrap_or(StatusCode::SERVICE_UNAVAILABLE))
                .header("content-type", "application/xml")
                .header("retry-after", "1")
                .header("x-amz-request-id", &req_id)
                .body(full_body(err_xml))
                .unwrap());
        }
    }

    // 1b. Injected server errors and connection resets (never on cleanup DELETEs).
    if !is_delete_cleanup {
        let every = |n: Option<usize>, offset: u64| {
            n.filter(|&n| n > 0)
                .is_some_and(|n| (seq + offset).is_multiple_of(n as u64))
        };
        // Resets fall one request before the errors' multiples, so the two never land on the
        // same request: `error_every_n: 25` is 4 % errors whatever the resets (K4).
        if every(faults.reset_every_n, 1) {
            // An error from the service makes hyper drop the connection without a response.
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "lab: injected connection reset",
            ));
        }
        if every(faults.error_every_n, 0) {
            let _ = req.into_body().collect().await;
            return Ok(s3_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "We encountered an internal error. Please try again.",
                &req_id,
            ));
        }
    }

    // 2. Base Gateway TTFB & Straggler Tail Latency Injection
    if !faults.base_ttfb_delay.is_zero() {
        tokio::time::sleep(faults.base_ttfb_delay).await;
    }
    // 2b. An M/M/c queue: wait for a server, then the service time (the server is free again
    // at its deadline, before the body streams).
    if let (Some(q), false) = (&faults.queue, is_delete_cleanup) {
        store.limits.serve_queued(q).await;
    }
    if !is_delete_cleanup
        && faults.straggler_every_n > 0
        && seq.is_multiple_of(faults.straggler_every_n as u64)
        && !faults.straggler_delay.is_zero()
    {
        *straggle = faults.straggler_delay;
    }

    let trimmed_path = uri.path().trim_start_matches('/');
    if trimmed_path.is_empty() {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("x-amz-request-id", &req_id)
            .body(full_body(Bytes::new()))
            .unwrap());
    }

    let (bucket, raw_key) = match trimmed_path.split_once('/') {
        Some((b, k)) => (percent_decode(b), percent_decode(k)),
        None => (percent_decode(trimmed_path), String::new()),
    };

    // Bucket-level operations (`ListObjectsV2` and batch `POST /{bucket}?delete`)
    if raw_key.is_empty() {
        if method == Method::POST && qmap.contains_key("delete") {
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let xml_str = String::from_utf8_lossy(&body_bytes);
            let keys = extract_xml_tag_values(&xml_str, "Key");
            let quiet = extract_xml_tag_values(&xml_str, "Quiet")
                .first()
                .is_some_and(|q| q == "true");
            let mut deleted = String::new();
            if let Ok(mut objs) = store.objects.write() {
                for k in keys {
                    let full_k = format!("{bucket}/{}", k.trim_start_matches('/'));
                    objs.remove(&full_k);
                    if !quiet {
                        deleted
                            .push_str(&format!("<Deleted><Key>{}</Key></Deleted>", xml_escape(&k)));
                    }
                }
            }
            let resp_xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                <DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{deleted}</DeleteResult>"
            );
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(resp_xml))
                .unwrap());
        }

        if method == Method::HEAD {
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::GET && qmap.contains_key("versions") {
            // An unversioned bucket: every object has one version, `null`.
            let prefix = qmap.get("prefix").cloned().unwrap_or_default();
            let full = format!("{bucket}/{prefix}");
            let mut items = String::new();
            if let Ok(objs) = store.objects.read() {
                for (k, o) in objs.range(full.clone()..) {
                    if !k.starts_with(&full) {
                        break;
                    }
                    items.push_str(&format!(
                        "<Version><Key>{}</Key><VersionId>null</VersionId><IsLatest>true</IsLatest><LastModified>2026-11-19T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Version>",
                        xml_escape(&k[bucket.len() + 1..]),
                        xml_escape(&o.etag),
                        o.data.len()
                    ));
                }
            }
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Name>{}</Name><Prefix>{}</Prefix><IsTruncated>false</IsTruncated>{items}</ListVersionsResult>",
                xml_escape(&bucket),
                xml_escape(&prefix)
            );
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(xml))
                .unwrap());
        }

        if method == Method::GET {
            let prefix = qmap.get("prefix").cloned().unwrap_or_default();
            let delimiter = qmap.get("delimiter").cloned().filter(|d| !d.is_empty());
            let max_keys: usize = qmap
                .get("max-keys")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000)
                .clamp(1, 1000);
            let start_after = qmap
                .get("continuation-token")
                .or_else(|| qmap.get("start-after"))
                .or_else(|| qmap.get("marker"))
                .cloned()
                .unwrap_or_default();
            let v1 = !qmap.contains_key("list-type");
            let bucket_prefix = format!("{bucket}/");
            let full_search_prefix = format!("{bucket_prefix}{prefix}");
            let mut items_xml = String::new();
            let mut common: Vec<String> = Vec::new();
            let mut count = 0_usize;
            let mut last_key = String::new();
            let mut truncated = false;

            if let Ok(objs) = store.objects.read() {
                for (full_k, obj) in objs.range(full_search_prefix.clone()..) {
                    if !full_k.starts_with(&full_search_prefix) {
                        break;
                    }
                    let rel_key = &full_k[bucket_prefix.len()..];
                    if !start_after.is_empty() && rel_key <= start_after.as_str() {
                        continue;
                    }
                    if let Some(d) = &delimiter {
                        let rest = &rel_key[prefix.len()..];
                        if let Some(pos) = rest.find(d.as_str()) {
                            let cp = format!("{prefix}{}", &rest[..pos + d.len()]);
                            if common.last() == Some(&cp) {
                                last_key = rel_key.to_owned();
                                continue;
                            }
                            if count >= max_keys {
                                truncated = true;
                                break;
                            }
                            common.push(cp);
                            count += 1;
                            last_key = rel_key.to_owned();
                            continue;
                        }
                    }
                    if count >= max_keys {
                        truncated = true;
                        break;
                    }
                    count += 1;
                    last_key = rel_key.to_owned();
                    items_xml.push_str(&format!(
                        "<Contents><Key>{}</Key><LastModified>2026-11-19T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                        xml_escape(rel_key),
                        xml_escape(&obj.etag),
                        obj.data.len()
                    ));
                }
            }
            let prefixes_xml: String = common
                .iter()
                .map(|p| {
                    format!(
                        "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                        xml_escape(p)
                    )
                })
                .collect();
            let token_xml = match (truncated, v1) {
                (false, _) => String::new(),
                (true, false) => format!(
                    "<NextContinuationToken>{}</NextContinuationToken>",
                    xml_escape(&last_key)
                ),
                (true, true) => format!("<NextMarker>{}</NextMarker>", xml_escape(&last_key)),
            };

            let list_xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{count}</KeyCount>\
                 <MaxKeys>{max_keys}</MaxKeys><IsTruncated>{truncated}</IsTruncated>{token_xml}{items_xml}{prefixes_xml}</ListBucketResult>",
                xml_escape(&bucket),
                xml_escape(&prefix)
            );
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/xml")
                .header("x-amz-request-id", &req_id)
                .body(full_body(list_xml))
                .unwrap());
        }
    }

    let store_key = format!("{bucket}/{raw_key}");

    // Multipart Upload APIs
    if method == Method::POST && qmap.contains_key("uploads") {
        let upload_id = format!(
            "mpu-{}",
            store.upload_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        if let Ok(mut ups) = store.uploads.write() {
            ups.insert(
                upload_id.clone(),
                MultipartUploadState {
                    bucket: bucket.clone(),
                    key: raw_key.clone(),
                    parts: BTreeMap::new(),
                    meta: user_meta(req.headers()),
                    content_type: header(req.headers(), "content-type"),
                },
            );
        }
        let init_xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket><Key>{}</Key><UploadId>{upload_id}</UploadId>\
             </InitiateMultipartUploadResult>",
            xml_escape(&bucket),
            xml_escape(&raw_key)
        );
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/xml")
            .header("x-amz-request-id", &req_id)
            .body(full_body(init_xml))
            .unwrap());
    }

    if let Some(upload_id) = qmap.get("uploadId") {
        if method == Method::PUT {
            let part_num: usize = qmap
                .get("partNumber")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let part_bytes = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            let crc = crate::util::crc32c(&part_bytes);
            let etag = format!("\"{crc:08x}\"");

            let known = match store.uploads.write() {
                Ok(mut ups) => match ups.get_mut(upload_id) {
                    Some(st) => {
                        st.parts.insert(part_num, part_bytes);
                        true
                    }
                    None => false,
                },
                Err(_) => false,
            };
            if !known {
                return Ok(s3_error(
                    StatusCode::NOT_FOUND,
                    "NoSuchUpload",
                    "The specified upload does not exist.",
                    &req_id,
                ));
            }
            if let Some(g) = upload_guard.take() {
                g.commit();
            }
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("etag", etag)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::POST {
            let _ = req.into_body().collect().await;
            if !faults.multipart_merge_delay.is_zero() {
                tokio::time::sleep(faults.multipart_merge_delay).await;
            }
            enum MergeStage<'s> {
                TooSmall,
                Ready(MultipartUploadState, caps::InflightGuard<'s>),
                NotFound,
            }
            let stage = match store.uploads.write() {
                Ok(mut ups) => match ups.get(upload_id) {
                    Some(st)
                        if faults.min_part_size > 0
                            && st
                                .parts
                                .values()
                                .rev()
                                .skip(1)
                                .any(|p| p.len() < faults.min_part_size) =>
                    {
                        MergeStage::TooSmall
                    }
                    Some(st) => {
                        let total_len: usize = st.parts.values().map(|b| b.len()).sum();
                        let guard = store.usage.hold_inflight(total_len as u64);
                        let st = ups.remove(upload_id).expect("checked Some");
                        MergeStage::Ready(st, guard)
                    }
                    None => MergeStage::NotFound,
                },
                Err(_) => MergeStage::NotFound,
            };
            match stage {
                MergeStage::TooSmall => {
                    // Like S3: the upload stays open (the client may abort it).
                    return Ok(s3_error(
                        StatusCode::BAD_REQUEST,
                        "EntityTooSmall",
                        "Your proposed upload is smaller than the minimum allowed object size.",
                        &req_id,
                    ));
                }
                MergeStage::Ready(st, merge_guard) => {
                    let total_len: usize = st.parts.values().map(|b| b.len()).sum();
                    let mut merged = BytesMut::with_capacity(total_len);
                    let num_parts = st.parts.len();
                    for (_, p_bytes) in st.parts {
                        merged.extend_from_slice(&p_bytes);
                    }
                    let frozen = merged.freeze();
                    let crc = crate::util::crc32c(&frozen);
                    let etag = format!("\"{crc:08x}-{num_parts}\"");

                    if let Ok(mut objs) = store.objects.write() {
                        let mut o = StoredObject::new(frozen, etag.clone());
                        o.meta = st.meta;
                        o.content_type = st.content_type;
                        objs.insert(format!("{}/{}", st.bucket, st.key), o);
                    }
                    merge_guard.commit();
                    let comp_xml = format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                         <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                         <Location>http://localhost/{}/{}</Location>\
                         <Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag>\
                         </CompleteMultipartUploadResult>",
                        xml_escape(&st.bucket),
                        xml_escape(&st.key),
                        xml_escape(&st.bucket),
                        xml_escape(&st.key),
                        xml_escape(&etag)
                    );
                    if let Ok(mut done) = store.completed_uploads.write() {
                        done.insert(upload_id.clone(), comp_xml.clone());
                    }
                    return Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/xml")
                        .header("x-amz-request-id", &req_id)
                        .body(full_body(comp_xml))
                        .unwrap());
                }
                MergeStage::NotFound => {}
            }
            if let Some(prev_xml) = store
                .completed_uploads
                .read()
                .ok()
                .and_then(|done| done.get(upload_id).cloned())
            {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/xml")
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(prev_xml))
                    .unwrap());
            }
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }

        if method == Method::DELETE {
            if let Ok(mut ups) = store.uploads.write() {
                ups.remove(upload_id);
            }
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap());
        }
    }

    // Standard Single-Object CRUD
    match method {
        Method::PUT => put_object(req, store, upload_guard, &bucket, store_key, &req_id).await,
        Method::HEAD | Method::GET => {
            let is_head = method == Method::HEAD;
            let h = req.headers();
            let found = store
                .objects
                .read()
                .ok()
                .and_then(|objs| objs.get(&store_key).cloned());
            let Some(obj) = found else {
                if is_head {
                    return Ok(Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header("x-amz-request-id", &req_id)
                        .body(full_body(Bytes::new()))
                        .unwrap());
                }
                return Ok(s3_error(
                    StatusCode::NOT_FOUND,
                    "NoSuchKey",
                    "The specified key does not exist.",
                    &req_id,
                ));
            };
            if let Some(m) = header(h, "if-match")
                && !etag_matches(&m, &obj.etag)
            {
                return Ok(s3_error(
                    StatusCode::PRECONDITION_FAILED,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                    &req_id,
                ));
            }
            if let Some(m) = header(h, "if-none-match")
                && etag_matches(&m, &obj.etag)
            {
                return Ok(obj
                    .headers(Response::builder(), false)
                    .status(StatusCode::NOT_MODIFIED)
                    .header("x-amz-request-id", &req_id)
                    .body(full_body(Bytes::new()))
                    .unwrap());
            }
            let checksum =
                header(h, "x-amz-checksum-mode").is_some_and(|v| v.eq_ignore_ascii_case("enabled"));
            let total_len = obj.data.len();
            let range_hdr = header(h, "range");
            let range = match &range_hdr {
                Some(r) if total_len > 0 => match parse_http_range_header(r, total_len) {
                    Some(r) => Some(r),
                    None => {
                        let mut resp = s3_error(
                            StatusCode::RANGE_NOT_SATISFIABLE,
                            "InvalidRange",
                            "The requested range is not satisfiable",
                            &req_id,
                        );
                        resp.headers_mut().insert(
                            http::header::CONTENT_RANGE,
                            http::HeaderValue::from_str(&format!("bytes */{total_len}")).unwrap(),
                        );
                        return Ok(resp);
                    }
                },
                _ => None,
            };
            let (status, slice, content_range) = match range {
                Some((start, end)) => (
                    StatusCode::PARTIAL_CONTENT,
                    obj.data.slice(start..=end),
                    Some(format!("bytes {start}-{end}/{total_len}")),
                ),
                None => (StatusCode::OK, obj.data.clone(), None),
            };
            let mut b = obj
                .headers(Response::builder(), checksum && content_range.is_none())
                .status(status)
                .header("content-length", slice.len())
                .header("accept-ranges", "bytes")
                .header("x-amz-request-id", &req_id);
            if let Some(cr) = content_range {
                b = b.header("content-range", cr);
            }
            let body = if is_head {
                full_body(Bytes::new())
            } else {
                build_shaped_body(
                    slice,
                    faults.stream_bandwidth_kib_s,
                    faults.aggregate_bandwidth_kib_s.map(|k| (store.clone(), k)),
                )
                .await
            };
            Ok(b.body(body).unwrap())
        }
        Method::DELETE => {
            if let Ok(mut objs) = store.objects.write() {
                objs.remove(&store_key);
            }
            Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", &req_id)
                .body(full_body(Bytes::new()))
                .unwrap())
        }
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("x-amz-request-id", &req_id)
            .body(full_body(Bytes::new()))
            .unwrap()),
    }
}

fn header(h: &http::HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

fn user_meta(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .filter(|(k, _)| k.as_str().starts_with("x-amz-meta-"))
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
}

/// PutObject / CopyObject, with conditional writes and checksums.
async fn put_object<'a>(
    req: Request<Incoming>,
    store: &'a S3Store,
    upload_guard: Option<caps::InflightGuard<'a>>,
    bucket: &str,
    store_key: String,
    req_id: &str,
) -> std::result::Result<Response<BoxBody>, std::io::Error> {
    let h = req.headers().clone();
    let if_none_match = header(&h, "if-none-match");
    let if_match = header(&h, "if-match");
    let copy_source = header(&h, "x-amz-copy-source");
    let checksum = header(&h, "x-amz-checksum-crc32c");
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let fail = |status, code: &str, msg: &str| Ok(s3_error(status, code, msg, req_id));

    // Reserve store quota for CopyObject BEFORE taking `objects.write()` so a scan never deadlocks.
    let copy_reservation = if let Some(raw_src) = &copy_source {
        let src = percent_decode(raw_src.split('?').next().unwrap_or_default());
        let src = src.trim_start_matches('/').to_owned();
        let src_len = store
            .objects
            .read()
            .map_err(|_| std::io::Error::other("lab store poisoned"))?
            .get(&src)
            .map(|o| o.data.len() as u64);
        let Some(src_len) = src_len else {
            return fail(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            );
        };
        if src_len > store.caps.max_object_bytes {
            return fail(
                StatusCode::BAD_REQUEST,
                "EntityTooLarge",
                &format!(
                    "Your proposed upload exceeds the maximum allowed size ({} bytes).",
                    store.caps.max_object_bytes
                ),
            );
        }
        match store
            .usage
            .try_reserve(store, src_len, store.caps.max_store_bytes)
        {
            Ok(g) => Some((src, src_len, g)),
            Err(held) => {
                return fail(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "LabStoreFull",
                    &format!(
                        "The lab store is full ({held} bytes held, limit {} bytes): delete objects or restart it with a larger --max-store-size.",
                        store.caps.max_store_bytes
                    ),
                );
            }
        }
    } else {
        None
    };

    let mut objs = store
        .objects
        .write()
        .map_err(|_| std::io::Error::other("lab store poisoned"))?;
    // Preconditions are checked and the object replaced under one lock: concurrent conditional
    // writers see exactly one winner, like S3.
    let existing = objs.get(&store_key);
    if if_none_match.as_deref().is_some_and(|v| v.trim() == "*") && existing.is_some() {
        return fail(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "At least one of the pre-conditions you specified did not hold",
        );
    }
    if let Some(m) = &if_match {
        match existing {
            None => {
                return fail(
                    StatusCode::NOT_FOUND,
                    "NoSuchKey",
                    "The specified key does not exist.",
                );
            }
            Some(o) if !etag_matches(m, &o.etag) => {
                return fail(
                    StatusCode::PRECONDITION_FAILED,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            Some(_) => {}
        }
    }

    if let Some((src, reserved_len, copy_guard)) = copy_reservation {
        let Some(mut o) = objs.get(&src).cloned() else {
            return fail(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            );
        };
        if o.data.len() as u64 > reserved_len {
            // Source was concurrently replaced by a larger object; enforce cap strictly.
            return fail(
                StatusCode::CONFLICT,
                "OperationAborted",
                "The source object changed during copy; please retry.",
            );
        }
        if header(&h, "x-amz-metadata-directive").is_some_and(|d| d.eq_ignore_ascii_case("REPLACE"))
        {
            o.meta = user_meta(&h);
            o.content_type = header(&h, "content-type");
        }
        o.last_modified = std::time::SystemTime::now();
        let etag = o.etag.clone();
        objs.insert(store_key, o);
        drop(objs);
        copy_guard.commit();
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <CopyObjectResult><LastModified>{}</LastModified><ETag>{}</ETag></CopyObjectResult>",
            crate::util::now_rfc3339(),
            xml_escape(&etag)
        );
        let _ = bucket;
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/xml")
            .header("x-amz-request-id", req_id)
            .body(full_body(xml))
            .unwrap());
    }

    if let Some(c) = &checksum
        && *c != crc32c_b64(&body_bytes)
    {
        return fail(
            StatusCode::BAD_REQUEST,
            "BadDigest",
            "The CRC32C you specified did not match the calculated checksum.",
        );
    }
    let crc = crate::util::crc32c(&body_bytes);
    let etag = format!("\"{crc:08x}\"");
    let mut o = StoredObject::new(body_bytes, etag.clone());
    o.meta = user_meta(&h);
    o.content_type = header(&h, "content-type");
    o.checksum_crc32c = checksum.clone();
    objs.insert(store_key, o);
    drop(objs);
    if let Some(g) = upload_guard {
        g.commit();
    }
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header("etag", etag)
        .header("x-amz-request-id", req_id);
    if let Some(c) = checksum {
        b = b.header("x-amz-checksum-crc32c", c);
    }
    Ok(b.body(full_body(Bytes::new())).unwrap())
}

/// Live handle to a running [`LabServer`] instance.
pub struct LabHandle {
    pub addr: SocketAddr,
    pub endpoint_url: String,
    store: Arc<S3Store>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl LabHandle {
    /// Update fault-injection parameters dynamically at runtime.
    pub fn update_faults(&self, next: LabConfig) {
        if let Ok(mut f) = self.store.faults.write() {
            *f = next;
        }
    }

    /// Keys of the objects currently stored (`bucket/key`).
    pub fn stored_keys(&self) -> Vec<String> {
        self.store
            .objects
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Multipart uploads started and neither completed nor aborted.
    pub fn open_uploads(&self) -> usize {
        self.store.uploads.read().map(|m| m.len()).unwrap_or(0)
    }

    /// Number of stored objects currently alive in the S3 testbed (used to prove zero leaked data after cleanup).
    pub fn stored_object_count(&self) -> usize {
        self.store.objects.read().map(|m| m.len()).unwrap_or(0)
    }

    /// Total `429`/`503` throttle responses emitted by this server.
    pub fn emitted_throttles(&self) -> u64 {
        self.store.emitted_throttles.load(Ordering::Relaxed)
    }

    /// Peak concurrent server-side requests observed.
    pub fn peak_server_inflight(&self) -> usize {
        self.store.peak_inflight.load(Ordering::Relaxed)
    }

    /// Profile targeting this lab server (credentials are dummies; TLS trusts the lab CA).
    pub fn profile(&self, bucket: &str) -> Profile {
        let mut opts = BTreeMap::new();
        opts.insert("region".to_owned(), "us-east-1".to_owned());
        opts.insert("endpoint".to_owned(), self.endpoint_url.clone());
        opts.insert("access_key_id".to_owned(), "oprobe-lab".to_owned());
        opts.insert(
            "secret_access_key".to_owned(),
            "oprobe-lab-secret".to_owned(),
        );
        opts.insert("disable_config_load".to_owned(), "true".to_owned());
        if self.endpoint_url.starts_with("https://") {
            opts.insert("ca_cert_pem".to_owned(), lab_ca_pem().to_owned());
        }
        let mut op = crate::operator::OperatorSpec::from_url(format!("s3://{bucket}/bench"));
        op.options = opts;
        op.http.proxy = Some("none".to_owned());
        Profile::from_operator(op)
    }

    /// Total requests served.
    pub fn requests_served(&self) -> u64 {
        self.store.req_seq.load(Ordering::Relaxed)
    }
}

impl Drop for LabHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn an ephemeral or bound S3-compatible HTTP/HTTPS chaos server.
pub struct LabServer;

impl LabServer {
    /// Spawn with the default [`LabLimits`] (loopback only).
    pub async fn spawn(bind_addr: &str, faults: LabConfig) -> Result<LabHandle> {
        Self::spawn_with(bind_addr, faults, LabLimits::default()).await
    }

    /// Spawn with explicit exposure limits.
    pub async fn spawn_with(
        bind_addr: &str,
        mut faults: LabConfig,
        limits: LabLimits,
    ) -> Result<LabHandle> {
        fine_timers();
        if faults.throttle_status_code == 0 {
            faults.throttle_status_code = 503;
        }
        if let Ok(addr) = bind_addr.parse::<SocketAddr>() {
            caps::check_bind(addr, limits.allow_remote)?;
        } else {
            for addr in tokio::net::lookup_host(bind_addr).await? {
                caps::check_bind(addr, limits.allow_remote)?;
            }
        }
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        caps::check_bind(local_addr, limits.allow_remote)?;
        let connections = Arc::new(tokio::sync::Semaphore::new(limits.max_connections.max(1)));

        let tls_acceptor = if faults.enable_tls {
            let tls = lab_tls();
            Some(if faults.h2_only {
                crate::net::tls::build_tls_acceptor_alpn(&tls.cert_pem, &tls.key_pem, &[b"h2"])?
            } else {
                build_tls_acceptor(&tls.cert_pem, &tls.key_pem, faults.enable_http2)?
            })
        } else {
            None
        };

        let scheme = if faults.enable_tls { "https" } else { "http" };
        // Advertise the address actually bound. `localhost` would make clients try the other
        // address family first (`::1` vs `127.0.0.1`), which costs a Happy Eyeballs fallback on
        // every new connection — reqwest waits 300 ms, and Windows takes ~2 s to refuse `::1`.
        let advertised = if local_addr.ip().is_unspecified() {
            std::net::SocketAddr::new(
                if local_addr.is_ipv6() {
                    std::net::Ipv6Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv4Addr::LOCALHOST.into()
                },
                local_addr.port(),
            )
        } else {
            local_addr
        };
        let endpoint_url = format!("{scheme}://{advertised}");

        let store = Arc::new(S3Store {
            objects: RwLock::new(BTreeMap::new()),
            uploads: RwLock::new(HashMap::new()),
            completed_uploads: RwLock::new(CompletedUploads::default()),
            req_seq: AtomicU64::new(0),
            upload_seq: AtomicU64::new(0),
            active_inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            emitted_throttles: AtomicU64::new(0),
            faults: RwLock::new(faults.clone()),
            limits: faults::LimitState::default(),
            caps: limits,
            usage: caps::StoreUsage::default(),
        });

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let store_bg = store.clone();
        let allow_h2 = faults.enable_http2;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    accepted = listener.accept() => {
                        let Ok((tcp, _peer)) = accepted else {
                            continue;
                        };
                        // Over the connection cap: close it at once.
                        let Ok(permit) = connections.clone().try_acquire_owned() else {
                            drop(tcp);
                            continue;
                        };
                        let _ = tcp.set_nodelay(true);
                        let st = store_bg.clone();
                        let tls_acc = tls_acceptor.clone();

                        tokio::spawn(async move {
                            let _permit = permit;
                            let svc = service_fn(move |req| serve_s3_request(req, st.clone()));
                            let mut builder = AutoServerBuilder::new(TokioExecutor::new());
                            builder.http2().adaptive_window(true);
                            // A timer enables hyper's 30 s header-read timeout (slow clients
                            // cannot hold a connection open forever before sending a request).
                            builder.http1().timer(hyper_util::rt::TokioTimer::new());
                            if !allow_h2 {
                                let io_plain_or_tls = tls_acc;
                                if let Some(acc) = io_plain_or_tls {
                                    if let Ok(tls_s) = acc.accept(tcp).await {
                                        let _ = hyper::server::conn::http1::Builder::new()
                                            .timer(hyper_util::rt::TokioTimer::new())
                                            .keep_alive(true)
                                            .serve_connection(TokioIo::new(tls_s), svc)
                                            .await;
                                    }
                                } else {
                                    let _ = hyper::server::conn::http1::Builder::new()
                                        .timer(hyper_util::rt::TokioTimer::new())
                                        .keep_alive(true)
                                        .serve_connection(TokioIo::new(tcp), svc)
                                        .await;
                                }
                                return;
                            }

                            if let Some(acc) = tls_acc {
                                if let Ok(tls_s) = acc.accept(tcp).await {
                                    let _ = builder.serve_connection(TokioIo::new(tls_s), svc).await;
                                }
                            } else {
                                let _ = builder.serve_connection(TokioIo::new(tcp), svc).await;
                            }
                        });
                    }
                }
            }
        });

        Ok(LabHandle {
            addr: local_addr,
            endpoint_url,
            store,
            shutdown_tx: Some(shutdown_tx),
        })
    }
}

#[cfg(test)]
mod cert_tests {
    use super::*;

    #[test]
    fn parses_the_lab_certificate_chain() {
        use rustls_pki_types::pem::PemObject;
        let pem = lab_ca_pem();
        let ders: Vec<rustls_pki_types::CertificateDer<'static>> =
            rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let chain = crate::net::tlsinfo::CertChain::from_der(&ders);
        assert_eq!(chain.certs.len(), 1);
        let c = &chain.certs[0];
        assert_eq!(c.subject, "oprobe lab CA (generated)");
        assert_eq!(c.issuer, c.subject);
        assert!((28..=31).contains(&c.expires_in_days), "{c:?}");
        assert!(c.not_after.ends_with('Z'));
        assert_eq!(chain.total_bytes, c.der_bytes);
        assert_eq!(chain.min_expiry_days(), Some(c.expires_in_days));
    }

    #[test]
    fn batch_delete_xml_preserves_plus_percent_and_entities() {
        let keys = [
            "dir/a+b.parquet",
            "dir/a%2Bb.parquet",
            "dir/x&y<z>\"'.bin",
            "dir/nested&amp;lt;.bin",
        ];
        let body: String = keys
            .iter()
            .map(|k| format!("<Object><Key>{}</Key></Object>", xml_escape(k)))
            .collect();
        let xml = format!("<Delete>{body}</Delete>");
        let parsed = extract_xml_tag_values(&xml, "Key");
        assert_eq!(parsed, keys);
    }

    #[test]
    fn concurrent_store_reservations_never_exceed_cap_or_leak_on_drop() {
        let store = S3Store {
            objects: RwLock::new(BTreeMap::new()),
            uploads: RwLock::new(HashMap::new()),
            completed_uploads: RwLock::new(CompletedUploads::default()),
            req_seq: AtomicU64::new(0),
            upload_seq: AtomicU64::new(0),
            active_inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            emitted_throttles: AtomicU64::new(0),
            faults: RwLock::new(LabConfig::default()),
            limits: faults::LimitState::default(),
            caps: LabLimits {
                max_store_bytes: 100,
                ..LabLimits::default()
            },
            usage: caps::StoreUsage::default(),
        };
        // Two concurrent 60 B uploads against a 100 B cap: only the first is admitted while
        // in flight, even before anything enters `store.objects`.
        let g1 = store
            .usage
            .try_reserve(&store, 60, 100)
            .expect("first fits");
        assert!(store.usage.try_reserve(&store, 60, 100).is_err());
        // Dropping `g1` (e.g. a throttled or aborted upload) releases the reservation cleanly.
        drop(g1);
        let g2 = store
            .usage
            .try_reserve(&store, 60, 100)
            .expect("released after drop");
        store.objects.write().unwrap().insert(
            "b/k".into(),
            StoredObject::new(Bytes::from(vec![0u8; 60]), "\"0\"".into()),
        );
        g2.commit();
        assert!(store.usage.try_reserve(&store, 50, 100).is_err());
        assert!(store.usage.try_reserve(&store, 40, 100).is_ok());
    }
}
