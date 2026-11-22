//! Typed inputs of the service layer: the flag groups every front end fills (the CLI from
//! its clap arguments, MCP from tool arguments, the App from its forms). They carry the values
//! as the user typed them; [`super::App`] validates and resolves them (`crate::input`).
//!
//! None of these types depends on clap: `oprobe` (the binary) converts its argument structs
//! into them.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::config::tune_defaults;
use crate::net::HttpVersion;
pub use crate::tune::Objective;

/// `--fail-on`: findings of this severity or worse fail the command (exit 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailOn {
    Warning,
    Critical,
}

/// `--sla`: whether an SLA breach fails the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SlaMode {
    /// Exit with code 3 when the SLA is breached (the default)
    Enforce,
    /// Report the SLA verdict but never fail on it
    None,
}

/// `runs export --format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Json,
    Ndjson,
    Csv,
    Har,
    Otlp,
    Md,
    Html,
}

/// `runs export --signal` (OTLP).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OtlpSignal {
    #[default]
    Traces,
    Metrics,
    All,
}

impl OtlpSignal {
    pub fn signals(self) -> &'static [&'static str] {
        match self {
            Self::Traces => &["traces"],
            Self::Metrics => &["metrics"],
            Self::All => &["traces", "metrics"],
        }
    }
}

/// How a run's profile is put together: the flag groups, or a stored run (`from`) with the
/// flags on top, then `set` pairs and the label (as `oprobe run`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProfileRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub cost: CostInput,
    pub sla: SlaInput,
    /// Replay a stored run (RUNREF): its spec, with credentials resolved again from the
    /// target; the flag groups and `set` override it
    pub from: Option<String>,
    /// `--set KEY=VALUE` pairs, applied last
    pub set: Vec<String>,
    /// Label stored in the report
    pub label: Option<String>,
}

/// Safety rails of commands that touch storage (design §10).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Safety {
    /// Writes to the target are allowed at all (false: a run that would write to a target
    /// outside this process is refused; the MCP server without `--allow-write` sets it)
    pub allow_write: bool,
    /// Print the plan (requests, bytes, objects written/deleted, cost, ETA) and exit
    pub dry_run: bool,
    /// Refuse every write and delete (exit 5); implied by --existing
    pub read_only: bool,
    /// Stop when the estimated request cost exceeds this (USD; exit 4)
    pub max_cost_usd: Option<f64>,
    /// Stop after this many backend requests (exit 4)
    pub max_requests: Option<u64>,
    /// Stop after transferring this many bytes, e.g. 10GiB (exit 4)
    pub max_bytes: Option<String>,
    /// Stop after this much wall time, e.g. 5m (exit 4)
    pub max_duration: Option<String>,
    /// Name the run prefix after KEY: a retried command reuses and cleans it up
    pub idempotency_key: Option<String>,
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            allow_write: true,
            dry_run: false,
            read_only: false,
            max_cost_usd: None,
            max_requests: None,
            max_bytes: None,
            max_duration: None,
            idempotency_key: None,
        }
    }
}

impl Safety {
    /// Whether every write and delete is refused (`read_only`, or writes not allowed).
    pub fn refuses_writes(&self) -> bool {
        self.read_only || !self.allow_write
    }
}

/// Where to benchmark.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct TargetInput {
    /// Target name (see `oprobe target list`) or storage URL: s3://bucket/prefix, gcs://bucket,
    /// azblob://container, oss://, cos://, b2://, fs:///path, memory://
    pub target: Option<String>,
    /// Load a profile (TOML); flags override its values
    pub profile: Option<PathBuf>,
    /// Custom endpoint (MinIO, R2, Ceph, private clouds)
    pub endpoint: Option<String>,
    /// Region of the bucket (e.g. us-east-1); most S3-compatible services accept any value
    pub region: Option<String>,
    /// Extra backend option, repeatable (e.g. --option enable_virtual_host_style=true)
    pub options: Vec<String>,
    /// Access a public bucket without credentials (s3, gcs, azblob, oss: unsigned requests)
    pub anonymous: bool,
    /// Trust an extra PEM CA bundle
    pub ca_cert: Option<PathBuf>,
    /// Skip TLS certificate verification (development only)
    pub insecure: bool,
    /// Egress proxy URL, or `none` to ignore HTTPS_PROXY/HTTP_PROXY
    pub proxy: Option<String>,
}

/// What to run.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct WorkloadInput {
    /// Workload preset: parquet, checkpoint, small-files, metadata, mixed
    pub preset: Option<String>,
    /// Measurement window (e.g. 30s, 2m)
    pub duration: Option<String>,
    /// Warmup before measuring (connections, caches)
    pub warmup: Option<String>,
    /// Stop after N measured operations
    pub max_ops: Option<u64>,
    /// Open-loop arrival rate (ops/s); latency is measured from the intended start time
    pub rate: Option<f64>,
    /// Benchmark existing objects under this prefix (read-only)
    pub existing: Option<String>,
    /// Number of dataset objects to generate
    pub objects: Option<usize>,
    /// Size of each generated object (e.g. 16MiB)
    pub object_size: Option<String>,
    /// Key popularity: uniform, zipf:1.1, hotset:0.2/0.8
    pub access: Option<String>,
    /// Keep the generated dataset for later runs (oprobe-dataset/<fingerprint>/)
    pub persist_dataset: bool,
    /// Skip CRC32C verification
    pub no_verify: bool,
    /// Do not delete test data when the run ends
    pub keep_data: bool,
    /// Open-loop arrival process: constant or poisson
    pub arrivals: Option<String>,
    /// Time-varying open-loop rate: ramp:FROM..TO/D, step:R1,R2,…/D, diurnal[:MIN..MAX/P]
    pub shape: Option<String>,
    /// Replay a traffic log (S3 server access log, CloudTrail S3 data events, oprobe run file)
    pub replay: Option<PathBuf>,
    /// Replay speed factor (2 = twice as fast as recorded)
    pub speed: Option<f64>,
    /// Rewrite replayed keys: FROM=TO prefix pairs, repeatable
    pub map_prefix: Vec<String>,
}

/// Client configuration under test.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct KnobInput {
    /// Concurrent operations (workers)
    pub concurrency: Option<usize>,
    /// Per-operation reader/writer parallelism
    pub io_concurrency: Option<usize>,
    /// Reader/writer chunk and multipart part size (e.g. 8MiB)
    pub chunk_size: Option<String>,
    /// Merge vectored ranges closer than this (e.g. 64KiB)
    pub range_gap: Option<String>,
    /// Cap in-flight HTTP requests (0 = unlimited)
    pub max_inflight: Option<usize>,
    /// HTTP version
    pub http: Option<HttpVersion>,
    /// Open a new connection for every request
    pub no_keep_alive: bool,
    /// Use oprobe's instrumented connection pool instead of OpenDAL's own HTTP client:
    /// exact pool queueing, per-dial DNS/TCP/proxy/TLS and HTTP/2 streams, at the cost of
    /// measuring a different client than production runs
    pub microscope: bool,
    /// Enable the in-memory read cache with this size (e.g. 512MiB)
    pub cache: Option<String>,
    /// Retry attempts for temporary errors
    pub retries: Option<usize>,
    /// Cancel and retry straggler requests (TailCutLayer)
    pub tail_cut: bool,
    /// S3 bucket addressing: auto (as configured), virtual (bucket.host) or path (host/bucket)
    pub addressing: Option<String>,
    /// Checksum computed by the client and verified by the server on writes (S3: crc32c)
    pub checksum: Option<String>,
    /// Server-side encryption of written objects (S3: AES256, aws:kms)
    pub sse: Option<String>,
}

/// Cost projection inputs.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct CostInput {
    /// Pricing preset: aws-s3, gcs, azure-blob, cloudflare-r2, backblaze-b2, aliyun-oss
    /// (aliases: aws, gcp, google, azure, r2, b2, oss, aliyun)
    pub provider: Option<String>,
    /// Logical operations per month to project costs for
    pub monthly_ops: Option<f64>,
    /// Fraction of downloads leaving the cloud (0..1)
    pub egress_fraction: Option<f64>,
    /// Stored data in GiB
    pub stored_gib: Option<f64>,
}

/// Service level objectives (`[sla]` in a profile). By default `oprobe run` / `tune` exit with
/// code 3 when more than 1% of operations fail or any data is corrupted.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct SlaInput {
    /// `none` reports the SLA verdict without failing the command
    pub sla: Option<SlaMode>,
    /// Max share of failed operations, e.g. 1% or 0.01 (default 1%)
    pub max_error_rate: Option<String>,
    /// Max share of throttled (429/503) operations, e.g. 0% (default: not checked by `run`;
    /// `tune` rejects any throttling)
    pub max_throttle_rate: Option<String>,
    /// Max p99 latency per operation in ms (tune rejects trials above it)
    pub p99_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
/// What to do with a measured run besides returning it.
pub struct OutputOptions {
    /// Also save the report as JSON (input for `oprobe compare`)
    pub json: Option<PathBuf>,
    /// Also save a Markdown report
    pub markdown: Option<PathBuf>,
    /// Do not save the run in the run store
    pub no_save: bool,
    /// Also exit with code 3 if the diagnosis has findings of this severity or worse
    /// (SLA breaches always do, unless `--sla none`)
    pub fail_on: Option<FailOn>,
    /// Label stored in the report (e.g. a git SHA)
    pub label: Option<String>,
}

/// The tuner's search flags (shared by `tune` and `fix`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TuneSearch {
    /// What to optimize
    pub objective: Objective,
    /// Measurement window per trial
    pub trial_duration: String,
    /// Warmup per trial
    pub trial_warmup: String,
    /// Maximum number of trials
    pub max_trials: usize,
    /// Highest concurrency to try
    pub max_concurrency: usize,
    /// Pause after a throttled trial
    pub cooldown: String,
    /// Share of throttled requests a configuration may cause and still be feasible
    /// (default 0.001, or the profile's `sla.max_throttle_rate`)
    pub max_throttle_ratio: Option<f64>,
    /// Runs of each bisection trial near the throttle boundary
    pub boundary_repeats: usize,
    /// In-flight headroom kept below the throttling ceiling (default 0.15)
    pub headroom_ratio: Option<f64>,
    /// Posterior chance of exceeding the throttle budget the recommendation may carry
    /// (default 0.05)
    #[serde(default)]
    pub risk_ratio: Option<f64>,
}

impl Default for TuneSearch {
    fn default() -> Self {
        Self {
            objective: Objective::Throughput,
            trial_duration: tune_defaults::TRIAL_DURATION.into(),
            trial_warmup: tune_defaults::TRIAL_WARMUP.into(),
            max_trials: tune_defaults::MAX_TRIALS,
            max_concurrency: tune_defaults::MAX_CONCURRENCY,
            cooldown: tune_defaults::COOLDOWN.into(),
            max_throttle_ratio: Default::default(),
            boundary_repeats: tune_defaults::BOUNDARY_REPEATS,
            headroom_ratio: Default::default(),
            risk_ratio: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct TuneRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub cost: CostInput,
    pub sla: SlaInput,
    pub output: OutputOptions,
    pub safety: Safety,
    pub search: TuneSearch,
    /// Total wall-time budget of the tuner, e.g. 5m: caps the trials so the estimate fits
    /// (add --max-duration for a hard stop)
    pub budget: Option<String>,
    /// Write the tuned profile (TOML)
    pub emit_profile: Option<PathBuf>,
    /// Write a Rust/OpenDAL snippet with the tuned settings
    pub emit_rust: Option<PathBuf>,
    /// Write a Python/OpenDAL snippet with the tuned settings
    pub emit_python: Option<PathBuf>,
}

/// `oprobe verify RUNREF [CODE|--all] [--repeat N]`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct VerifyRequest {
    /// The run whose findings to test (RUNREF: id, prefix, last, last~N, label)
    pub run: String,
    /// The finding code to verify (default: every finding with a verification)
    pub code: Option<String>,
    /// Verify every finding that has a verification
    pub all: bool,
    /// Baseline / candidate pairs to run (interleaved A B A B…)
    pub repeat: usize,
    /// Do not store the outcomes or calibrate the capacity history and noise floor
    pub no_calibrate: bool,
    pub safety: Safety,
    /// Do not save the replayed runs in the run store
    pub no_save: bool,
}

impl Default for VerifyRequest {
    fn default() -> Self {
        Self {
            run: Default::default(),
            code: Default::default(),
            all: Default::default(),
            repeat: 3,
            no_calibrate: Default::default(),
            safety: Default::default(),
            no_save: Default::default(),
        }
    }
}

/// `oprobe fix TARGET`: diagnose → tune → verify.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FixRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub cost: CostInput,
    pub sla: SlaInput,
    pub output: OutputOptions,
    pub safety: Safety,
    pub search: TuneSearch,
    /// Baseline / candidate pairs of the verification
    pub repeat: usize,
    /// Write the recommended profile (TOML) to this file
    pub write_profile: Option<PathBuf>,
    /// Total wall-time budget of the tuning step, e.g. 5m (caps the trials so the estimate fits)
    pub budget: Option<String>,
    /// Fix the workload of a stored run (RUNREF): its spec, with credentials resolved again
    /// from the target; other flags and --set override it (as `run --from`)
    pub from: Option<String>,
    /// Override one setting of the workload, repeatable (as `run --set`)
    pub set: Vec<String>,
}

impl Default for FixRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            workload: Default::default(),
            knobs: Default::default(),
            cost: Default::default(),
            sla: Default::default(),
            output: Default::default(),
            safety: Default::default(),
            search: Default::default(),
            repeat: 2,
            write_profile: Default::default(),
            budget: Default::default(),
            from: Default::default(),
            set: Default::default(),
        }
    }
}

/// `oprobe baseline calibrate TARGET`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CalibrateRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub safety: Safety,
    /// Identical runs to measure (N − 1 A/A pairs)
    pub repeat: usize,
    /// Calibrate the workload of a stored run instead (RUNREF; its spec is replayed)
    pub from: Option<String>,
    /// Do not save the calibration runs in the run store
    pub no_save: bool,
}

impl Default for CalibrateRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            workload: Default::default(),
            knobs: Default::default(),
            safety: Default::default(),
            repeat: 4,
            from: Default::default(),
            no_save: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CompareRequest {
    /// Baseline: RUNREF (id, prefix, last~N, label) or a .oprobe / report .json file
    pub baseline: Option<String>,
    /// Candidate: RUNREF or file (default: `last`)
    pub candidate: Option<String>,
    /// Compare against a named baseline instead of A
    pub baseline_name: Option<String>,
    /// Max p99 increase per operation (%)
    pub max_p99_increase: f64,
    /// Max p50 increase per operation (%)
    pub max_p50_increase: f64,
    /// Ignore latency changes smaller than this (ms)
    pub min_latency_delta_ms: f64,
    /// Max throughput drop (%)
    pub max_throughput_drop: f64,
    /// Max increase of failed operations (percentage points)
    pub max_error_increase: f64,
    /// Max increase of requests per operation (%)
    pub max_request_amp_increase: f64,
    /// Max increase of read amplification (%)
    pub max_read_amp_increase: f64,
    /// Max increase of cost per operation (%)
    pub max_cost_increase: f64,
    /// Minimum samples per operation for latency checks
    pub min_samples: u64,
    /// Do not fail when the candidate starts getting throttled
    pub allow_throttling: bool,
    /// Do not fail when the workload differs (preset, mix, dataset, access, rate)
    pub allow_workload_change: bool,
    /// Do not fail when the target differs (scheme, bucket/prefix, endpoint, options)
    pub allow_target_change: bool,
    /// Runs of different targets or workloads are an error (exit 2) instead of a warning
    pub strict: bool,
    /// A significant regression within the limits (`warn`) fails the gate too (exit 3)
    #[serde(default)]
    pub fail_on_warn: bool,
    /// Write a Markdown summary (PR comment / job summary)
    pub markdown: Option<PathBuf>,
    /// Write JUnit XML
    pub junit: Option<PathBuf>,
    /// Write the comparison as JSON
    pub json: Option<PathBuf>,
}

impl Default for CompareRequest {
    fn default() -> Self {
        Self {
            baseline: Default::default(),
            candidate: Default::default(),
            baseline_name: Default::default(),
            max_p99_increase: 20.0,
            max_p50_increase: 20.0,
            min_latency_delta_ms: 1.0,
            max_throughput_drop: 10.0,
            max_error_increase: 0.5,
            max_request_amp_increase: 10.0,
            max_read_amp_increase: 10.0,
            max_cost_increase: 10.0,
            min_samples: 30,
            allow_throttling: Default::default(),
            allow_workload_change: Default::default(),
            allow_target_change: Default::default(),
            strict: Default::default(),
            fail_on_warn: Default::default(),
            markdown: Default::default(),
            junit: Default::default(),
            json: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct DoctorRequest {
    pub target: TargetInput,
    /// Only check reachability and listing; never write (the default; kept for scripts)
    pub read_only: bool,
    /// Also write, read back, stat and delete a 256 KiB probe object under
    /// `oprobe-run-<time>-doctor/` (the default is read-only)
    pub write_probe: bool,
    /// Show the raw backend error of failed checks
    pub verbose: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SampleRequest {
    pub target: TargetInput,
    /// List under this prefix (relative to the target root)
    pub prefix: Option<String>,
    /// List at most N objects
    pub limit: usize,
}

impl Default for SampleRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            prefix: Default::default(),
            limit: super::sample::DEFAULT_LIMIT,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct TargetAddRequest {
    /// Name, e.g. prod
    pub name: String,
    /// Storage URL, e.g. s3://bucket/prefix
    pub url: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    /// env | profile:NAME | anonymous
    pub credentials: Option<String>,
    /// OpenDAL option, repeatable (KEY=VALUE)
    pub options: Vec<String>,
    /// Save without the quick read-only check
    pub no_test: bool,
    /// Replace a target of the same name (otherwise an existing name is refused)
    pub replace: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct TargetTestRequest {
    /// Target name (or a storage URL)
    pub name: String,
    /// Only check reachability and listing; never write (the default)
    pub read_only: bool,
    /// Also write, read back and delete a probe object
    pub write_probe: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CleanupRequest {
    pub target: TargetInput,
    /// Only remove run prefixes older than this
    pub older_than: String,
    /// Also remove persisted datasets (oprobe-dataset/)
    pub datasets: bool,
    /// Show what would be deleted (the default without --yes)
    pub dry_run: bool,
    /// Refuse to delete anything (implies --dry-run)
    pub read_only: bool,
    /// Delete what was found (without it, cleanup only lists it)
    pub yes: bool,
}

impl Default for CleanupRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            older_than: "1h".into(),
            datasets: Default::default(),
            dry_run: Default::default(),
            read_only: Default::default(),
            yes: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AnalyzeRequest {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe file
    pub run: String,
    /// Group requests by: api, conn, remote, stage, second, status
    pub by: Option<String>,
    /// Only requests started in this window, seconds from the window start (e.g. 5..10)
    pub window: Option<String>,
    /// Only requests matching all conditions, e.g. api=GetObject,status=503 (repeatable)
    pub r#where: Vec<String>,
    /// Attribute the slowest PCT percent of the (filtered) requests (default 1)
    pub slowest: Option<f64>,
}

impl Default for AnalyzeRequest {
    fn default() -> Self {
        Self {
            run: "last".into(),
            by: Default::default(),
            window: Default::default(),
            r#where: Default::default(),
            slowest: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ExplainRequest {
    /// A RUNREF (id, prefix, `last`, `last~N`, label, .oprobe path) or a finding code
    /// (e.g. `throttling`); `oprobe explain --list` lists the codes
    pub subject: Option<String>,
    /// List every finding code
    pub list: bool,
    /// Findings to narrate for a run
    pub top: usize,
}

impl Default for ExplainRequest {
    fn default() -> Self {
        Self {
            subject: Default::default(),
            list: Default::default(),
            top: 3,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunsListRequest {
    /// Show at most N runs
    pub limit: usize,
}

impl Default for RunsListRequest {
    fn default() -> Self {
        Self { limit: 20 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunsShowRequest {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe / report .json file
    pub run: String,
    /// Every column, per-stage and per-verb breakdowns, the timeline and the 5 slowest operations
    pub detail: bool,
}

impl Default for RunsShowRequest {
    fn default() -> Self {
        Self {
            run: "last".into(),
            detail: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct RunsRmRequest {
    /// Runs to delete (RUNREFs)
    pub runs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunsGcRequest {
    /// Keep the newest N runs
    pub keep: usize,
    /// Only delete runs older than this, e.g. 30d, 12h, 2w
    pub older_than: Option<String>,
    /// Show what would be deleted
    pub dry_run: bool,
}

impl Default for RunsGcRequest {
    fn default() -> Self {
        Self {
            keep: 50,
            older_than: Default::default(),
            dry_run: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct BaselineSetRequest {
    /// Baseline name (letters, digits, - _ .)
    pub name: String,
    /// Runs: RUNREFs, comma lists, or `all:LABEL`
    pub runs: Vec<String>,
    /// Use the newest N stored runs
    pub last: Option<usize>,
    /// Keep at most this many runs (rolling window; default 10, or the number given)
    pub window: Option<usize>,
    /// Add to the baseline's runs instead of replacing them
    pub append: bool,
}

/// `oprobe demo`: no flags needed.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DemoRequest {
    /// Measurement window of the demo run
    pub duration: String,
}

impl Default for DemoRequest {
    fn default() -> Self {
        Self {
            duration: "8s".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProbeRequest {
    /// http(s) endpoint, or a storage URL whose endpoint should be probed
    pub endpoint: String,
    pub samples: usize,
    pub http: HttpVersion,
    /// Use GET instead of HEAD for the cold/warm requests
    pub get: bool,
    pub timeout: String,
    pub ca_cert: Option<PathBuf>,
    pub insecure: bool,
    pub proxy: Option<String>,
    /// Also save the result as JSON
    pub json: Option<PathBuf>,
}

impl Default for ProbeRequest {
    fn default() -> Self {
        Self {
            endpoint: Default::default(),
            samples: 5,
            http: HttpVersion::Auto,
            get: Default::default(),
            timeout: "10s".into(),
            ca_cert: Default::default(),
            insecure: Default::default(),
            proxy: Default::default(),
            json: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct CompatRequest {
    pub target: TargetInput,
    /// `--read-only` refuses (compat writes test objects); budgets stop the suite (exit 4)
    pub safety: Safety,
    /// Exit 3 when the score (percent of the checks that ran) is below this
    pub min_score: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MatrixRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub cost: CostInput,
    pub sla: SlaInput,
    pub safety: Safety,
    /// Targets to compare (names or URLs, comma-separated or repeated)
    pub targets: Vec<String>,
    /// A knob and its values, repeatable: concurrency=8,16,32 · http=h1,h2 ·
    /// addressing=virtual,path · io_concurrency · chunk_size · range_gap · max_inflight ·
    /// keep_alive=on,off · cache=off,256MiB · retries · tail_cut=on,off · preset
    pub vary: Vec<String>,
    /// Rank cells by throughput, mib, p50, p99 or cost
    pub rank_by: String,
    /// Label of the stored runs (default matrix-<id>)
    pub label: Option<String>,
}

impl Default for MatrixRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            workload: Default::default(),
            knobs: Default::default(),
            cost: Default::default(),
            sla: Default::default(),
            safety: Default::default(),
            targets: Default::default(),
            vary: Default::default(),
            rank_by: "throughput".into(),
            label: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WatchRequest {
    pub target: TargetInput,
    pub workload: WorkloadInput,
    pub knobs: KnobInput,
    pub cost: CostInput,
    pub safety: Safety,
    /// Time between probe windows (each window lasts --duration, default 5s)
    pub interval: String,
    /// Rolling window the SLOs are evaluated over
    pub window: String,
    /// Service level objective, repeatable: p99<200ms, p50<50ms, errors<1%, throttle<1%,
    /// availability>99.9%
    pub slo: Vec<String>,
    /// Serve Prometheus metrics on ADDR (/metrics), e.g. 127.0.0.1:9464
    pub listen: Option<String>,
    /// Push OTLP metrics after every window (collector base URL, e.g. http://localhost:4318)
    pub otlp_endpoint: Option<String>,
    /// Stop after N probe windows (default: until Ctrl-C)
    pub count: Option<u64>,
}

impl Default for WatchRequest {
    fn default() -> Self {
        Self {
            target: Default::default(),
            workload: Default::default(),
            knobs: Default::default(),
            cost: Default::default(),
            safety: Default::default(),
            interval: "10s".into(),
            window: "5m".into(),
            slo: Default::default(),
            listen: Default::default(),
            otlp_endpoint: Default::default(),
            count: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ExportRequest {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe file
    pub run: String,
    /// Export format
    pub format: ExportFormat,
    /// Write to this file (default: stdout)
    pub out: Option<PathBuf>,
    /// OTLP/HTTP collector to push to (e.g. http://localhost:4318); with --format otlp
    pub endpoint: Option<String>,
    /// CSV rows: requests or ops
    pub table: String,
    /// OTLP signal: one OTLP/JSON document per signal; `all` needs --out DIR (two files) or
    /// --endpoint (posts /v1/traces and /v1/metrics)
    pub signal: OtlpSignal,
}

impl Default for ExportRequest {
    fn default() -> Self {
        Self {
            run: Default::default(),
            format: Default::default(),
            out: Default::default(),
            endpoint: Default::default(),
            table: "requests".into(),
            signal: OtlpSignal::Traces,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LabRequest {
    pub bind: String,
    /// Start from a fault preset: clean, demo, throttle, stragglers, lossy, flaky, parquet (the
    /// flags below override it)
    pub scenario: Option<String>,
    /// Answer every Nth request with 500 InternalError
    pub error_every: Option<usize>,
    /// Drop the connection on every Nth request
    pub reset_every: Option<usize>,
    /// Serve HTTPS (certificate for localhost / 127.0.0.1)
    pub tls: bool,
    /// Disable HTTP/2
    pub h1_only: bool,
    /// Added time-to-first-byte for every request (e.g. 20ms)
    pub latency: String,
    /// Make every Nth request a straggler
    pub straggler_every: usize,
    /// Extra delay for stragglers
    pub straggler: String,
    /// Return 503 SlowDown when more than N requests are in flight
    pub throttle_above: Option<usize>,
    /// Throttle every Nth request
    pub throttle_every: Option<usize>,
    /// Status code used for throttling (429 or 503)
    pub throttle_status: u16,
    /// Cap response bandwidth per request (KiB/s)
    pub bandwidth_kib: Option<u64>,
    /// Answer throttled requests without reading the upload and close the connection (as real
    /// gateways do), so clients see resets mid-upload
    pub throttle_early_close: bool,
    /// Delay for CompleteMultipartUpload
    pub complete_delay: String,
    /// Write the lab CA certificate (for --ca-cert) to this file
    pub write_ca: Option<PathBuf>,
    /// Accept a non-loopback `bind` address (the lab has no authentication)
    pub allow_remote: bool,
    /// Largest object body accepted, e.g. `1GiB` (unset: the built-in cap)
    pub max_object_size: Option<String>,
    /// Largest in-memory store, e.g. `8GiB` (unset: the built-in cap)
    pub max_store_size: Option<String>,
    /// Most open connections (unset: the built-in cap)
    pub max_connections: Option<usize>,
}

impl Default for LabRequest {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9000".into(),
            scenario: Default::default(),
            error_every: Default::default(),
            reset_every: Default::default(),
            tls: Default::default(),
            h1_only: Default::default(),
            latency: "0ms".into(),
            straggler_every: 0,
            straggler: "200ms".into(),
            throttle_above: Default::default(),
            throttle_every: Default::default(),
            throttle_status: 503,
            bandwidth_kib: Default::default(),
            throttle_early_close: Default::default(),
            complete_delay: "0ms".into(),
            write_ca: Default::default(),
            allow_remote: false,
            max_object_size: None,
            max_store_size: None,
            max_connections: None,
        }
    }
}

impl WatchRequest {
    /// Probe rate when `rate` is not given (ops/s): low enough to watch production.
    pub fn default_rate(&self) -> f64 {
        2.0
    }
}
