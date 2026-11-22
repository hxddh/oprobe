//! Profile model (`*.toml`): target, workload, knobs, SLA and cost assumptions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::net::HttpVersion;
use crate::util::{ByteSize, HumanDuration};

/// A typed `usage.invalid_argument` error (values given as flags, tool arguments or in a
/// profile; [`Profile::load`] re-codes them `config.invalid`).
fn invalid(msg: impl Into<String>) -> anyhow::Error {
    crate::input::invalid(msg).into()
}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    invalid(msg)
}

/// Defaults of `oprobe tune`, shared with the app's Tune page and MCP so every surface runs
/// the same search (clap needs them as `&'static str`; [`defaults`] parses them).
pub mod tune_defaults {
    /// Measurement window per trial.
    pub const TRIAL_DURATION: &str = "5s";
    /// Warmup per trial.
    pub const TRIAL_WARMUP: &str = "1s";
    /// Maximum number of trials.
    pub const MAX_TRIALS: usize = 24;
    /// Fewest trials a search can run (a baseline and one candidate).
    pub const MIN_TRIALS: usize = 2;
    /// Highest concurrency to try.
    pub const MAX_CONCURRENCY: usize = 256;
    /// Pause after a throttled trial.
    pub const COOLDOWN: &str = "5s";
    /// Share of throttled requests a configuration may cause and still be feasible (v4 §6).
    pub const MAX_THROTTLE_RATIO: f64 = 0.001;
    /// Repeats of each trial near the throttle boundary (v4 §6).
    pub const BOUNDARY_REPEATS: usize = 2;
}

/// The tuner's built-in search settings, parsed.
#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
pub struct TuneDefaults {
    pub trial_duration: HumanDuration,
    pub trial_warmup: HumanDuration,
    pub max_trials: usize,
    pub min_trials: usize,
    pub max_concurrency: usize,
    pub cooldown: HumanDuration,
    pub max_throttle_ratio: f64,
    pub boundary_repeats: usize,
}

/// Every built-in default in one place (design v4 §1): the CLI, the App, MCP and tune read
/// these instead of hard-coding their own. A profile's defaults are `Profile::from_target`'s
/// (the same values); `[defaults]` in the config file and flags override them.
#[derive(Clone, Debug, PartialEq, Serialize, schemars::JsonSchema)]
pub struct BuiltinDefaults {
    pub workload: WorkloadSpec,
    pub knobs: Knobs,
    pub sla: Sla,
    pub cost: CostSpec,
    pub tune: TuneDefaults,
    /// Spare in-flight capacity kept below a measured ceiling (`analysis::capacity`).
    pub headroom_ratio: f64,
    /// The demo target (`oprobe demo`, the App's Demo button).
    pub demo_target: String,
    /// The demo's measurement window.
    pub demo_duration: HumanDuration,
}

/// The built-in defaults (see [`BuiltinDefaults`]).
pub fn defaults() -> BuiltinDefaults {
    let d = |s: &str| HumanDuration(crate::util::parse_duration(s).unwrap_or_default());
    BuiltinDefaults {
        workload: WorkloadSpec::default(),
        knobs: Knobs::default(),
        sla: Sla::default(),
        cost: CostSpec::default(),
        tune: TuneDefaults {
            trial_duration: d(tune_defaults::TRIAL_DURATION),
            trial_warmup: d(tune_defaults::TRIAL_WARMUP),
            max_trials: tune_defaults::MAX_TRIALS,
            min_trials: tune_defaults::MIN_TRIALS,
            max_concurrency: tune_defaults::MAX_CONCURRENCY,
            cooldown: d(tune_defaults::COOLDOWN),
            max_throttle_ratio: tune_defaults::MAX_THROTTLE_RATIO,
            boundary_repeats: tune_defaults::BOUNDARY_REPEATS,
        },
        headroom_ratio: crate::analysis::capacity::DEFAULT_HEADROOM_RATIO,
        demo_target: "lab://demo".into(),
        demo_duration: HumanDuration::secs(8),
    }
}

/// Complete, round-trippable benchmark profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// OpenDAL scheme; derived from `url` when empty (and then not written back).
    #[serde(default, skip_serializing_if = "String::is_empty")]
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

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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
    /// Lakehouse scan reading the footer with a suffix range (no HEAD per file).
    ParquetSuffix,
    /// Churn: small writes removed again by batched deletes (OpenDAL `Deleter`).
    Delete,
    /// Server-side copies of dataset objects.
    Copy,
    /// LIST with a page size (`limit`): every page is a billed request.
    ListPaged,
    /// GETs through presigned URLs.
    Presign,
    /// Optimistic concurrency: conditional create (`If-None-Match: *`) and overwrite (`If-Match`).
    Conditional,
    /// HEAD storm plus small writes carrying user metadata.
    MetadataHeavy,
}

impl Preset {
    pub const ALL: [Preset; 12] = [
        Preset::Parquet,
        Preset::Checkpoint,
        Preset::SmallFiles,
        Preset::Metadata,
        Preset::Mixed,
        Preset::ParquetSuffix,
        Preset::Delete,
        Preset::Copy,
        Preset::ListPaged,
        Preset::Presign,
        Preset::Conditional,
        Preset::MetadataHeavy,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            Preset::Parquet => "parquet",
            Preset::Checkpoint => "checkpoint",
            Preset::SmallFiles => "small-files",
            Preset::Metadata => "metadata",
            Preset::Mixed => "mixed",
            Preset::ParquetSuffix => "parquet-suffix",
            Preset::Delete => "delete",
            Preset::Copy => "copy",
            Preset::ListPaged => "list-paged",
            Preset::Presign => "presign",
            Preset::Conditional => "conditional",
            Preset::MetadataHeavy => "metadata-heavy",
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Preset::Parquet => "lakehouse scan: footer + vectored column-chunk range reads",
            Preset::Checkpoint => "ML checkpoint: large multipart writes + full restores",
            Preset::SmallFiles => "small objects: whole-object GET/PUT",
            Preset::Metadata => "metadata storm: LIST + HEAD over partitions",
            Preset::Mixed => "general purpose: 70% reads / 20% writes / 10% stat",
            Preset::ParquetSuffix => {
                "lakehouse scan without HEAD: suffix footer read + vectored column chunks"
            }
            Preset::Delete => "churn: 4 KiB writes removed by batched deletes (20 keys/batch)",
            Preset::Copy => "server-side copies of 1 MiB objects",
            Preset::ListPaged => "paged LIST: 32 keys per page over 128-object partitions",
            Preset::Presign => "GETs through presigned URLs",
            Preset::Conditional => "optimistic concurrency: If-None-Match / If-Match writes",
            Preset::MetadataHeavy => "60% HEAD / 40% small writes with user metadata",
        }
    }

    /// Typed: see [`crate::input::preset`].
    pub fn parse(s: &str) -> Result<Self> {
        Ok(crate::input::preset(s)?)
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
                        suffix: false,
                    },
                }],
            ),
            Preset::ParquetSuffix => (
                ds(8, ByteSize::mib(16)),
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::ParquetScan {
                        columns: 4,
                        column_size: ByteSize::kib(512),
                        footer_size: ByteSize::kib(64),
                        suffix: true,
                    },
                }],
            ),
            Preset::Delete => (
                ds(1, ByteSize::kib(4)),
                vec![
                    OpSpec {
                        weight: 95,
                        op: OpKind::Write {
                            size: ByteSize::kib(4),
                        },
                    },
                    OpSpec {
                        weight: 5,
                        op: OpKind::Delete { batch: 20 },
                    },
                ],
            ),
            Preset::Copy => (
                ds(16, ByteSize::mib(1)),
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::Copy,
                }],
            ),
            Preset::ListPaged => (
                DatasetSpec {
                    objects: 512,
                    object_size: ByteSize::kib(1),
                    partitions: 4,
                    ..DatasetSpec::default()
                },
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::ListPaged { limit: 32 },
                }],
            ),
            Preset::Presign => (
                ds(32, ByteSize::kib(256)),
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::PresignedRead,
                }],
            ),
            Preset::Conditional => (
                ds(1, ByteSize::kib(16)),
                vec![OpSpec {
                    weight: 100,
                    op: OpKind::ConditionalWrite {
                        size: ByteSize::kib(16),
                    },
                }],
            ),
            Preset::MetadataHeavy => (
                DatasetSpec {
                    objects: 256,
                    object_size: ByteSize::kib(4),
                    partitions: 8,
                    ..DatasetSpec::default()
                },
                vec![
                    OpSpec {
                        weight: 60,
                        op: OpKind::Stat,
                    },
                    OpSpec {
                        weight: 40,
                        op: OpKind::MetadataWrite {
                            size: ByteSize::kib(4),
                            entries: 8,
                        },
                    },
                ],
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpSpec {
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(flatten)]
    pub op: OpKind,
}

fn default_weight() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpKind {
    /// Read a random range of `size` bytes, or the whole object when `size` is omitted.
    Read {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<ByteSize>,
    },
    /// `stat` + footer read + `columns` random column chunks fetched as one vectored read.
    /// With `suffix`, the footer is read with a suffix range (`bytes=-N`) and no `stat`: one
    /// request fewer per file.
    ParquetScan {
        columns: u32,
        column_size: ByteSize,
        footer_size: ByteSize,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        suffix: bool,
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
    /// LIST one dataset partition `limit` keys per page (each page is a billed request).
    ListPaged { limit: u32 },
    /// Delete up to `batch` objects the workload wrote, in one batch (OpenDAL `Deleter`:
    /// `DeleteObjects` where the service batches).
    Delete {
        #[serde(default = "default_delete_batch")]
        batch: u32,
    },
    /// Server-side copy of a dataset object to a scratch key.
    Copy,
    /// GET a dataset object through a presigned URL.
    PresignedRead,
    /// Optimistic-concurrency write of `size` bytes: create with `If-None-Match: *`, then
    /// overwrite with `If-Match: <etag of the previous write>`.
    ConditionalWrite { size: ByteSize },
    /// Small write carrying `entries` user-metadata pairs and a content type.
    MetadataWrite {
        size: ByteSize,
        #[serde(default = "default_meta_entries")]
        entries: u32,
    },
}

fn default_delete_batch() -> u32 {
    100
}
fn default_meta_entries() -> u32 {
    4
}

impl OpKind {
    /// The operation writes or deletes (refused on read-only datasets).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            OpKind::Write { .. }
                | OpKind::MultipartWrite { .. }
                | OpKind::Delete { .. }
                | OpKind::Copy
                | OpKind::ConditionalWrite { .. }
                | OpKind::MetadataWrite { .. }
        )
    }

    /// The operation works on dataset objects (the dataset must be prepared).
    pub fn needs_dataset(&self) -> bool {
        matches!(
            self,
            OpKind::Read { .. }
                | OpKind::ParquetScan { .. }
                | OpKind::Stat
                | OpKind::List { .. }
                | OpKind::ListPaged { .. }
                | OpKind::Copy
                | OpKind::PresignedRead
        )
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
    /// Typed: see [`crate::input::access`].
    fn from_str(s: &str) -> Result<Self> {
        Ok(crate::input::access(s)?)
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

impl schemars::JsonSchema for Access {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Access".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Key popularity: `uniform`, `zipf:S` (e.g. `zipf:1.1`) or `hotset:F/P` (e.g. `hotset:0.2/0.8`)."
        })
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Provider request options (addressing style, checksums, SSE), applied when the session
    /// opens.
    #[serde(default, skip_serializing_if = "ProviderKnobs::is_default")]
    pub provider: ProviderKnobs,
}

/// S3 bucket addressing.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Addressing {
    /// As the target configures it (OpenDAL's default: path style unless
    /// `enable_virtual_host_style`).
    #[default]
    Auto,
    /// `https://bucket.s3.region.amazonaws.com/key`.
    Virtual,
    /// `https://s3.region.amazonaws.com/bucket/key`.
    Path,
}

/// Provider request options measured for their overhead (S3 service options).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderKnobs {
    #[serde(default)]
    pub addressing: Addressing,
    /// Checksum the client computes and the server verifies on writes (`crc32c`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Server-side encryption: `AES256` or `aws:kms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<String>,
}

impl ProviderKnobs {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The target options these knobs set (S3 only; an error elsewhere).
    pub fn options(&self, scheme: &str) -> Result<Vec<(&'static str, String)>> {
        if self.is_default() {
            return Ok(Vec::new());
        }
        if scheme != "s3" {
            return Err(usage(format!(
                "addressing / checksum / sse knobs apply to S3-compatible targets only (this target is `{scheme}`)"
            )));
        }
        let mut o = Vec::new();
        match self.addressing {
            Addressing::Auto => {}
            Addressing::Virtual => o.push(("enable_virtual_host_style", "true".to_owned())),
            Addressing::Path => o.push(("enable_virtual_host_style", "false".to_owned())),
        }
        if let Some(c) = &self.checksum {
            let c = c.trim().to_ascii_lowercase();
            if c != "crc32c" {
                return Err(crate::input::invalid(format!("unsupported checksum `{c}`"))
                    .with_hint("OpenDAL's S3 service supports crc32c")
                    .into());
            }
            o.push(("checksum_algorithm", c));
        }
        if let Some(s) = &self.sse {
            let s = s.trim();
            if !matches!(s, "AES256" | "aws:kms" | "aws:kms:dsse") {
                return Err(crate::input::invalid(format!("unsupported sse `{s}`"))
                    .with_hint(crate::input::choices_hint(
                        s,
                        &["AES256", "aws:kms", "aws:kms:dsse"],
                    ))
                    .into());
            }
            o.push(("server_side_encryption", s.to_owned()));
        }
        Ok(o)
    }
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
            provider: ProviderKnobs::default(),
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
            + &self.provider_summary()
    }

    fn provider_summary(&self) -> String {
        let p = &self.provider;
        let mut s = String::new();
        if p.addressing != Addressing::Auto {
            s += &format!(" addressing={:?}", p.addressing).to_ascii_lowercase();
        }
        if let Some(c) = &p.checksum {
            s += &format!(" checksum={c}");
        }
        if let Some(e) = &p.sse {
            s += &format!(" sse={e}");
        }
        s
    }
}

/// Service level objectives. `oprobe run` / `oprobe tune` exit with code 3 when the measured
/// result breaches them (unless `enforce = false` / `--sla none`); the tuner rejects trials that
/// breach them and the diagnosis grades findings against them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sla {
    /// Turn a breach into exit code 3. Corrupted data (CRC mismatch) is always a breach.
    #[serde(default = "default_true")]
    pub enforce: bool,
    /// Max acceptable p99 per operation verb (ms); unset = no latency constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    /// Max fraction of failed operations (default 0.01 = 1%).
    #[serde(default = "default_error_rate")]
    pub max_error_rate: f64,
    /// Max fraction of operations that hit a 429/503 throttle. Unset: throttling that retries
    /// absorb does not breach the SLA of a run (it is still a finding), and the tuner treats
    /// any throttling as infeasible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_throttle_rate: Option<f64>,
}

fn default_error_rate() -> f64 {
    0.01
}

impl Default for Sla {
    fn default() -> Self {
        Self {
            enforce: true,
            p99_ms: None,
            max_error_rate: default_error_rate(),
            max_throttle_rate: None,
        }
    }
}

impl Sla {
    /// Throttle-rate limit the tuner applies (any throttling when unset).
    pub fn tune_throttle_limit(&self) -> f64 {
        self.max_throttle_rate.unwrap_or(0.0)
    }

    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.max_error_rate) {
            return Err(invalid(
                "sla.max_error_rate must be within 0..1 (e.g. 0.01 = 1%)",
            ));
        }
        if let Some(t) = self.max_throttle_rate
            && !(0.0..=1.0).contains(&t)
        {
            return Err(invalid("sla.max_throttle_rate must be within 0..1"));
        }
        if let Some(p) = self.p99_ms
            && !(p.is_finite() && p > 0.0)
        {
            return Err(invalid("sla.p99_ms must be > 0"));
        }
        Ok(())
    }
}

/// Business volume used to project measured per-operation costs to a monthly bill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Storage class the data is kept in: `standard` (default), `infrequent` or `archive`
    /// (the provider's equivalents: S3 Standard-IA / Glacier IR, GCS Nearline / Archive, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// Fraction of downloaded bytes that cross regions (inter-region transfer).
    #[serde(default, skip_serializing_if = "crate::config::is_zero_f64")]
    pub inter_region_fraction: f64,
}

fn default_monthly_ops() -> f64 {
    100_000_000.0
}
pub(crate) fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
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
            storage_class: None,
            inter_region_fraction: 0.0,
        }
    }
}

impl TargetSpec {
    /// `fs://PATH` / `file://PATH`: the path is taken verbatim (not parsed as a URL), so Windows
    /// paths keep their drive and separators: `fs://C:\\bench`, `fs:///C:/bench`, `fs:///tmp/x`,
    /// `fs://relative/dir`. Empty → a directory under the temp dir. Query parameters become
    /// options like for other schemes.
    fn from_fs_url(raw: &str) -> Option<Self> {
        let lower = raw.to_ascii_lowercase();
        let rest = ["fs://", "file://"]
            .iter()
            .find_map(|p| lower.starts_with(p).then(|| &raw[p.len()..]))?;
        let (path, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let mut options = BTreeMap::new();
        if let Some(q) = query {
            for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
                options.insert(k.into_owned(), v.into_owned());
            }
        }
        // `/C:/x` (a file URL's form of a Windows path) → `C:/x`.
        let b = path.as_bytes();
        let path = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            &path[1..]
        } else {
            path
        };
        let trimmed = path.trim_end_matches(['/', '\\']);
        let root = if trimmed.is_empty() {
            // `fs://` or `fs:///`: never the filesystem root.
            std::env::temp_dir()
                .join("oprobe")
                .to_string_lossy()
                .into_owned()
        } else if trimmed.ends_with(':') {
            // `C:/`: keep the drive's root separator.
            path.to_owned()
        } else {
            trimmed.to_owned()
        };
        options.insert("root".into(), root);
        Some(Self {
            url: Some(raw.to_owned()),
            scheme: "fs".into(),
            options,
            ..Default::default()
        })
    }

    /// Build a target from a storage URL such as `s3://bucket/prefix`, `gcs://bucket`,
    /// `azblob://container/prefix`, `fs:///tmp/bench`, `memory://`, `lab://demo` (an in-process
    /// lab server, see [`crate::lab::Scenario`]) or any other OpenDAL scheme compiled in
    /// (`azdls://`, `obs://`, `swift://`, `tos://`, ... via the service's `Config::from_uri`).
    /// Query parameters become service options: `s3://b/p?region=eu-west-1`.
    pub fn from_url(raw: &str) -> Result<Self> {
        if let Some(t) = Self::from_fs_url(raw) {
            return Ok(t);
        }
        crate::input::storage_url(raw)?;
        let u = url::Url::parse(raw).map_err(|e| {
            crate::input::invalid(format!("invalid storage URL `{raw}`: {e}"))
                .with_hint(crate::input::URL_EXAMPLES)
        })?;
        let scheme = u.scheme().to_ascii_lowercase();
        let mut options = BTreeMap::new();
        for (k, v) in u.query_pairs() {
            options.insert(k.into_owned(), v.into_owned());
        }
        let raw_no_query = match raw.split_once('?') {
            Some((base, _)) => base,
            None => raw,
        };
        let path = u.path().trim_end_matches('/');
        let root = if path.is_empty() {
            None
        } else {
            Some(path.to_owned())
        };
        let host = u.host_str().map(str::to_owned).filter(|h| !h.is_empty());
        let need = |h: Option<String>, what: &str| -> Result<String> {
            h.ok_or_else(|| crate::input::missing_bucket(&scheme, what).into())
        };
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
                let bucket = need(host, "bucket")?;
                options.insert("bucket".into(), bucket);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "s3"
            }
            "gs" | "gcs" => {
                options.insert("bucket".into(), need(host, "bucket")?);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "gcs"
            }
            "oss" | "cos" | "b2" => {
                options.insert("bucket".into(), need(host, "bucket")?);
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
                options.insert("container".into(), need(host, "container")?);
                if let Some(r) = root {
                    options.insert("root".into(), r);
                }
                "azblob"
            }
            "webdav" | "webdavs" => {
                let s = if scheme == "webdavs" { "https" } else { "http" };
                let h = need(host, "host")?;
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
            "lab" => {
                let scenario = crate::input::lab_scenario(host.as_deref().unwrap_or_default())?;
                let lab = crate::lab::shared(scenario)?;
                options.insert("bucket".into(), crate::lab::LAB_BUCKET.into());
                options.insert("region".into(), "us-east-1".into());
                options.insert("endpoint".into(), lab.endpoint_url.clone());
                options.insert("access_key_id".into(), "oprobe-lab".into());
                options.insert("secret_access_key".into(), "oprobe-lab-secret".into());
                options.insert("disable_config_load".into(), "true".into());
                options.insert("root".into(), root.unwrap_or_else(|| "/".into()));
                return Ok(Self {
                    url: Some(raw.to_owned()),
                    scheme: "s3".into(),
                    options,
                    // Local server: never through an egress proxy.
                    proxy: Some("none".into()),
                    ..Default::default()
                });
            }
            other => match uri_options(other, raw_no_query)? {
                Some(o) => {
                    for (k, v) in o {
                        options.entry(k).or_insert(v);
                    }
                    other
                }
                None => {
                    return Err(crate::input::scheme(other)
                        .err()
                        .unwrap_or_else(|| {
                            crate::input::invalid(format!("unsupported storage scheme `{other}`"))
                                .with_hint(format!(
                                    "supported: {}, lab, memory",
                                    supported_schemes().join(", ")
                                ))
                        })
                        .into());
                }
            },
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
                .ok_or_else(|| invalid("target needs either `url` or `scheme`"))?;
            let mut base = Self::from_url(url)?;
            for (k, v) in &self.options {
                base.options.insert(k.clone(), v.clone());
            }
            base.insecure = self.insecure;
            base.ca_cert = self.ca_cert.clone();
            // `lab://` targets bring their own (`none`).
            base.proxy = self.proxy.clone().or(base.proxy);
            base
        } else {
            self.clone()
        };
        // Keep the user's URL but never its credentials (they moved into options above).
        if let Some(u) = &t.url {
            t.url = Some(crate::util::strip_url_credentials(u));
        }
        t.apply_provider_url()?;
        crate::service_options::validate_keys(&t.scheme, t.options.keys())?;
        let missing: Vec<&str> = required_options(&t.scheme)
            .iter()
            .copied()
            .filter(|k| t.options.get(*k).is_none_or(|v| v.trim().is_empty()))
            .collect();
        if !missing.is_empty() {
            return Err(crate::input::invalid(format!(
                "the `{}` service needs {}",
                t.scheme,
                missing
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(" and "),
            ))
            .with_hint(format!(
                "add `--option {}=VALUE` (or `?{}=VALUE` in the URL, or under [targets.NAME.options] in the config file)",
                missing[0], missing[0]
            ))
            .into());
        }
        Ok(t)
    }

    /// The OpenDAL scheme, derived from `url` when not set explicitly.
    pub fn service_scheme(&self) -> Result<String> {
        if !self.scheme.is_empty() {
            return Ok(self.scheme.clone());
        }
        let url = self
            .url
            .as_deref()
            .ok_or_else(|| invalid("target needs either `url` or `scheme`"))?;
        Ok(Self::from_url(url)?.scheme)
    }

    /// The scheme as the user wrote it in `url` (`r2`, `minio`, `s3a`, ...).
    pub fn url_scheme(&self) -> Option<String> {
        self.url
            .as_deref()
            .and_then(|u| u.split_once("://"))
            .map(|(s, _)| s.to_ascii_lowercase())
    }

    /// `r2://` and `minio://` are S3 with a provider-specific endpoint: never let them fall back
    /// to AWS. R2 takes `account_id` (→ `https://<account>.r2.cloudflarestorage.com`, region
    /// `auto`) or an explicit endpoint; MinIO needs its endpoint.
    fn apply_provider_url(&mut self) -> Result<()> {
        let Some(kind) = self.url_scheme() else {
            return Ok(());
        };
        let has_endpoint = self
            .options
            .get("endpoint")
            .is_some_and(|e| !e.trim().is_empty());
        match kind.as_str() {
            "r2" => {
                let account = self.options.remove("account_id");
                if !has_endpoint {
                    let Some(account) = account.filter(|a| !a.trim().is_empty()) else {
                        return Err(crate::input::invalid(
                            "r2:// needs your Cloudflare account_id or an endpoint",
                        )
                        .with_hint(
                            "add `--option account_id=<ACCOUNT_ID>` (or `account_id = \"${R2_ACCOUNT_ID}\"` \
                             under [targets.NAME.options]), or pass \
                             `--endpoint https://<ACCOUNT_ID>.r2.cloudflarestorage.com`",
                        )
                        .into());
                    };
                    self.options.insert(
                        "endpoint".into(),
                        format!("https://{}.r2.cloudflarestorage.com", account.trim()),
                    );
                }
                self.options
                    .entry("region".into())
                    .or_insert_with(|| "auto".into());
            }
            "minio" => {
                if !has_endpoint {
                    return Err(
                        crate::input::invalid("minio:// needs the server's endpoint")
                            .with_hint(
                                "pass `--endpoint http://<host>:9000` (or `endpoint = ...` under \
                             [targets.NAME.options])",
                            )
                            .into(),
                    );
                }
                self.options
                    .entry("enable_virtual_host_style".into())
                    .or_insert_with(|| "false".into());
            }
            _ => {}
        }
        Ok(())
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

/// URL schemes oprobe resolves (aliases included), for messages.
pub fn supported_schemes() -> Vec<&'static str> {
    vec![
        "s3", "r2", "minio", "gcs", "azblob", "azdls", "azfile", "oss", "cos", "obs", "tos", "b2",
        "swift", "webdav", "webhdfs", "upyun", "http", "fs",
    ]
}

/// Service options of `raw` from the OpenDAL service's own `Config::from_uri` (bucket /
/// container / account / endpoint / root, per service), or `None` for a scheme not compiled in.
fn uri_options(scheme: &str, raw: &str) -> Result<Option<BTreeMap<String, String>>> {
    use opendal::services::*;
    fn opts<C: opendal::Configurator>(raw: &str) -> Result<BTreeMap<String, String>> {
        let uri = opendal::OperatorUri::new(raw, Vec::<(String, String)>::new()).map_err(|e| {
            crate::input::invalid(format!("invalid storage URL `{raw}`: {e}"))
                .with_hint(crate::input::URL_EXAMPLES)
        })?;
        let cfg = C::from_uri(&uri).map_err(|e| {
            crate::input::invalid(format!("cannot read storage URL `{raw}`: {e}"))
                .with_hint(crate::input::URL_EXAMPLES)
        })?;
        let v = serde_json::to_value(&cfg)?;
        let mut out = BTreeMap::new();
        if let serde_json::Value::Object(m) = v {
            for (k, v) in m {
                // Only what the URL set: defaults (null, false, empty) stay unset.
                let s = match v {
                    serde_json::Value::String(s) if !s.is_empty() => s,
                    serde_json::Value::Bool(true) => "true".into(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                out.insert(k, s);
            }
        }
        Ok(out)
    }
    Ok(Some(match scheme {
        #[cfg(feature = "services-azdls")]
        "azdls" => opts::<AzdlsConfig>(raw)?,
        #[cfg(feature = "services-azfile")]
        "azfile" => opts::<AzfileConfig>(raw)?,
        #[cfg(feature = "services-obs")]
        "obs" => opts::<ObsConfig>(raw)?,
        #[cfg(feature = "services-swift")]
        "swift" => opts::<SwiftConfig>(raw)?,
        #[cfg(feature = "services-tos")]
        "tos" => opts::<TosConfig>(raw)?,
        #[cfg(feature = "services-webhdfs")]
        "webhdfs" => opts::<WebhdfsConfig>(raw)?,
        #[cfg(feature = "services-upyun")]
        "upyun" => opts::<UpyunConfig>(raw)?,
        "http" | "https" => {
            let mut o = BTreeMap::new();
            let u = url::Url::parse(raw)
                .map_err(|e| crate::input::invalid(format!("invalid URL `{raw}`: {e}")))?;
            o.insert(
                "endpoint".into(),
                format!(
                    "{}://{}",
                    u.scheme(),
                    u.host_str().filter(|h| !h.is_empty()).ok_or_else(|| {
                        crate::input::invalid(format!("`{raw}` needs a host"))
                            .with_hint("e.g. https://files.example.com/data")
                    })?
                ) + &u.port().map(|p| format!(":{p}")).unwrap_or_default(),
            );
            let path = u.path().trim_end_matches('/');
            if !path.is_empty() {
                o.insert("root".into(), path.to_owned());
            }
            return Ok(Some(o));
        }
        _ => return Ok(None),
    }))
}

/// Options each service cannot work without (checked before building, for a clear message).
pub fn required_options(scheme: &str) -> &'static [&'static str] {
    match scheme {
        "s3" | "gcs" | "oss" | "cos" | "b2" | "obs" | "tos" => &["bucket"],
        "azblob" => &["container"],
        "azdls" => &["filesystem", "endpoint"],
        "azfile" => &["share_name", "endpoint"],
        "swift" => &["endpoint", "container"],
        "webdav" | "webhdfs" | "http" => &["endpoint"],
        "upyun" => &["bucket", "operator", "password"],
        _ => &[],
    }
}

/// Backend options for unsigned, credential-free access to a public bucket (`--anonymous`), per
/// OpenDAL service. `skip_signature` is what OpenDAL 0.59 calls it (`allow_anonymous` is its
/// deprecated alias); credential discovery is disabled too so no config files or metadata
/// servers are consulted.
pub fn anonymous_options(scheme: &str) -> Result<&'static [(&'static str, &'static str)]> {
    Ok(match scheme {
        "s3" => &[("skip_signature", "true"), ("disable_config_load", "true")],
        "gcs" => &[
            ("skip_signature", "true"),
            ("disable_vm_metadata", "true"),
            ("disable_config_load", "true"),
        ],
        // No credentials at all: unsigned requests.
        "azblob" | "oss" => &[("skip_signature", "true")],
        other => {
            return Err(crate::input::invalid(format!(
                "--anonymous is not supported for `{other}` (OpenDAL's {other} service has no unsigned mode)"
            ))
            .with_hint("configure credentials instead (`oprobe doctor TARGET` shows which it finds)")
            .into());
        }
    })
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
            .ok_or_else(|| invalid(format!("unterminated `${{` in `{v}`")))?;
        let name = &after[..end];
        let val = std::env::var(name).map_err(|_| {
            crate::errors::OprobeError::new(
                crate::errors::ErrorCode::ConfigInvalid,
                format!("environment variable `{name}` referenced by the profile is not set"),
            )
            .with_hint(format!("export {name}=… or edit the reference"))
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Profile {
    /// A `lab://NAME` target starts from its scenario's default workload
    /// ([`crate::lab::Scenario::defaults`]).
    pub fn from_target(target: TargetSpec) -> Self {
        let scenario = target.url.as_deref().and_then(crate::lab::Scenario::of_url);
        let mut p = Self {
            name: None,
            target,
            workload: WorkloadSpec::default(),
            knobs: Knobs::default(),
            sla: Sla::default(),
            cost: CostSpec::default(),
        };
        if let Some(sc) = scenario {
            sc.apply_defaults(&mut p);
        }
        p
    }

    /// Load and validate a profile file: a missing file is `usage.invalid_argument`, a file that
    /// does not parse or validate is `config.invalid`.
    pub fn load(path: &Path) -> Result<Self> {
        use crate::errors::{ErrorCode, OprobeError};
        let raw = std::fs::read_to_string(path).map_err(|e| {
            OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("cannot read profile `{}`: {e}", path.display()),
            )
            .with_hint(
                "pass the path of a profile TOML file (`oprobe init > profile.toml` writes one)",
            )
        })?;
        let p: Profile = toml::from_str(&raw).map_err(|e| {
            OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid profile `{}`: {e}", path.display()),
            )
        })?;
        p.validate().map_err(|e| {
            let body = crate::errors::classify(&e, "s3");
            let mut o = OprobeError::new(
                ErrorCode::ConfigInvalid,
                format!("invalid profile `{}`: {}", path.display(), body.message),
            );
            o.hint = body.hint;
            o
        })?;
        Ok(p)
    }

    /// Check every value; errors are typed (`usage.invalid_argument`, or `config.unknown_option`
    /// for backend options the service would ignore).
    pub fn validate(&self) -> Result<()> {
        self.target.normalized()?;
        let k = &self.knobs;
        if k.concurrency < 1 {
            return Err(invalid("knobs.concurrency must be >= 1"));
        }
        if k.io_concurrency < 1 {
            return Err(invalid("knobs.io_concurrency must be >= 1"));
        }
        if k.chunk_size.0 < 4096 {
            return Err(invalid("knobs.chunk_size must be >= 4KiB"));
        }
        let w = &self.workload;
        if let Some(r) = w.rate
            && !(r.is_finite() && r > 0.0)
        {
            return Err(invalid("workload.rate must be > 0"));
        }
        let ds = w.effective_dataset();
        let mix = w.effective_mix();
        let reads = mix.iter().any(|m| m.weight > 0 && m.op.needs_dataset());
        if ds.existing_prefix.is_none() && ds.objects < 1 && reads {
            return Err(invalid(
                "workload.dataset.objects must be >= 1 for read workloads",
            ));
        }
        if ds.existing_prefix.is_none() && ds.object_size.0 == 0 && reads {
            return Err(invalid(
                "workload.dataset.object_size must be > 0 for read workloads",
            ));
        }
        if !mix.iter().any(|m| m.weight > 0) {
            return Err(invalid(
                "workload.mix needs at least one op with weight > 0",
            ));
        }
        let c = &self.cost;
        if !(0.0..=1.0).contains(&c.egress_fraction) {
            return Err(invalid("cost.egress_fraction must be within 0..1"));
        }
        if !(c.monthly_ops.is_finite() && c.monthly_ops >= 0.0) {
            return Err(invalid("cost.monthly_ops must be >= 0"));
        }
        if !(c.stored_gib.is_finite() && c.stored_gib >= 0.0) {
            return Err(invalid("cost.stored_gib must be >= 0"));
        }
        if let Some(p) = &c.provider {
            crate::input::provider(p)?;
        }
        if let Some(e) = self.target.options.get("endpoint")
            && !e.contains("${")
        {
            crate::input::endpoint(e)?;
        }
        self.sla.validate()?;
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
    fn anonymous_access_maps_per_service() {
        for scheme in ["s3", "gcs", "azblob", "oss"] {
            let opts = anonymous_options(scheme).unwrap();
            assert!(opts.contains(&("skip_signature", "true")), "{scheme}");
            // Every key is one the service accepts (none silently ignored).
            let keys: Vec<String> = opts.iter().map(|(k, _)| (*k).to_owned()).collect();
            crate::service_options::validate_keys(scheme, &keys).unwrap();
        }
        assert!(
            anonymous_options("gcs")
                .unwrap()
                .contains(&("disable_vm_metadata", "true"))
        );
        let e = anonymous_options("b2").unwrap_err().to_string();
        assert!(e.contains("not supported for `b2`"), "{e}");
        assert!(anonymous_options("fs").is_err());
        let t = TargetSpec::from_url("gs://b").unwrap();
        assert_eq!(
            TargetSpec {
                scheme: String::new(),
                ..t
            }
            .service_scheme()
            .unwrap(),
            "gcs"
        );
    }

    #[test]
    fn r2_and_minio_never_fall_back_to_aws() {
        let full = |e: anyhow::Error| {
            let b = crate::errors::classify(&e, "s3");
            assert_eq!(b.code, crate::errors::ErrorCode::UsageInvalidArgument);
            format!("{} {}", b.message, b.hint.unwrap_or_default())
        };
        let e = TargetSpec::from_url("r2://bkt").unwrap().normalized();
        assert!(full(e.unwrap_err()).contains("account_id"));
        let e = TargetSpec::from_url("minio://bkt").unwrap().normalized();
        assert!(full(e.unwrap_err()).contains("--endpoint"));

        let mut r2 = TargetSpec::from_url("r2://bkt/p").unwrap();
        r2.options.insert("account_id".into(), "abc123".into());
        let n = r2.normalized().unwrap();
        assert_eq!(
            n.endpoint().unwrap(),
            "https://abc123.r2.cloudflarestorage.com"
        );
        assert_eq!(n.options["region"], "auto");
        assert!(!n.options.contains_key("account_id"));
        assert_eq!(
            crate::analysis::cost::detect_provider("s3", n.endpoint().as_deref()),
            Some("cloudflare-r2")
        );
        // A profile written from it reloads to the same target.
        let toml = Profile::from_target(n.clone()).to_toml().unwrap();
        let back: Profile = toml::from_str(&toml).unwrap();
        assert_eq!(back.target.normalized().unwrap().endpoint(), n.endpoint());

        let mut m = TargetSpec::from_url("minio://bkt").unwrap();
        m.options
            .insert("endpoint".into(), "http://127.0.0.1:9000".into());
        let n = m.normalized().unwrap();
        assert_eq!(n.endpoint().unwrap(), "http://127.0.0.1:9000");
        assert_eq!(n.options["enable_virtual_host_style"], "false");
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
        assert!(
            !toml.contains("scheme"),
            "derived scheme is not written: {toml}"
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

[sla]                       # `oprobe run` / `tune` exit 3 when breached (`--sla none` disables)
enforce = true              # corrupted data always breaches the SLA
max_error_rate = 0.01       # fraction of failed operations
# p99_ms = 200.0            # per-operation p99 limit; tune rejects configurations above it
# max_throttle_rate = 0.0   # fraction of throttled operations (unset: tune rejects any throttling)

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

#[cfg(test)]
mod fs_url_tests {
    use super::TargetSpec;

    fn root(u: &str) -> String {
        let t = TargetSpec::from_url(u).unwrap();
        assert_eq!(t.scheme, "fs", "{u}");
        t.options["root"].clone()
    }

    #[test]
    fn fs_urls_keep_windows_and_unix_paths() {
        assert_eq!(root("fs:///tmp/x"), "/tmp/x");
        assert_eq!(root("fs:///tmp/x/"), "/tmp/x");
        assert_eq!(root(r"fs://C:\Users\me\bench\"), r"C:\Users\me\bench");
        assert_eq!(root("fs:///C:/Users/me/bench"), "C:/Users/me/bench");
        assert_eq!(root("file:///D:/data"), "D:/data");
        assert_eq!(root("fs://C:/"), "C:/");
        assert_eq!(root("fs://relative/dir"), "relative/dir");
        assert!(root("fs://").ends_with("oprobe"));
        assert!(root("fs:///").ends_with("oprobe"));
        let t = TargetSpec::from_url("fs:///tmp/x?atomic_write_dir=/tmp/w").unwrap();
        assert_eq!(t.options["atomic_write_dir"], "/tmp/w");
    }
}
