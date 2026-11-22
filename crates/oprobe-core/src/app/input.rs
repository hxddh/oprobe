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

/// How a run's profile is put together: the operator, the workload flags and `[defaults]`,
/// then `set` pairs and the label (as `oprobe run`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProfileRequest {
    /// The operator: a URL (`s3://bucket/prefix`, `lab://demo`), an operator file (`op.toml`)
    /// or a name of the config file's `[operators.NAME]`
    pub operator: String,
    pub workload: WorkloadInput,
    pub sla: SlaInput,
    /// `--set PATH=VALUE` pairs (operator paths, workload and SLA keys), applied last
    pub set: Vec<String>,
    /// Label stored in the report
    pub label: Option<String>,
}

/// Safety rails of commands that touch storage (design §10).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Safety {
    /// Print the plan (requests, bytes, objects written/deleted, cost, ETA) and exit
    pub dry_run: bool,
    /// Refuse every write and delete (exit 5); implied by --existing
    pub read_only: bool,
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
            dry_run: false,
            read_only: false,
            max_requests: None,
            max_bytes: None,
            max_duration: None,
            idempotency_key: None,
        }
    }
}

impl Safety {
    /// Whether every write and delete is refused.
    pub fn refuses_writes(&self) -> bool {
        self.read_only
    }
}

/// What to run.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct WorkloadInput {
    /// Workload preset: small-files, large-objects, range-reads, list-stat
    pub preset: Option<String>,
    /// Concurrent operations
    pub concurrency: Option<usize>,
    /// Seed of the workload's random choices
    pub seed: Option<u64>,
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
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct TuneRequest {
    /// The operator (as `oprobe run OPERATOR`)
    pub operator: String,
    pub workload: WorkloadInput,
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
    /// The operator (as `oprobe run OPERATOR`)
    pub operator: String,
    pub workload: WorkloadInput,
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
            operator: Default::default(),
            workload: Default::default(),
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
    /// Group the requests: by dominant stage, API or connection
    pub by: Option<crate::analysis::slice::By>,
}

impl Default for RunsShowRequest {
    fn default() -> Self {
        Self {
            run: "last".into(),
            detail: Default::default(),
            by: None,
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

