//! Production storage workload engine, sparse range coalescer, streaming time-window runner,
//! one-command storage URI parser, and guaranteed remote test data cleanup.

pub mod cleanup;

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use opendal::layers::{AwaitTreeLayer, ConcurrentLimitLayer, RetryLayer, TailCutLayer, TimeoutLayer};
use opendal::Operator;
use opendal_core::{HttpTransporter, OperationContext};
use rand::{RngCore, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::sync::mpsc;
use url::Url;

use crate::autotune::{SlaPolicy, TuneKnob};
use crate::cost::CloudPricingProfile;
use crate::layers::{ProbedSemaphore, WhiteboxCacheLayer, WhiteboxObserveLayer};
use crate::net_probe::probe_endpoint_handshakes_with_tls;
use crate::telemetry::{MeasurementPhase, OpTraceScope, TelemetryFrame, TelemetryRegistry};
use crate::tls::TlsClientOptions;
use crate::transport::hyper_transport::install_default_hyper_transport;
use crate::transport::{HyperTransportOptions, HyperWhiteboxTransport};
pub use cleanup::{sweep_all_oprobe_prefixes, RemoteObjectTracker};

static OPENDAL_REGISTRY_INIT: Once = Once::new();

/// Initialize OpenDAL's global `OperatorRegistry` and install our pure-Rust `HyperWhiteboxTransport`.
pub fn ensure_opendal_registry() {
    OPENDAL_REGISTRY_INIT.call_once(|| {
        install_default_hyper_transport();
        opendal::init_default_registry();
    });
}

/// Parse human-friendly duration strings (`"10s"`, `"500ms"`, `"1.5m"`, or plain seconds `"5"`).
pub fn parse_duration_str(raw: &str) -> Result<Duration> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        anyhow::bail!("duration string cannot be empty");
    }
    if let Some(ms) = s.strip_suffix("ms") {
        let val: f64 = ms.trim().parse().context("invalid milliseconds duration")?;
        anyhow::ensure!(val > 0.0, "duration must be > 0");
        return Ok(Duration::from_secs_f64(val / 1000.0));
    }
    if let Some(mins) = s.strip_suffix('m') {
        let val: f64 = mins.trim().parse().context("invalid minutes duration")?;
        anyhow::ensure!(val > 0.0, "duration must be > 0");
        return Ok(Duration::from_secs_f64(val * 60.0));
    }
    let secs_str = s.strip_suffix('s').unwrap_or(&s).trim();
    let val: f64 = secs_str
        .parse()
        .with_context(|| format!("invalid duration `{raw}` (examples: `5s`, `500ms`, `1m`)"))?;
    anyhow::ensure!(val > 0.0, "duration must be > 0");
    Ok(Duration::from_secs_f64(val))
}

/// Four production cloud storage workload archetypes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    #[default]
    ParquetPointLookup,
    CheckpointMultipart,
    MetadataPrefixStorm,
    MixedReadWrite,
}

impl WorkloadKind {
    pub fn title(&self) -> &'static str {
        match self {
            Self::ParquetPointLookup => "Parquet Sparse Column Lookup",
            Self::CheckpointMultipart => "AI Checkpoint Multipart Write",
            Self::MetadataPrefixStorm => "Metadata Prefix Storm (List+Stat)",
            Self::MixedReadWrite => "80/20 Mixed Read/Write",
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            Self::ParquetPointLookup => "parquet",
            Self::CheckpointMultipart => "checkpoint",
            Self::MetadataPrefixStorm => "metadata",
            Self::MixedReadWrite => "mixed",
        }
    }

    pub fn from_slug(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "parquet" | "parquet_point_lookup" => Ok(Self::ParquetPointLookup),
            "2" | "ckpt" | "checkpoint" | "multipart" | "checkpoint_multipart" => {
                Ok(Self::CheckpointMultipart)
            }
            "3" | "meta" | "metadata" | "list" | "metadata_prefix_storm" => {
                Ok(Self::MetadataPrefixStorm)
            }
            "4" | "mixed" | "rw" | "mixed_read_write" => Ok(Self::MixedReadWrite),
            other => anyhow::bail!(
                "unknown workload `{other}` (expected: parquet | checkpoint | metadata | mixed)"
            ),
        }
    }

    pub fn write_ratio(&self) -> f64 {
        match self {
            Self::ParquetPointLookup => 0.02,
            Self::CheckpointMultipart => 0.90,
            Self::MetadataPrefixStorm => 0.25,
            Self::MixedReadWrite => 0.20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    pub scheme: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkloadSpec {
    #[serde(default)]
    pub kind: WorkloadKind,
    #[serde(default = "default_rounds")]
    pub rounds: usize,
    /// Optional continuous streaming measurement window in seconds (overrides `rounds` when `Some(>0)`).
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// Optional steady-state warmup window in seconds prior to resetting measurement counters.
    #[serde(default)]
    pub warmup_secs: f64,
    #[serde(default = "default_object_size_mib")]
    pub object_size_mib: usize,
    #[serde(default = "default_true")]
    pub verify_crc32c: bool,
    #[serde(default = "default_true", alias = "enable_local_cache")]
    pub enable_foyer_cache: bool,
    #[serde(default = "default_cache_mib", alias = "cache_memory_mib")]
    pub foyer_memory_mib: usize,
    #[serde(default = "default_egress_ratio")]
    pub internet_egress_ratio: f64,
    /// Automatically delete all remote test objects under the run's isolated prefix upon completion/abort.
    #[serde(default = "default_true")]
    pub auto_cleanup: bool,
}

fn default_rounds() -> usize {
    24
}
fn default_object_size_mib() -> usize {
    4
}
fn default_true() -> bool {
    true
}
fn default_cache_mib() -> usize {
    64
}
fn default_egress_ratio() -> f64 {
    0.15
}

impl Default for WorkloadSpec {
    fn default() -> Self {
        Self {
            kind: WorkloadKind::ParquetPointLookup,
            rounds: default_rounds(),
            duration_secs: None,
            warmup_secs: 0.0,
            object_size_mib: default_object_size_mib(),
            verify_crc32c: true,
            enable_foyer_cache: true,
            foyer_memory_mib: default_cache_mib(),
            internet_egress_ratio: default_egress_ratio(),
            auto_cleanup: true,
        }
    }
}

/// Complete benchmark & auto-tune profile (round-trip TOML serializable, including `[knobs]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub target: TargetConfig,
    #[serde(default)]
    pub workload: WorkloadSpec,
    #[serde(default, alias = "tuned_knobs")]
    pub knobs: Option<TuneKnob>,
    #[serde(default)]
    pub sla: SlaPolicy,
    #[serde(default)]
    pub pricing: CloudPricingProfile,
}

impl ProfileConfig {
    /// Zero-dependency in-memory sandbox profile.
    pub fn memory_demo(kind: WorkloadKind) -> Self {
        Self {
            target: TargetConfig {
                name: "In-Memory Sandbox".to_owned(),
                scheme: "memory".to_owned(),
                options: BTreeMap::new(),
            },
            workload: WorkloadSpec {
                kind,
                rounds: 24,
                duration_secs: None,
                warmup_secs: 0.0,
                object_size_mib: 4,
                verify_crc32c: true,
                enable_foyer_cache: true,
                foyer_memory_mib: 64,
                internet_egress_ratio: 0.15,
                auto_cleanup: true,
            },
            knobs: None,
            sla: SlaPolicy::default(),
            pricing: CloudPricingProfile::aws_s3_standard(),
        }
    }

    /// Resolve effective starting [`TuneKnob`] from `[knobs]` in the profile or [`TuneKnob::default()`].
    pub fn effective_knob(&self) -> TuneKnob {
        let mut k = self.knobs.unwrap_or_default();
        if !self.workload.enable_foyer_cache && self.knobs.is_none() {
            k.enable_foyer = false;
        }
        k
    }

    /// Construct a ready-to-run [`ProfileConfig`] directly from a storage URI (`--url`),
    /// such as `s3://my-bucket/bench`, `fs:///tmp/oprobe`, `gcs://bucket/prefix`, or `memory://`.
    pub fn from_storage_url(
        storage_url: &str,
        endpoint_override: Option<&str>,
        region_override: Option<&str>,
        insecure_tls: bool,
        ca_cert_path: Option<&Path>,
    ) -> Result<Self> {
        let parsed =
            Url::parse(storage_url).with_context(|| format!("invalid storage URL: {storage_url}"))?;
        let scheme = parsed.scheme().to_ascii_lowercase();
        let mut options = BTreeMap::new();

        match scheme.as_str() {
            "memory" => {}
            "fs" | "file" => {
                let root = if parsed.path().is_empty() {
                    "/tmp/oprobe_fs_bench".to_owned()
                } else {
                    parsed.path().to_owned()
                };
                options.insert("root".to_owned(), root);
            }
            "s3" => {
                let bucket = parsed
                    .host_str()
                    .context("s3:// URL requires bucket host, e.g. s3://my-bucket/prefix")?;
                options.insert("bucket".to_owned(), bucket.to_owned());
                if !parsed.path().is_empty() && parsed.path() != "/" {
                    options.insert("root".to_owned(), parsed.path().to_owned());
                }
                let region = region_override
                    .map(str::to_owned)
                    .or_else(|| std::env::var("AWS_REGION").ok())
                    .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
                    .unwrap_or_else(|| "us-east-1".to_owned());
                options.insert("region".to_owned(), region);

                if let Some(ep) = endpoint_override
                    .map(str::to_owned)
                    .or_else(|| std::env::var("AWS_ENDPOINT_URL").ok())
                {
                    options.insert("endpoint".to_owned(), ep);
                    options.insert("enable_virtual_host_style".to_owned(), "false".to_owned());
                }
                if let Ok(ak) = std::env::var("AWS_ACCESS_KEY_ID") {
                    options.insert("access_key_id".to_owned(), ak);
                }
                if let Ok(sk) = std::env::var("AWS_SECRET_ACCESS_KEY") {
                    options.insert("secret_access_key".to_owned(), sk);
                }
                if let Ok(tok) = std::env::var("AWS_SESSION_TOKEN") {
                    options.insert("session_token".to_owned(), tok);
                }
            }
            "gcs" | "gs" => {
                let bucket = parsed
                    .host_str()
                    .context("gcs:// URL requires bucket name")?;
                options.insert("bucket".to_owned(), bucket.to_owned());
                if !parsed.path().is_empty() && parsed.path() != "/" {
                    options.insert("root".to_owned(), parsed.path().to_owned());
                }
                if let Some(ep) = endpoint_override {
                    options.insert("endpoint".to_owned(), ep.to_owned());
                }
            }
            "oss" | "cos" | "b2" => {
                if let Some(bucket) = parsed.host_str() {
                    options.insert("bucket".to_owned(), bucket.to_owned());
                }
                if !parsed.path().is_empty() && parsed.path() != "/" {
                    options.insert("root".to_owned(), parsed.path().to_owned());
                }
                if let Some(ep) = endpoint_override {
                    options.insert("endpoint".to_owned(), ep.to_owned());
                }
            }
            "azblob" => {
                if let Some(container) = parsed.host_str() {
                    options.insert("container".to_owned(), container.to_owned());
                }
                if !parsed.path().is_empty() && parsed.path() != "/" {
                    options.insert("root".to_owned(), parsed.path().to_owned());
                }
                if let Some(ep) = endpoint_override {
                    options.insert("endpoint".to_owned(), ep.to_owned());
                }
            }
            "http" | "https" | "webdav" => {
                let ep = format!(
                    "{}://{}",
                    if scheme == "webdav" { "http" } else { &scheme },
                    parsed.authority()
                );
                options.insert("endpoint".to_owned(), endpoint_override.unwrap_or(&ep).to_owned());
                if !parsed.path().is_empty() {
                    options.insert("root".to_owned(), parsed.path().to_owned());
                }
            }
            other => anyhow::bail!("unsupported storage URL scheme `{other}`"),
        }

        if insecure_tls {
            options.insert("allow_insecure_tls".to_owned(), "true".to_owned());
        }
        if let Some(ca_p) = ca_cert_path {
            options.insert(
                "ca_cert_path".to_owned(),
                ca_p.to_string_lossy().into_owned(),
            );
        }

        let normalized_scheme = match scheme.as_str() {
            "file" => "fs",
            "gs" => "gcs",
            "https" => "http",
            s => s,
        }
        .to_owned();

        let pricing = match normalized_scheme.as_str() {
            "gcs" => CloudPricingProfile::google_gcs_standard(),
            "oss" => CloudPricingProfile::alibaba_oss_standard(),
            "b2" => CloudPricingProfile::backblaze_b2(),
            _ => CloudPricingProfile::aws_s3_standard(),
        };

        Ok(Self {
            target: TargetConfig {
                name: storage_url.to_owned(),
                scheme: normalized_scheme,
                options,
            },
            workload: WorkloadSpec::default(),
            knobs: None,
            sla: SlaPolicy::default(),
            pricing,
        })
    }
}

/// Generate an incompressible Xoshiro256++ payload with 64-bit head/tail anti-deduplication stamps and CRC32C digest.
pub fn generate_incompressible_payload(seed: u64, size: usize) -> (Bytes, u32) {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut buf = BytesMut::zeroed(size);
    rng.fill_bytes(&mut buf);
    if size >= 16 {
        buf[..8].copy_from_slice(&seed.to_le_bytes());
        let tail_stamp = !(seed.wrapping_mul(0x517C_C1B7_2722_0A95));
        buf[size - 8..].copy_from_slice(&tail_stamp.to_le_bytes());
    }
    let frozen = buf.freeze();
    let digest = crc32c::crc32c(&frozen);
    (frozen, digest)
}

/// Coalesce adjacent/overlapping sparse byte ranges within `max_gap`, returning `(coalesced_ranges, wasted_gap_bytes)`.
pub fn coalesce_ranges_with_gap(ranges: &[Range<u64>], max_gap: usize) -> (Vec<Range<u64>>, u64) {
    if ranges.is_empty() {
        return (Vec::new(), 0);
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| r.start);

    let mut merged: Vec<Range<u64>> = Vec::with_capacity(sorted.len());
    let mut wasted_gap_bytes = 0_u64;

    for r in sorted {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
            } else {
                let gap = r.start - last.end;
                if gap <= max_gap as u64 {
                    wasted_gap_bytes += gap;
                    last.end = r.end;
                } else {
                    merged.push(r);
                }
            }
        } else {
            merged.push(r);
        }
    }
    (merged, wasted_gap_bytes)
}

/// Sample host logical CPU count and current `oprobe` process RSS memory (MiB).
pub fn sample_host_resources() -> (usize, u64) {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let rss_mib = sys
        .process(pid)
        .map(|p| p.memory() / (1024 * 1024))
        .unwrap_or(32);
    (cores, rss_mib)
}

/// Extract custom TLS options (`allow_insecure_tls`, `ca_cert_pem`, `ca_cert_path`, `http_protocol`)
/// from `[target.options]` while returning the cleaned map passed to `Operator::via_iter`.
pub fn extract_tls_and_clean_options(
    raw_opts: &BTreeMap<String, String>,
) -> Result<(TlsClientOptions, BTreeMap<String, String>, Option<String>)> {
    let mut cleaned = raw_opts.clone();
    let insecure = cleaned
        .remove("allow_insecure_tls")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    let mut extra_ca_pem = cleaned.remove("ca_cert_pem");
    if let Some(ca_path) = cleaned.remove("ca_cert_path") {
        let pem = std::fs::read_to_string(&ca_path)
            .with_context(|| format!("failed to read ca_cert_path `{ca_path}`"))?;
        extra_ca_pem = Some(pem);
    }

    let alpn_protocols = match cleaned
        .remove("http_version")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("http1" | "http/1.1" | "1.1") => vec![b"http/1.1".to_vec()],
        Some("h2" | "http2" | "2") => vec![b"h2".to_vec()],
        _ => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    };

    let endpoint = cleaned.get("endpoint").cloned().or_else(|| {
        if let Some(region) = cleaned.get("region") {
            if !region.is_empty() && region != "auto" {
                return Some(format!("https://s3.{region}.amazonaws.com"));
            }
        }
        None
    });

    Ok((
        TlsClientOptions {
            alpn_protocols,
            extra_ca_pem,
            insecure_skip_verify: insecure,
        },
        cleaned,
        endpoint,
    ))
}

/// Build an unthrottled administrative OpenDAL `Operator` for standalone `oprobe cleanup`.
pub fn build_admin_operator(profile: &ProfileConfig) -> Result<Operator> {
    ensure_opendal_registry();
    let (tls_opts, clean_opts, _) = extract_tls_and_clean_options(&profile.target.options)?;
    let transport = HyperWhiteboxTransport::try_new(
        HyperTransportOptions {
            pool_max_idle_per_host: 16,
            keep_alive: true,
            connect_timeout: Duration::from_secs(10),
            tls: tls_opts,
        },
        None,
    )?;
    let ctx = OperationContext::new().with_http_transport(HttpTransporter::new(transport));
    let op = Operator::via_iter(&profile.target.scheme, clean_opts)?
        .with_context(ctx)
        .layer(TimeoutLayer::new().with_io_timeout(Duration::from_secs(15)))
        .layer(RetryLayer::default().with_jitter().with_max_times(3));
    Ok(op)
}

/// Build the full 8-layer white-box instrumented OpenDAL `Operator` + companion cleanup `Operator`.
pub async fn build_probed_operator(
    profile: &ProfileConfig,
    knob: &TuneKnob,
) -> Result<(
    Operator,
    Operator,
    Arc<TelemetryRegistry>,
    await_tree::Registry,
)> {
    ensure_opendal_registry();

    let await_reg = await_tree::Registry::new(await_tree::Config::default());
    let telemetry = TelemetryRegistry::new(await_reg.clone(), &profile.target.scheme);

    let (tls_opts, clean_opts, endpoint_url) =
        extract_tls_and_clean_options(&profile.target.options)?;

    // Perform 2-sample cold/warm handshake probe when targeting a real HTTP/HTTPS endpoint
    if !matches!(profile.target.scheme.as_str(), "memory" | "fs") {
        if let Some(ref ep) = endpoint_url {
            if ep.starts_with("http://") || ep.starts_with("https://") {
                if let Ok(hs) =
                    probe_endpoint_handshakes_with_tls(ep, 2, Duration::from_secs(3), &tls_opts)
                        .await
                {
                    telemetry.set_handshake_summary(hs);
                }
            }
        }
    }

    let transport = HyperWhiteboxTransport::try_new(
        HyperTransportOptions {
            pool_max_idle_per_host: knob.concurrent.saturating_mul(2).max(8),
            keep_alive: knob.keep_alive,
            connect_timeout: Duration::from_secs(10),
            tls: tls_opts.clone(),
        },
        Some(telemetry.clone()),
    )?;
    let ctx = OperationContext::new().with_http_transport(HttpTransporter::new(transport));

    let aux_transport = HyperWhiteboxTransport::try_new(
        HyperTransportOptions {
            pool_max_idle_per_host: 16,
            keep_alive: true,
            connect_timeout: Duration::from_secs(10),
            tls: tls_opts.clone(),
        },
        None,
    )?;
    let aux_ctx = OperationContext::new().with_http_transport(HttpTransporter::new(aux_transport));

    let base_op = Operator::via_iter(&profile.target.scheme, clean_opts.clone())?;
    let cleanup_op = base_op
        .clone()
        .with_context(aux_ctx)
        .layer(
            TimeoutLayer::new()
                .with_timeout(Duration::from_secs(60))
                .with_io_timeout(Duration::from_secs(30)),
        )
        .layer(RetryLayer::default().with_jitter().with_max_times(3));

    let retry_notify = TelemetryRegistry::make_retry_notify(telemetry.clone());

    // Innermost -> Outermost OpenDAL layer composition:
    // Raw Service -> WhiteboxObserveLayer -> TimeoutLayer -> TailCutLayer -> RetryLayer
    // -> AwaitTreeLayer -> WhiteboxCacheLayer -> ConcurrentLimitLayer(ProbedSemaphore)
    let mut op = base_op
        .with_context(ctx)
        .layer(WhiteboxObserveLayer::new(
            telemetry.clone(),
            &profile.target.scheme,
        ))
        .layer(
            TimeoutLayer::new()
                .with_timeout(Duration::from_secs(60))
                .with_io_timeout(Duration::from_secs(30)),
        );

    if knob.enable_tail_cut {
        op = op.layer(
            TailCutLayer::builder()
                .percentile(95)
                .safety_factor(1.35)
                .min_samples(8)
                .min_deadline(Duration::from_millis(40))
                .max_deadline(Duration::from_secs(15))
                .window(Duration::from_secs(60))
                .build(),
        );
    }

    op = op
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_min_delay(Duration::from_millis(35))
                .with_max_delay(Duration::from_secs(2))
                .with_max_times(4)
                .with_notify(retry_notify),
        )
        .layer(AwaitTreeLayer::new());

    if knob.enable_foyer && profile.workload.enable_foyer_cache {
        let cap_bytes = (profile.workload.foyer_memory_mib.max(8) as u64) * 1024 * 1024;
        op = op.layer(WhiteboxCacheLayer::new(cap_bytes, telemetry.clone()));
    }

    let probed_sem = ProbedSemaphore::new(knob.concurrent.max(1), telemetry.clone());
    op = op.layer(ConcurrentLimitLayer::with_semaphore(probed_sem));

    Ok((op, cleanup_op, telemetry, await_reg))
}

/// Execution options for [`execute_workload_with_options`], supporting streaming duration windows,
/// warmup phases, live 100ms telemetry frame broadcasts, and cooperative cancellation.
#[derive(Default)]
pub struct WorkloadRunOptions {
    pub rounds_override: Option<usize>,
    pub duration_override: Option<Duration>,
    pub warmup_override: Option<Duration>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    pub live_frame_tx: Option<mpsc::UnboundedSender<TelemetryFrame>>,
}

#[derive(Clone)]
enum PreparedFixture {
    Parquet {
        files: Vec<(String, u64, u32)>,
        chunk_sz: usize,
        range_gap: usize,
        reader_conc: usize,
    },
    Checkpoint {
        shard_size: usize,
        chunk_sz: usize,
        writer_conc: usize,
        verify: bool,
    },
    Metadata {
        partition_prefixes: Vec<String>,
    },
    Mixed {
        base_files: Vec<(String, u32)>,
        chunk_sz: usize,
        io_conc: usize,
        verify: bool,
    },
}

async fn seed_workload_fixtures(
    profile: &ProfileConfig,
    knob: &TuneKnob,
    op: &Operator,
    tracker: &RemoteObjectTracker,
) -> Result<PreparedFixture> {
    let chunk_sz = knob.chunk_bytes.max(64 * 1024);
    let io_conc = (knob.concurrent / 2).max(1);

    match profile.workload.kind {
        WorkloadKind::ParquetPointLookup => {
            let file_bytes = profile.workload.object_size_mib.clamp(1, 64) * 1024 * 1024;
            let num_parts = 2_usize;
            let mut files = Vec::with_capacity(num_parts);
            for p_idx in 0..num_parts {
                let path = tracker.scoped_path(&format!("parquet/part_{p_idx:04}.parquet"));
                let (blob, crc) = generate_incompressible_payload(42 + p_idx as u64, file_bytes);
                op.write(&path, blob).await?;
                tracker.record_written(&path, file_bytes as u64);
                files.push((path, file_bytes as u64, crc));
            }
            Ok(PreparedFixture::Parquet {
                files,
                chunk_sz,
                range_gap: knob.range_gap_bytes,
                reader_conc: io_conc,
            })
        }
        WorkloadKind::CheckpointMultipart => {
            let min_part_sz = op
                .info()
                .capability()
                .write_multi_min_size
                .unwrap_or(256 * 1024);
            let effective_chunk = chunk_sz.max(min_part_sz);
            let obj_mib_bytes = profile.workload.object_size_mib.clamp(1, 64) * 1024 * 1024;
            let shard_size = obj_mib_bytes
                .max(effective_chunk.saturating_mul(2))
                .clamp(512 * 1024, 64 * 1024 * 1024);
            Ok(PreparedFixture::Checkpoint {
                shard_size,
                chunk_sz: effective_chunk,
                writer_conc: io_conc,
                verify: profile.workload.verify_crc32c,
            })
        }
        WorkloadKind::MetadataPrefixStorm => {
            let mut prefixes = Vec::with_capacity(4);
            for p in 0..4_u64 {
                prefixes.push(tracker.scoped_path(&format!("meta/part={p:02}/")));
            }
            for i in 0..24_u64 {
                let path =
                    tracker.scoped_path(&format!("meta/part={:02}/manifest_{i:04}.json", i % 4));
                let (b, _) = generate_incompressible_payload(300 + i, 512);
                op.write(&path, b).await?;
                tracker.record_written(&path, 512);
            }
            Ok(PreparedFixture::Metadata {
                partition_prefixes: prefixes,
            })
        }
        WorkloadKind::MixedReadWrite => {
            let base_bytes =
                (profile.workload.object_size_mib.clamp(1, 32) * 1024 * 1024).min(4 * 1024 * 1024);
            let mut base_files = Vec::with_capacity(2);
            for b_idx in 0..2_u64 {
                let path = tracker.scoped_path(&format!("mixed/hot_segment_{b_idx:02}.bin"));
                let (blob, crc) = generate_incompressible_payload(99 + b_idx, base_bytes);
                op.write(&path, blob).await?;
                tracker.record_written(&path, base_bytes as u64);
                base_files.push((path, crc));
            }
            Ok(PreparedFixture::Mixed {
                base_files,
                chunk_sz,
                io_conc,
                verify: profile.workload.verify_crc32c,
            })
        }
    }
}

async fn execute_single_op(
    seq: u64,
    fixture: &PreparedFixture,
    op: &Operator,
    aux_op: &Operator,
    telemetry: &Arc<TelemetryRegistry>,
    tracker: &RemoteObjectTracker,
) -> Result<()> {
    match fixture {
        PreparedFixture::Parquet {
            files,
            chunk_sz,
            range_gap,
            reader_conc,
        } => {
            // 75% hot partition 0, 25% warm partition 1 to exercise realistic cache locality
            let file_idx = if seq % 4 == 3 && files.len() > 1 {
                1
            } else {
                0
            };
            let (obj_path, _, _) = &files[file_idx];

            // 1. Stat Parquet Footer Metadata
            let stat_scope = OpTraceScope::new();
            let t_stat = Instant::now();
            let meta_res = stat_scope.run(op.stat(obj_path)).await;
            let stat_elapsed = t_stat.elapsed();
            let meta = match meta_res {
                Ok(m) => {
                    telemetry.record_verb_completion(
                        "stat (FooterMeta)",
                        stat_elapsed,
                        0,
                        0,
                        &stat_scope,
                        true,
                    );
                    m
                }
                Err(e) => {
                    telemetry.record_verb_completion(
                        "stat (FooterMeta)",
                        stat_elapsed,
                        0,
                        0,
                        &stat_scope,
                        false,
                    );
                    return Err(e.into());
                }
            };

            // 2. Sparse Column-Chunk Range Fetch
            let len = meta.content_length().max(262_144);
            let jitter_shift = ((seq % 2) * 65_536).min(len / 8);
            let raw_ranges: Vec<Range<u64>> = vec![
                0..16_384,
                (196_608 + jitter_shift)..(262_144 + jitter_shift).min(len),
                (524_288 + jitter_shift)..(589_824 + jitter_shift).min(len),
                (1_310_720.min(len.saturating_sub(98_304)))
                    ..(1_376_256.min(len.saturating_sub(32_768))),
                len.saturating_sub(32_768)..len,
            ];
            let logical_bytes: u64 = raw_ranges
                .iter()
                .map(|r| r.end.saturating_sub(r.start))
                .sum();

            let (coalesced, wasted_gap) = coalesce_ranges_with_gap(&raw_ranges, *range_gap);
            if telemetry.is_measuring() {
                telemetry
                    .coalesced_gap_waste_bytes
                    .fetch_add(wasted_gap, Ordering::Relaxed);
            }

            let read_scope = OpTraceScope::new();
            let t_read = Instant::now();
            let fetch_res = read_scope
                .run(async {
                    let reader = op
                        .reader_with(obj_path)
                        .chunk(*chunk_sz)
                        .gap(*range_gap)
                        .concurrent(*reader_conc)
                        .await?;
                    reader.fetch(coalesced).await
                })
                .await;
            let read_elapsed = t_read.elapsed();

            match fetch_res {
                Ok(_chunks) => {
                    telemetry.record_verb_completion(
                        "reader::fetch (Columns)",
                        read_elapsed,
                        logical_bytes,
                        0,
                        &read_scope,
                        true,
                    );
                }
                Err(e) => {
                    telemetry.record_verb_completion(
                        "reader::fetch (Columns)",
                        read_elapsed,
                        0,
                        0,
                        &read_scope,
                        false,
                    );
                    return Err(e.into());
                }
            }
        }
        PreparedFixture::Checkpoint {
            shard_size,
            chunk_sz,
            writer_conc,
            verify,
        } => {
            let path = tracker.scoped_path(&format!(
                "ckpt/rank_{:02}/shard_{seq:05}.safetensors",
                seq % 4
            ));
            let (payload, digest) = generate_incompressible_payload(2000 + seq, *shard_size);
            tracker.record_written(&path, *shard_size as u64);

            let write_scope = OpTraceScope::new();
            let t_w = Instant::now();
            let write_res = write_scope
                .run(async {
                    let mut writer = op
                        .writer_with(&path)
                        .chunk(*chunk_sz)
                        .concurrent(*writer_conc)
                        .await?;
                    // Feed payload in `chunk_sz` slices so OpenDAL exercises multi-part streaming
                    let mut offset = 0;
                    while offset < payload.len() {
                        let end = (offset + *chunk_sz).min(payload.len());
                        writer.write(payload.slice(offset..end)).await?;
                        offset = end;
                    }
                    writer.close().await
                })
                .await;
            let w_elapsed = t_w.elapsed();

            match write_res {
                Ok(_) => {
                    telemetry.record_verb_completion(
                        "writer::multipart",
                        w_elapsed,
                        0,
                        *shard_size as u64,
                        &write_scope,
                        true,
                    );
                }
                Err(e) => {
                    telemetry.record_verb_completion(
                        "writer::multipart",
                        w_elapsed,
                        0,
                        0,
                        &write_scope,
                        false,
                    );
                    return Err(e.into());
                }
            }

            // Sampled end-to-end readback CRC32C check via unprobed `aux_op` so concurrent
            // measurement workers never race on `telemetry.phase` or pollute measured counters.
            if *verify && (seq < 2 || seq.is_multiple_of(8)) {
                let t_crc = Instant::now();
                if let Ok(rb) = aux_op.read(&path).await {
                    if telemetry.is_measuring() {
                        if crc32c::crc32c(&rb.to_bytes()) == digest {
                            telemetry.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            telemetry.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                        }
                        telemetry
                            .sum_verify_crc_us
                            .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
            }

            // Eagerly delete completed transient shard via unprobed `aux_op` to bound peak storage footprint
            if aux_op.delete(&path).await.is_ok() {
                tracker.record_deleted(&path);
            }
        }
        PreparedFixture::Metadata { partition_prefixes } => {
            let prefix = &partition_prefixes[(seq as usize) % partition_prefixes.len()];
            let list_scope = OpTraceScope::new();
            let t_list = Instant::now();
            let entries_res = list_scope
                .run(async {
                    let lister = op.lister_with(prefix).recursive(true).await?;
                    lister.try_collect::<Vec<_>>().await
                })
                .await;
            let list_elapsed = t_list.elapsed();

            let entries = match entries_res {
                Ok(ent) => {
                    telemetry.record_verb_completion(
                        "list (PrefixScan)",
                        list_elapsed,
                        (ent.len() * 128) as u64,
                        0,
                        &list_scope,
                        true,
                    );
                    ent
                }
                Err(e) => {
                    telemetry.record_verb_completion(
                        "list (PrefixScan)",
                        list_elapsed,
                        0,
                        0,
                        &list_scope,
                        false,
                    );
                    return Err(e.into());
                }
            };

            if let Some(target_entry) = entries.get((seq as usize) % entries.len().max(1)) {
                let stat_scope = OpTraceScope::new();
                let t_st = Instant::now();
                let st_res = stat_scope.run(op.stat(target_entry.path())).await;
                let st_elapsed = t_st.elapsed();
                match st_res {
                    Ok(_) => {
                        telemetry.record_verb_completion(
                            "stat (HeadObject)",
                            st_elapsed,
                            0,
                            0,
                            &stat_scope,
                            true,
                        );
                    }
                    Err(e) => {
                        telemetry.record_verb_completion(
                            "stat (HeadObject)",
                            st_elapsed,
                            0,
                            0,
                            &stat_scope,
                            false,
                        );
                        return Err(e.into());
                    }
                }
            }
        }
        PreparedFixture::Mixed {
            base_files,
            chunk_sz,
            io_conc,
            verify,
        } => {
            if seq.is_multiple_of(5) {
                // 20% Write immutable delta segment
                let w_path = tracker.scoped_path(&format!("mixed/delta_{seq:05}.bin"));
                let w_len = (*chunk_sz).clamp(128 * 1024, 1024 * 1024);
                let (payload, crc) = generate_incompressible_payload(5000 + seq, w_len);
                tracker.record_written(&w_path, w_len as u64);

                let w_scope = OpTraceScope::new();
                let t_w = Instant::now();
                let res = w_scope.run(op.write(&w_path, payload)).await;
                let w_elapsed = t_w.elapsed();
                match res {
                    Ok(_) => {
                        telemetry.record_verb_completion(
                            "write (DeltaObject)",
                            w_elapsed,
                            0,
                            w_len as u64,
                            &w_scope,
                            true,
                        );
                    }
                    Err(e) => {
                        telemetry.record_verb_completion(
                            "write (DeltaObject)",
                            w_elapsed,
                            0,
                            0,
                            &w_scope,
                            false,
                        );
                        return Err(e.into());
                    }
                }

                if *verify && seq == 0 {
                    let t_crc = Instant::now();
                    if let Ok(rb) = aux_op.read(&w_path).await {
                        if telemetry.is_measuring() {
                            if crc32c::crc32c(&rb.to_bytes()) == crc {
                                telemetry.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                telemetry.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                            }
                            telemetry
                                .sum_verify_crc_us
                                .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                        }
                    }
                }

                if aux_op.delete(&w_path).await.is_ok() {
                    tracker.record_deleted(&w_path);
                }
            } else {
                // 80% Range Read path
                let (base_path, _) = &base_files[(seq as usize) % base_files.len()];
                let offset = (seq * 65_536) % (512 * 1024);
                let span = 131_072_u64;

                let r_scope = OpTraceScope::new();
                let t_r = Instant::now();
                let r_res = r_scope
                    .run(async {
                        let reader = op
                            .reader_with(base_path)
                            .chunk(*chunk_sz)
                            .concurrent(*io_conc)
                            .await?;
                        reader.read(offset..(offset + span)).await
                    })
                    .await;
                let r_elapsed = t_r.elapsed();
                match r_res {
                    Ok(_) => {
                        telemetry.record_verb_completion(
                            "reader::read (HotRange)",
                            r_elapsed,
                            span,
                            0,
                            &r_scope,
                            true,
                        );
                    }
                    Err(e) => {
                        telemetry.record_verb_completion(
                            "reader::read (HotRange)",
                            r_elapsed,
                            0,
                            0,
                            &r_scope,
                            false,
                        );
                        return Err(e.into());
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_pool_window(
    worker_count: usize,
    max_rounds: Option<u64>,
    window_duration: Option<Duration>,
    fixture: &PreparedFixture,
    op: &Operator,
    aux_op: &Operator,
    telemetry: &Arc<TelemetryRegistry>,
    tracker: &RemoteObjectTracker,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<()> {
    let deadline = window_duration.map(|d| Instant::now() + d);
    let seq_counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let seq_c = seq_counter.clone();
        let fix_c = fixture.clone();
        let op_c = op.clone();
        let aux_c = aux_op.clone();
        let tel_c = telemetry.clone();
        let trk_c = tracker.clone();
        let cancel_c = cancel_flag.clone();

        handles.push(tokio::spawn(async move {
            loop {
                if cancel_c.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        break;
                    }
                }
                let seq = seq_c.fetch_add(1, Ordering::Relaxed);
                if let Some(limit) = max_rounds {
                    if seq >= limit {
                        break;
                    }
                }

                let task_label = format!("{}#{seq}", match fix_c {
                    PreparedFixture::Parquet { .. } => "parquet_lookup",
                    PreparedFixture::Checkpoint { .. } => "ckpt_multipart",
                    PreparedFixture::Metadata { .. } => "meta_storm",
                    PreparedFixture::Mixed { .. } => "mixed_rw",
                });
                let (root, _guard) = tel_c.register_active_task(seq + 1, task_label);
                root.instrument(execute_single_op(
                    seq, &fix_c, &op_c, &aux_c, &tel_c, &trk_c,
                ))
                .await?;
            }
            Ok::<(), anyhow::Error>(())
        }));
    }

    let mut first_err: Option<anyhow::Error> = None;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                cancel_flag.store(true, Ordering::Relaxed);
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(join_err) => {
                cancel_flag.store(true, Ordering::Relaxed);
                if first_err.is_none() {
                    first_err = Some(join_err.into());
                }
            }
        }
    }

    if let Some(err) = first_err {
        return Err(err);
    }
    Ok(())
}

/// Execute a full benchmark run (seed -> optional warmup -> measured round/duration window -> CRC verify -> guaranteed auto-cleanup).
pub async fn execute_workload_with_options(
    profile: &ProfileConfig,
    knob: &TuneKnob,
    opts: WorkloadRunOptions,
) -> Result<TelemetryFrame> {
    let (op, cleanup_op, telemetry, _await_reg) = build_probed_operator(profile, knob).await?;
    let tracker = RemoteObjectTracker::new();
    let cancel_flag = opts
        .cancel_flag
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    let duration_window = opts.duration_override.or_else(|| {
        profile
            .workload
            .duration_secs
            .filter(|&s| s > 0.0)
            .map(Duration::from_secs_f64)
    });
    let warmup_window = opts.warmup_override.or_else(|| {
        if profile.workload.warmup_secs > 0.0 {
            Some(Duration::from_secs_f64(profile.workload.warmup_secs))
        } else {
            None
        }
    });
    let target_rounds = if duration_window.is_some() {
        None
    } else {
        let r = opts.rounds_override.unwrap_or(profile.workload.rounds).max(1);
        let r = if profile.workload.kind == WorkloadKind::CheckpointMultipart && opts.rounds_override.is_none() {
            r.min(16)
        } else {
            r
        };
        Some(r as u64)
    };

    let worker_count = if let Some(r) = target_rounds {
        knob.concurrent.clamp(1, 128).min(r as usize)
    } else {
        knob.concurrent.clamp(1, 128)
    };

    // Background 10ms AwaitTree slow-span profiler + 100ms live TelemetryFrame publisher
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler_stop_c = sampler_stop.clone();
    let sampler_tel = telemetry.clone();
    let live_tx = opts.live_frame_tx.clone();
    let (host_cores, _) = sample_host_resources();

    let sampler_handle = tokio::spawn(async move {
        let mut tick_idx = 0_u64;
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            if sampler_stop_c.load(Ordering::Relaxed) {
                break;
            }
            sampler_tel.sample_in_flight_trees();
            tick_idx += 1;
            if tick_idx.is_multiple_of(10) {
                if let Some(ref tx) = live_tx {
                    let (_, rss) = sample_host_resources();
                    let _ = tx.send(sampler_tel.snapshot(host_cores, rss));
                }
            }
        }
    });

    // Execute Seeding -> Warmup -> Measure -> Post-Verification while guaranteeing Cleanup on ANY exit path
    let run_result: Result<()> = async {
        telemetry.set_phase(MeasurementPhase::Seed);
        let fixture = seed_workload_fixtures(profile, knob, &op, &tracker).await?;

        if let Some(warm_dur) = warmup_window {
            if !warm_dur.is_zero() && !cancel_flag.load(Ordering::Relaxed) {
                telemetry.set_phase(MeasurementPhase::Warmup);
                run_worker_pool_window(
                    worker_count,
                    None,
                    Some(warm_dur),
                    &fixture,
                    &op,
                    &cleanup_op,
                    &telemetry,
                    &tracker,
                    &cancel_flag,
                )
                .await?;
            }
        }

        telemetry.reset_measurement_counters();
        telemetry.set_phase(MeasurementPhase::Measure);

        run_worker_pool_window(
            worker_count,
            target_rounds,
            duration_window,
            &fixture,
            &op,
            &cleanup_op,
            &telemetry,
            &tracker,
            &cancel_flag,
        )
        .await?;

        // End-to-end CRC32C verification on seeded fixtures
        if profile.workload.verify_crc32c && !cancel_flag.load(Ordering::Relaxed) {
            telemetry.set_phase(MeasurementPhase::Verify);
            match &fixture {
                PreparedFixture::Parquet { files, .. } => {
                    if let Some((path, _, expected_crc)) = files.first() {
                        let t_crc = Instant::now();
                        let full = cleanup_op.read(path).await?;
                        if crc32c::crc32c(&full.to_bytes()) == *expected_crc {
                            telemetry.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            telemetry.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                        }
                        telemetry
                            .sum_verify_crc_us
                            .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
                PreparedFixture::Mixed { base_files, .. } => {
                    if let Some((path, expected_crc)) = base_files.first() {
                        let t_crc = Instant::now();
                        let full = cleanup_op.read(path).await?;
                        if crc32c::crc32c(&full.to_bytes()) == *expected_crc {
                            telemetry.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            telemetry.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                        }
                        telemetry
                            .sum_verify_crc_us
                            .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
                PreparedFixture::Metadata { partition_prefixes } => {
                    if let Some(pref) = partition_prefixes.first() {
                        let t_crc = Instant::now();
                        let sample_key = format!("{pref}manifest_0000.json");
                        if let Ok(buf) = cleanup_op.read(&sample_key).await {
                            let (_, expected_crc) = generate_incompressible_payload(300, 512);
                            if crc32c::crc32c(&buf.to_bytes()) == expected_crc {
                                telemetry.crc_verified_ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                telemetry.crc_corrupted.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        telemetry
                            .sum_verify_crc_us
                            .fetch_add(t_crc.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
                PreparedFixture::Checkpoint { .. } => {}
            }
        }

        Ok(())
    }
    .await;

    sampler_stop.store(true, Ordering::Relaxed);
    let _ = sampler_handle.await;

    // Guaranteed remote test data cleanup (executes even if `run_result` errored or was cancelled!)
    telemetry.set_phase(MeasurementPhase::Cleanup);
    let cleanup_summary = tracker
        .cleanup(&cleanup_op, profile.workload.auto_cleanup)
        .await;
    telemetry.set_cleanup_summary(cleanup_summary);

    run_result?;

    let (cores, rss_mib) = sample_host_resources();
    Ok(telemetry.snapshot(cores, rss_mib))
}

/// Convenience entry point executing a single workload configuration with an optional round override.
pub async fn execute_workload_round(
    profile: &ProfileConfig,
    knob: &TuneKnob,
    rounds_override: Option<usize>,
) -> Result<TelemetryFrame> {
    execute_workload_with_options(
        profile,
        knob,
        WorkloadRunOptions {
            rounds_override,
            ..Default::default()
        },
    )
    .await
}
