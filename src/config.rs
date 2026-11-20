//! Profile model (`*.toml`): target, workload, knobs, SLA and cost assumptions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::net::HttpVersion;
use crate::util::{ByteSize, HumanDuration};

/// Complete, round-trippable benchmark profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub target: TargetSpec,
    #[serde(default)]
    pub workload: WorkloadSpec,
    #[serde(default)]
    pub knobs: Knobs,
    #[serde(default)]
    pub sla: Sla,
    #[serde(default)]
    pub cost: CostSpec,
}

/// Storage target. Either `url = "s3://bucket/prefix"` or explicit `scheme` + `options`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub scheme: String,
    /// OpenDAL service options. Values may reference environment variables as `${NAME}`.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Skip TLS certificate verification (self-signed dev endpoints only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
    /// Extra PEM CA bundle to trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Egress proxy URL; `"none"` disables proxies; unset = `HTTPS_PROXY`/`HTTP_PROXY` env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Lakehouse scan: stat + footer read + vectored column-chunk range reads.
    #[default]
    Parquet,
    /// AI/ML checkpoint: large multipart writes plus occasional full restores.
    Checkpoint,
    /// Many small objects: whole-object GETs and PUTs.
    SmallFiles,
    /// Metadata-heavy: LIST and HEAD storms over a partitioned prefix.
    Metadata,
    /// General purpose 70/20/10 read/write/stat mix.
    Mixed,
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::Parquet,
        Preset::Checkpoint,
        Preset::SmallFiles,
        Preset::Metadata,
        Preset::Mixed,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            Preset::Parquet => "parquet",
            Preset::Checkpoint => "checkpoint",
            Preset::SmallFiles => "small-files",
            Preset::Metadata => "metadata",
            Preset::Mixed => "mixed",
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Preset::Parquet => "lakehouse scan: footer + vectored column-chunk range reads",
            Preset::Checkpoint => "ML checkpoint: large multipart writes + full restores",
            Preset::SmallFiles => "small objects: whole-object GET/PUT",
            Preset::Metadata => "metadata storm: LIST + HEAD over partitions",
            Preset::Mixed => "general purpose: 70% reads / 20% writes / 10% stat",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "parquet" | "lakehouse" => Ok(Preset::Parquet),
            "checkpoint" | "ckpt" => Ok(Preset::Checkpoint),
            "small-files" | "small" => Ok(Preset::SmallFiles),
            "metadata" | "meta" => Ok(Preset::Metadata),
            "mixed" => Ok(Preset::Mixed),
            other => bail!(
                "unknown preset `{other}` (choose: parquet, checkpoint, small-files, metadata, mixed)"
            ),
        }
    }

    /// Default dataset shape and operation mix for the preset.
    pub fn defaults(&self) -> (DatasetSpec, Vec<OpSpec>) {
        let ds = |objects, size| DatasetSpec {
            objects,
            object_size: size,
            ..DatasetSpec::default()
        };
        match self {
            Preset::Parquet => (
                ds(8, ByteSize::mib(16)),
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::ParquetScan {
                        columns: 4,
                        column_size: ByteSize::kib(512),
                        footer_size: ByteSize::kib(64),
                    },
                }],
            ),
            Preset::Checkpoint => (
                ds(2, ByteSize::mib(32)),
                vec![
                    OpSpec {
                        weight: 80,
                        op: OpKind::MultipartWrite {
                            size: ByteSize::mib(32),
                        },
                    },
                    OpSpec {
                        weight: 20,
                        op: OpKind::Read { size: None },
                    },
                ],
            ),
            Preset::SmallFiles => (
                ds(256, ByteSize::kib(64)),
                vec![
                    OpSpec {
                        weight: 60,
                        op: OpKind::Read { size: None },
                    },
                    OpSpec {
                        weight: 30,
                        op: OpKind::Write {
                            size: ByteSize::kib(64),
                        },
                    },
                    OpSpec {
                        weight: 10,
                        op: OpKind::Stat,
                    },
                ],
            ),
            Preset::Metadata => (
                DatasetSpec {
                    objects: 512,
                    object_size: ByteSize::kib(4),
                    partitions: 16,
                    ..DatasetSpec::default()
                },
                vec![
                    OpSpec {
                        weight: 40,
                        op: OpKind::List { recursive: false },
                    },
                    OpSpec {
                        weight: 60,
                        op: OpKind::Stat,
                    },
                ],
            ),
            Preset::Mixed => (
                ds(32, ByteSize::mib(4)),
                vec![
                    OpSpec {
                        weight: 60,
                        op: OpKind::Read {
                            size: Some(ByteSize::mib(1)),
                        },
                    },
                    OpSpec {
                        weight: 10,
                        op: OpKind::Read { size: None },
                    },
                    OpSpec {
                        weight: 20,
                        op: OpKind::Write {
                            size: ByteSize::mib(1),
                        },
                    },
                    OpSpec {
                        weight: 10,
                        op: OpKind::Stat,
                    },
                ],
            ),
        }
    }
}

/// One weighted operation in the workload mix.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpSpec {
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(flatten)]
    pub op: OpKind,
}

fn default_weight() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpKind {
    /// Read a random range of `size` bytes, or the whole object when `size` is omitted.
    Read {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<ByteSize>,
    },
    /// `stat` + footer read + `columns` random column chunks fetched as one vectored read.
    ParquetScan {
        columns: u32,
        column_size: ByteSize,
        footer_size: ByteSize,
    },
    /// Single PUT of `size` bytes.
    Write { size: ByteSize },
    /// Streaming multipart write of `size` bytes (chunked by `knobs.chunk_size`).
    MultipartWrite { size: ByteSize },
    /// HEAD a dataset object.
    Stat,
    /// LIST one dataset partition.
    List {
        #[serde(default)]
        recursive: bool,
    },
}

impl OpKind {
    pub fn is_write(&self) -> bool {
        matches!(self, OpKind::Write { .. } | OpKind::MultipartWrite { .. })
    }
}

/// Key popularity distribution.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Access {
    #[default]
    Uniform,
    /// Zipfian with exponent `s` (1.0–1.3 is typical for hot partitions).
    Zipf(f64),
    /// `fraction` of keys receive `probability` of accesses (e.g. 0.2/0.8).
    Hotset { fraction: f64, probability: f64 },
}

impl std::str::FromStr for Access {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().to_ascii_lowercase();
        if s == "uniform" {
            return Ok(Access::Uniform);
        }
        if let Some(v) = s.strip_prefix("zipf") {
            let v = v.trim_start_matches([':', '=']);
            let exp = if v.is_empty() { 1.1 } else { v.parse()? };
            anyhow::ensure!(exp > 0.0, "zipf exponent must be > 0");
            return Ok(Access::Zipf(exp));
        }
        if let Some(v) = s.strip_prefix("hotset") {
            let v = v.trim_start_matches([':', '=']);
            let (f, p) = v.split_once('/').unwrap_or(("0.2", "0.8"));
            let (fraction, probability): (f64, f64) = (f.parse()?, p.parse()?);
            anyhow::ensure!(
                (0.0..=1.0).contains(&fraction) && (0.0..=1.0).contains(&probability),
                "hotset fractions must be within 0..1"
            );
            return Ok(Access::Hotset {
                fraction,
                probability,
            });
        }
        bail!("unknown access pattern `{s}` (uniform | zipf:1.1 | hotset:0.2/0.8)")
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Access::Uniform => write!(f, "uniform"),
            Access::Zipf(s) => write!(f, "zipf:{s}"),
            Access::Hotset {
                fraction,
                probability,
            } => write!(f, "hotset:{fraction}/{probability}"),
        }
    }
}

impl Serialize for Access {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Access {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Objects the workload reads from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetSpec {
    #[serde(default = "default_objects")]
    pub objects: usize,
    #[serde(default = "default_object_size")]
    pub object_size: ByteSize,
    /// Spread objects across this many `part=NN/` prefixes.
    #[serde(default = "default_partitions")]
    pub partitions: usize,
    /// Benchmark **existing** objects under this prefix (read-only; nothing is written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_prefix: Option<String>,
    /// Keep the generated dataset between runs (`oprobe-dataset/<fingerprint>/`) to avoid
    /// re-uploading it every time. Remove with `oprobe cleanup --datasets`.
    #[serde(default)]
    pub persist: bool,
}

fn default_objects() -> usize {
    16
}
fn default_object_size() -> ByteSize {
    ByteSize::mib(4)
}
fn default_partitions() -> usize {
    4
}

impl Default for DatasetSpec {
    fn default() -> Self {
        Self {
            objects: default_objects(),
            object_size: default_object_size(),
            partitions: default_partitions(),
            existing_prefix: None,
            persist: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadSpec {
    #[serde(default)]
    pub preset: Preset,
    #[serde(default = "default_duration")]
    pub duration: HumanDuration,
    #[serde(default = "default_warmup")]
    pub warmup: HumanDuration,
    /// Stop after this many measured operations (deterministic CI runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ops: Option<u64>,
    /// Open-loop arrival rate in ops/s. Unset = closed loop (`knobs.concurrency` workers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    #[serde(default)]
    pub access: Access,
    /// Dataset override (defaults come from the preset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<DatasetSpec>,
    /// Operation mix override (defaults come from the preset).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mix: Vec<OpSpec>,
    /// Verify CRC32C of whole-object reads and sampled writes.
    #[serde(default = "default_true")]
    pub verify: bool,
    /// Delete everything written under the run prefix when the run ends.
    #[serde(default = "default_true")]
    pub cleanup: bool,
    #[serde(default = "default_seed")]
    pub seed: u64,
}

fn default_duration() -> HumanDuration {
    HumanDuration::secs(10)
}
fn default_warmup() -> HumanDuration {
    HumanDuration::secs(2)
}
fn default_true() -> bool {
    true
}
fn default_seed() -> u64 {
    42
}

impl Default for WorkloadSpec {
    fn default() -> Self {
        Self {
            preset: Preset::default(),
            duration: default_duration(),
            warmup: default_warmup(),
            max_ops: None,
            rate: None,
            access: Access::Uniform,
            dataset: None,
            mix: Vec::new(),
            verify: true,
            cleanup: true,
            seed: default_seed(),
        }
    }
}

impl WorkloadSpec {
    pub fn effective_dataset(&self) -> DatasetSpec {
        self.dataset
            .clone()
            .unwrap_or_else(|| self.preset.defaults().0)
    }

    pub fn effective_mix(&self) -> Vec<OpSpec> {
        if self.mix.is_empty() {
            self.preset.defaults().1
        } else {
            self.mix.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    /// OpenDAL's default HTTP client (reqwest), observed through its extension points.
    #[default]
    Opendal,
    /// oprobe's own instrumented connection pool.
    Microscope,
}

impl TransportMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opendal => "opendal",
            Self::Microscope => "microscope",
        }
    }
}

/// Client-side configuration under test / being tuned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Knobs {
    /// Concurrent logical operations (closed-loop workers, or open-loop in-flight cap).
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Per-operation reader/writer concurrency (`.concurrent(n)`).
    #[serde(default = "default_io_concurrency")]
    pub io_concurrency: usize,
    /// Reader/writer chunk size (`.chunk(n)`), also the multipart part size.
    #[serde(default = "default_chunk")]
    pub chunk_size: ByteSize,
    /// Merge vectored ranges closer than this (`.gap(n)`).
    #[serde(default = "default_gap")]
    pub range_gap: ByteSize,
    /// Cap on in-flight HTTP requests across the client (0 = unlimited).
    #[serde(default)]
    pub max_inflight_requests: usize,
    #[serde(default)]
    pub http_version: HttpVersion,
    /// How requests are made and observed: `opendal` = OpenDAL's own reqwest client (what
    /// production runs; the default), `microscope` = oprobe's instrumented connection pool
    /// (exact queue / per-dial phases / HTTP/2 streams, but a different client).
    #[serde(default)]
    pub transport: TransportMode,
    #[serde(default = "default_true")]
    pub keep_alive: bool,
    /// In-memory read cache.
    #[serde(default)]
    pub cache: bool,
    #[serde(default = "default_cache_size")]
    pub cache_size: ByteSize,
    /// Retry attempts on temporary errors (OpenDAL `RetryLayer`).
    #[serde(default = "default_retries")]
    pub retries: usize,
    /// Cancel & retry straggler requests above the adaptive p95 deadline (`TailCutLayer`).
    #[serde(default)]
    pub tail_cut: bool,
    /// Per-operation timeout.
    #[serde(default = "default_timeout")]
    pub timeout: HumanDuration,
}

fn default_concurrency() -> usize {
    16
}
fn default_io_concurrency() -> usize {
    4
}
fn default_chunk() -> ByteSize {
    ByteSize::mib(8)
}
fn default_gap() -> ByteSize {
    ByteSize::kib(64)
}
fn default_cache_size() -> ByteSize {
    ByteSize::mib(256)
}
fn default_retries() -> usize {
    3
}
fn default_timeout() -> HumanDuration {
    HumanDuration::secs(60)
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            io_concurrency: default_io_concurrency(),
            chunk_size: default_chunk(),
            range_gap: default_gap(),
            max_inflight_requests: 0,
            http_version: HttpVersion::Auto,
            transport: TransportMode::Opendal,
            keep_alive: true,
            cache: false,
            cache_size: default_cache_size(),
            retries: default_retries(),
            tail_cut: false,
            timeout: default_timeout(),
        }
    }
}

impl Knobs {
    pub fn summary(&self) -> String {
        format!(
            "concurrency={} io={} chunk={} gap={} inflight={} http={:?} keep-alive={} cache={} retries={}{}{}",
            self.concurrency,
            self.io_concurrency,
            self.chunk_size,
            self.range_gap,
            if self.max_inflight_requests == 0 { "∞".to_owned() } else { self.max_inflight_requests.to_string() },
            self.http_version,
            self.keep_alive,
            if self.cache { self.cache_size.to_string() } else { "off".into() },
            self.retries,
            if self.tail_cut { " tail-cut" } else { "" },
            if self.transport == TransportMode::Microscope { " (microscope)" } else { "" },
        )
        .to_ascii_lowercase()
    }
}

/// Service level objectives used by `tune` and the diagnosis engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sla {
    /// Max acceptable p99 per operation verb (ms); unset = no latency constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    /// Max fraction of failed operations.
    #[serde(default = "default_error_rate")]
    pub max_error_rate: f64,
    /// Max fraction of operations that hit a 429/503 throttle.
    #[serde(default = "default_throttle_rate")]
    pub max_throttle_rate: f64,
}

fn default_error_rate() -> f64 {
    0.001
}
fn default_throttle_rate() -> f64 {
    0.0
}

impl Default for Sla {
    fn default() -> Self {
        Self {
            p99_ms: None,
            max_error_rate: default_error_rate(),
            max_throttle_rate: default_throttle_rate(),
        }
    }
}

/// Business volume used to project measured per-operation costs to a monthly bill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSpec {
    /// Pricing preset: aws-s3, gcs, azure-blob, cloudflare-r2, backblaze-b2, aliyun-oss.
    /// Defaults to the target's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Logical operations per month at this workload mix.
    #[serde(default = "default_monthly_ops")]
    pub monthly_ops: f64,
    /// Data kept at rest (GiB).
    #[serde(default = "default_stored_gib")]
    pub stored_gib: f64,
    /// Fraction of downloaded bytes that leave the cloud (internet egress).
    #[serde(default)]
    pub egress_fraction: f64,
    /// Custom price overrides (USD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<crate::analysis::cost::Pricing>,
}

fn default_monthly_ops() -> f64 {
    100_000_000.0
}
fn default_stored_gib() -> f64 {
    1024.0
}

impl Default for CostSpec {
    fn default() -> Self {
        Self {
            provider: None,
            monthly_ops: default_monthly_ops(),
            stored_gib: default_stored_gib(),
            egress_fraction: 0.0,
            pricing: None,
        }
    }
}

impl TargetSpec {
    /// Build a target from a storage URL such as `s3://bucket/prefix`, `gcs://bucket`,
    /// `azblob://container/prefix`, `fs:///tmp/bench`, `memory://`.
    pub fn from_url(raw: &str) -> Result<Self> {
        let u = url::Url::parse(raw).with_context(|| {
            format!("invalid storage URL `{raw}` (examples: s3://bucket/prefix, fs:///tmp/bench, memory://)")
        })?;
        let scheme = u.scheme().to_ascii_lowercase();
        let mut options = BTreeMap::new();
        let path = u.path().trim_end_matches('/');
        let root = if path.is_empty() {
            None
        } else {
            Some(path.to_owned())
        };
        let host = u.host_str().map(str::to_owned);
        let scheme = match scheme.as_str() {
            "memory" | "mem" => "memory",
            "fs" | "file" => {
                let p = match (&host, path) {
                    (Some(h), p) if !h.is_empty() => format!("{h}{p}"),
                    (_, "") => std::env::temp_dir()
                        .join("oprobe")
                        .to_string_lossy()
                        .into_owned(),
                    (_, p) => p.to_owned(),
                };
                options.insert("root".into(), p);
                "fs"
            }
            "s3" | "s3a" | "r2" | "minio" => {
                let bucket = host.context("s3:// URL needs a bucket: s3://my-bucket/prefix")?;
                options.insert("bucket".into(), bucket);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "s3"
            }
            "gs" | "gcs" => {
                options.insert("bucket".into(), host.context("gcs:// URL needs a bucket")?);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "gcs"
            }
            "oss" | "cos" | "b2" => {
                options.insert("bucket".into(), host.context("URL needs a bucket")?);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                match scheme.as_str() {
                    "oss" => "oss",
                    "cos" => "cos",
                    _ => "b2",
                }
            }
            "azblob" | "az" | "azure" => {
                options.insert(
                    "container".into(),
                    host.context("azblob:// URL needs a container")?,
                );
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "azblob"
            }
            "webdav" | "webdavs" => {
                let s = if scheme == "webdavs" { "https" } else { "http" };
                let h = host.context("webdav:// URL needs a host")?;
                let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                options.insert("endpoint".into(), format!("{s}://{h}{port}"));
                // Credentials in the URL become backend options (redacted in every output)
                // instead of leaking through `endpoint`.
                if !u.username().is_empty() {
                    options.insert("username".into(), u.username().to_owned());
                }
                if let Some(pw) = u.password() {
                    options.insert("password".into(), pw.to_owned());
                }
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "webdav"
            }
            other => bail!(
                "unsupported storage scheme `{other}` (supported: s3, gcs, azblob, oss, cos, b2, webdav, fs, memory)"
            ),
        };
        Ok(Self {
            url: Some(raw.to_owned()),
            scheme: scheme.to_owned(),
            options,
            ..Default::default()
        })
    }

    /// Normalize: expand `url` into scheme/options if needed.
    pub fn normalized(&self) -> Result<Self> {
        let mut t = if self.scheme.is_empty() {
            let url = self
                .url
                .as_deref()
                .context("target needs either `url` or `scheme`")?;
            let mut base = Self::from_url(url)?;
            for (k, v) in &self.options {
                base.options.insert(k.clone(), v.clone());
            }
            base.insecure = self.insecure;
            base.ca_cert = self.ca_cert.clone();
            base.proxy = self.proxy.clone();
            base
        } else {
            self.clone()
        };
        // Keep the user's URL but never its credentials (they moved into options above).
        if let Some(u) = &t.url {
            t.url = Some(crate::util::strip_url_credentials(u));
        }
        Ok(t)
    }

    /// Options with `${ENV}` references expanded, ready for `Operator::via_iter`.
    /// S3 gets a default region (`AWS_DEFAULT_REGION`, else `us-east-1`) unless one is
    /// configured; the default is applied here so it is never persisted as a user setting.
    pub fn resolved_options(&self) -> Result<BTreeMap<String, String>> {
        let mut opts: BTreeMap<String, String> = self
            .options
            .iter()
            .map(|(k, v)| Ok((k.clone(), expand_env(v)?)))
            .collect::<Result<_>>()?;
        if self.scheme == "s3"
            && !opts.contains_key("region")
            && std::env::var("AWS_REGION").is_err()
        {
            let region = std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into());
            opts.insert("region".into(), region);
        }
        Ok(opts)
    }

    /// Human-readable identity of the target (no secrets).
    pub fn display(&self) -> String {
        if let Some(u) = &self.url {
            return crate::util::display_url(u);
        }
        let o = &self.options;
        let base = o
            .get("bucket")
            .or_else(|| o.get("container"))
            .or_else(|| o.get("root"))
            .cloned()
            .unwrap_or_default();
        format!("{}://{}", self.scheme, base)
    }

    /// Endpoint URL the backend will talk to, when it can be determined.
    pub fn endpoint(&self) -> Option<String> {
        if let Some(e) = self.options.get("endpoint") {
            return expand_env(e)
                .ok()
                .map(|e| crate::util::strip_url_credentials(&e));
        }
        match self.scheme.as_str() {
            "s3" => {
                if let Ok(e) = std::env::var("AWS_ENDPOINT_URL") {
                    return Some(crate::util::strip_url_credentials(&e));
                }
                let region = self
                    .options
                    .get("region")
                    .cloned()
                    .or_else(|| std::env::var("AWS_REGION").ok())
                    .unwrap_or_else(|| "us-east-1".into());
                Some(format!("https://s3.{region}.amazonaws.com"))
            }
            "gcs" => Some("https://storage.googleapis.com".into()),
            "azblob" => self
                .options
                .get("account_name")
                .map(|a| format!("https://{a}.blob.core.windows.net")),
            "b2" => Some("https://api.backblazeb2.com".into()),
            _ => None,
        }
    }

    pub fn is_network(&self) -> bool {
        !matches!(self.scheme.as_str(), "memory" | "fs")
    }
}

/// Options `from_url` derives from URL credentials: always kept (as `${ENV}` references) when
/// writing a profile, since the URL written back no longer carries them.
fn is_url_secret(k: &str) -> bool {
    matches!(k, "username" | "password")
}

/// A value made only of `${NAME}` references (e.g. `${AWS_SECRET_ACCESS_KEY}`).
fn is_env_reference(v: &str) -> bool {
    let mut rest = v.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let Some(after) = rest.strip_prefix("${") else {
            return false;
        };
        let Some(end) = after.find('}') else {
            return false;
        };
        let name = &after[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
        rest = &after[end + 1..];
    }
    true
}

/// Expand `${NAME}` references from the environment.
pub fn expand_env(v: &str) -> Result<String> {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("unterminated `${{` in `{v}`"))?;
        let name = &after[..end];
        let val = std::env::var(name).with_context(|| {
            format!("environment variable `{name}` referenced by the profile is not set")
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Profile {
    pub fn from_target(target: TargetSpec) -> Self {
        Self {
            name: None,
            target,
            workload: WorkloadSpec::default(),
            knobs: Knobs::default(),
            sla: Sla::default(),
            cost: CostSpec::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read profile `{}`", path.display()))?;
        let p: Profile = toml::from_str(&raw)
            .with_context(|| format!("invalid profile `{}`", path.display()))?;
        p.validate()?;
        Ok(p)
    }

    pub fn validate(&self) -> Result<()> {
        let k = &self.knobs;
        anyhow::ensure!(k.concurrency >= 1, "knobs.concurrency must be >= 1");
        anyhow::ensure!(k.io_concurrency >= 1, "knobs.io_concurrency must be >= 1");
        anyhow::ensure!(k.chunk_size.0 >= 4096, "knobs.chunk_size must be >= 4KiB");
        let w = &self.workload;
        if let Some(r) = w.rate {
            anyhow::ensure!(r > 0.0, "workload.rate must be > 0");
        }
        let ds = w.effective_dataset();
        anyhow::ensure!(
            ds.existing_prefix.is_some()
                || ds.objects >= 1
                || w.effective_mix().iter().all(|m| m.op.is_write()),
            "workload.dataset.objects must be >= 1 for read workloads"
        );
        anyhow::ensure!(
            w.effective_mix().iter().any(|m| m.weight > 0),
            "workload.mix needs at least one op with weight > 0"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.cost.egress_fraction),
            "cost.egress_fraction must be within 0..1"
        );
        Ok(())
    }

    /// Serialize to TOML. Secrets are never stored: secret options become `${ENV}` references
    /// (existing references are kept as written), credentials embedded in URLs are dropped, and
    /// options that merely restate the target URL are left out so the file stays as the user
    /// wrote it.
    pub fn to_toml(&self) -> Result<String> {
        let mut p = self.clone();
        let t = &mut p.target;
        if let Some(url) = t.url.clone() {
            // Options derived from the URL (bucket, root, scheme...) are re-derived on load.
            if let Ok(base) = TargetSpec::from_url(&url) {
                t.options.retain(|k, v| {
                    base.options
                        .get(k)
                        .is_none_or(|b| b != v || is_url_secret(k))
                });
                if t.scheme == base.scheme {
                    t.scheme.clear();
                }
            }
            t.url = Some(crate::util::strip_url_credentials(&url));
        }
        t.options = std::mem::take(&mut t.options)
            .into_iter()
            .map(|(k, v)| {
                if crate::util::is_secret_key(&k) {
                    if is_env_reference(&v) {
                        (k, v)
                    } else {
                        let env = k.to_ascii_uppercase().replace('-', "_");
                        (k, format!("${{{env}}}"))
                    }
                } else {
                    let v = crate::util::strip_url_credentials(&v);
                    (k, v)
                }
            })
            .collect();
        t.proxy = t.proxy.as_deref().map(crate::util::strip_url_credentials);
        Ok(toml::to_string_pretty(&p)?)
    }

    pub fn duration(&self) -> Duration {
        self.workload.duration.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_targets() {
        let t = TargetSpec::from_url("s3://bkt/some/prefix/").unwrap();
        assert_eq!(t.scheme, "s3");
        assert_eq!(t.options["bucket"], "bkt");
        assert_eq!(t.options["root"], "/some/prefix");
        let t = TargetSpec::from_url("fs:///tmp/x").unwrap();
        assert_eq!(t.options["root"], "/tmp/x");
        assert_eq!(TargetSpec::from_url("memory://").unwrap().scheme, "memory");
        assert!(TargetSpec::from_url("ftp://x").is_err());
    }

    #[test]
    fn profile_roundtrip_and_presets() {
        let src = r#"
            [target]
            url = "s3://bucket/bench"
            options = { endpoint = "http://127.0.0.1:9000" }

            [workload]
            preset = "mixed"
            duration = "5s"
            access = "zipf:1.2"
            [[workload.mix]]
            op = "read"
            size = "256KiB"
            weight = 3
            [[workload.mix]]
            op = "parquet_scan"
            columns = 3
            column_size = "128KiB"
            footer_size = "16KiB"

            [knobs]
            concurrency = 8
            chunk_size = "4MiB"
        "#;
        let p: Profile = toml::from_str(src).unwrap();
        p.validate().unwrap();
        assert_eq!(p.workload.access, Access::Zipf(1.2));
        assert_eq!(p.workload.mix.len(), 2);
        assert_eq!(p.knobs.chunk_size, ByteSize::mib(4));
        let back: Profile = toml::from_str(&p.to_toml().unwrap()).unwrap();
        assert_eq!(back, p);
        for preset in Preset::ALL {
            let (_, mix) = preset.defaults();
            assert!(!mix.is_empty());
        }
    }

    #[test]
    fn env_expansion_and_secret_placeholders() {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("OPROBE_TEST_SECRET", "s3cr3t") };
        assert_eq!(
            expand_env("a-${OPROBE_TEST_SECRET}-b").unwrap(),
            "a-s3cr3t-b"
        );
        let mut t = TargetSpec::from_url("s3://b").unwrap();
        t.options.insert("secret_access_key".into(), "plain".into());
        let toml = Profile::from_target(t).to_toml().unwrap();
        assert!(!toml.contains("plain"));
        assert!(toml.contains("${SECRET_ACCESS_KEY}"));
    }

    #[test]
    fn to_toml_keeps_env_references_and_drops_derived_values() {
        let mut t = TargetSpec::from_url("s3://bkt/pre").unwrap();
        t.options
            .insert("secret_access_key".into(), "${MY_SK}".into());
        t.options.insert(
            "server_side_encryption_customer_key".into(),
            "c3VwZXJzZWNyZXQ=".into(),
        );
        t.options
            .insert("endpoint".into(), "http://ak:sk@minio:9000".into());
        let p = Profile::from_target(t.normalized().unwrap());
        let toml = p.to_toml().unwrap();
        assert!(toml.contains("secret_access_key = \"${MY_SK}\""), "{toml}");
        assert!(
            toml.contains("${SERVER_SIDE_ENCRYPTION_CUSTOMER_KEY}"),
            "{toml}"
        );
        assert!(!toml.contains("c3VwZXJzZWNyZXQ"), "{toml}");
        assert!(!toml.contains("ak:sk"), "{toml}");
        assert!(
            !toml.contains("region"),
            "defaulted region is not persisted: {toml}"
        );
        assert!(
            !toml.contains("bucket"),
            "URL-derived options are not repeated: {toml}"
        );
        let back: Profile = toml::from_str(&toml).unwrap();
        back.validate().unwrap();
        let n = back.target.normalized().unwrap();
        assert_eq!(n.scheme, "s3");
        assert_eq!(n.options["bucket"], "bkt");
        assert_eq!(n.options["endpoint"], "http://minio:9000");
    }

    #[test]
    fn url_credentials_never_reach_endpoint_or_display() {
        let t = TargetSpec::from_url("webdav://alice:hunter2@dav.example:8443/files")
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(t.options["endpoint"], "http://dav.example:8443");
        assert_eq!(t.options["password"], "hunter2");
        assert!(!t.display().contains("hunter2"), "{}", t.display());
        assert!(!t.endpoint().unwrap().contains("alice"));
        let toml = Profile::from_target(t).to_toml().unwrap();
        assert!(!toml.contains("hunter2"), "{toml}");
        assert!(toml.contains("${PASSWORD}"), "{toml}");
        let mut s3 = TargetSpec::from_url("s3://b").unwrap();
        s3.options
            .insert("endpoint".into(), "https://k:s@minio.local".into());
        assert_eq!(s3.endpoint().unwrap(), "https://minio.local");
        // Default region is applied when building the operator, not stored in the profile.
        let n = s3.normalized().unwrap();
        assert!(!n.options.contains_key("region"));
        if std::env::var("AWS_REGION").is_err() {
            assert!(n.resolved_options().unwrap().contains_key("region"));
        }
    }
}

/// Fully commented profile template for `oprobe init`.
pub fn template(preset: Preset, target: &str) -> String {
    let (ds, mix) = preset.defaults();
    let mix_toml: String = mix
        .iter()
        .map(|m| {
            let mut t = toml::to_string(m).unwrap_or_default();
            t = t.lines().map(|l| format!("{l}\n")).collect();
            format!("[[workload.mix]]\n{t}\n")
        })
        .collect();
    format!(
        r#"# oprobe profile — {desc}
# Run:  oprobe run --profile this.toml        Tune: oprobe tune --profile this.toml
# Docs: https://github.com/hxddh/oprobe/blob/main/docs/workloads.md

[target]
url = "{target}"
# Backend options are passed to Apache OpenDAL. Never paste secrets: reference env vars.
[target.options]
# region = "us-east-1"
# endpoint = "https://s3.us-east-1.amazonaws.com"     # MinIO / R2 / Ceph / private clouds
# access_key_id = "${{AWS_ACCESS_KEY_ID}}"
# secret_access_key = "${{AWS_SECRET_ACCESS_KEY}}"

[workload]
preset = "{slug}"
duration = "30s"            # measurement window
warmup = "5s"               # connections & caches warm up before measuring
access = "uniform"          # uniform | zipf:1.1 | hotset:0.2/0.8
# rate = 200.0              # open-loop ops/s (latency measured from intended start)
# max_ops = 10000           # stop after N ops (deterministic CI runs)
verify = true               # CRC32C-verify whole-object reads and sampled writes
cleanup = true              # delete test data under oprobe-run-*/ afterwards

[workload.dataset]
objects = {objects}
object_size = "{size}"
partitions = {parts}
# existing_prefix = "warehouse/events/"   # benchmark your real objects (read-only)
# persist = true                          # keep the generated dataset for later runs

# Operation mix (defaults of the preset; edit or extend):
{mix_toml}[knobs]
concurrency = 16            # concurrent operations
io_concurrency = 4          # per-operation reader/writer parallelism
chunk_size = "8MiB"         # reader/writer chunk = multipart part size
range_gap = "64KiB"         # merge vectored ranges closer than this
max_inflight_requests = 0   # cap on in-flight HTTP requests (0 = unlimited)
http_version = "auto"       # auto | h1 | h2
keep_alive = true
cache = false               # in-memory read cache
cache_size = "256MiB"
retries = 3
tail_cut = false            # cancel & retry stragglers above adaptive p95
timeout = "60s"

[sla]
# p99_ms = 200.0            # tune rejects configurations above this
max_error_rate = 0.001
max_throttle_rate = 0.0

[cost]
# provider = "aws-s3"       # aws-s3 | gcs | azure-blob | cloudflare-r2 | backblaze-b2 | aliyun-oss
monthly_ops = 100000000     # project measured per-op cost to this monthly volume
stored_gib = 1024
egress_fraction = 0.0       # share of downloads leaving the cloud
"#,
        desc = preset.describe(),
        slug = preset.slug(),
        objects = ds.objects,
        size = ds.object_size,
        parts = ds.partitions,
    )
}

#[cfg(test)]
mod template_tests {
    #[test]
    fn templates_parse() {
        for p in super::Preset::ALL {
            let t = super::template(p, "s3://b/x");
            let prof: super::Profile =
                toml::from_str(&t).unwrap_or_else(|e| panic!("{p:?}: {e}\n{t}"));
            prof.validate().unwrap();
        }
    }
}
