//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::net::HttpVersion;
pub use crate::output::OutputFormat;

const EXAMPLES: &str = "\
Examples:
  oprobe demo                                           # 30-second guided demo
  oprobe target add prod s3://my-bucket/bench --region eu-west-1
  oprobe run prod                                       # measure a saved target
  oprobe run s3://my-bucket/bench --preset parquet -d 30s --label base
  oprobe run s3://my-bucket/data --existing tables/events/ -c 64
  oprobe run s3://my-bucket/bench --dry-run             # plan: requests, bytes, cost, ETA
  oprobe runs list                                      # saved runs; `runs show last`
  oprobe compare base last --junit gate.xml             # regression gate between two runs
  oprobe tune s3://my-bucket/bench --emit-profile tuned.toml --emit-rust tuned.rs
  oprobe run memory:// -o ndjson                        # events + result for agents/CI
  oprobe top s3://my-bucket/bench                       # live workbench in this terminal
  oprobe gui s3://my-bucket/bench                       # the same workbench in a window
  oprobe doctor s3://my-bucket/bench                    # connectivity & permission checks
  oprobe probe https://s3.us-east-1.amazonaws.com       # DNS/TCP/TLS/TTFB microscope
  oprobe lab --tls --latency 20ms --throttle-above 32   # local S3 with faults

Docs: https://github.com/hxddh/oprobe/tree/main/docs";

/// Defaults of `oprobe tune`, shared with the app's Tune page so both run the same search.
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
}

/// A whole number that must be at least `MIN` (rejected with a clear error, never clamped).
fn at_least<const MIN: usize>(v: &str) -> Result<usize, String> {
    let n: usize = v
        .trim()
        .parse()
        .map_err(|_| format!("`{v}` is not a whole number"))?;
    if n < MIN {
        return Err(format!("must be at least {MIN}"));
    }
    Ok(n)
}

#[derive(Parser, Debug)]
#[command(
    name = "oprobe",
    version,
    about = "See which layer makes your object-storage I/O slow or expensive — then fix it.",
    long_about = "oprobe drives realistic workloads through Apache OpenDAL and traces every backend request: \
queue wait, DNS/TCP/TLS handshakes, request send, time-to-first-byte, transfer and consumer stall. \
It explains the bottleneck, projects the cloud bill, tunes the client configuration and gates regressions in CI.",
    after_help = EXAMPLES,
    propagate_version = true
)]
pub struct Cli {
    /// Output format of the result on stdout (logs and progress go to stderr)
    #[arg(
        short = 'o',
        long = "output",
        value_enum,
        global = true,
        env = "OPROBE_OUTPUT"
    )]
    pub output: Option<OutputFormat>,

    /// When to use colors.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    /// No progress or informational messages on stderr
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Never prompt or redraw (implied when stdin/stdout is not a terminal)
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// Config file (default: $OPROBE_CONFIG or <config dir>/oprobe/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Without a command, oprobe opens its workbench: a window when a display is available
    /// (builds with the `gui` feature), otherwise in this terminal.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Arguments of a command that a later 3.0 stream implements (accepted and ignored for now).
#[derive(Args, Debug, Clone, Default)]
pub struct PendingArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub args: Vec<String>,
}

// Parsed once per process; boxing the larger argument structs would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Measure a target: where time and money go (saves the run)
    Run(RunArgs),
    /// Start a lab target with throttling and stragglers, run it and narrate the result
    Demo(DemoArgs),
    /// Find the best client configuration for a workload (throttle-aware)
    Tune(TuneArgs),
    /// Regression gate: compare two runs (RUNREFs or files) and fail on regressions
    Compare(CompareArgs),
    /// Saved runs: list, show, open, export, rm, gc
    #[command(subcommand)]
    Runs(RunsCommand),
    /// Re-slice a run by API, connection, remote, stage or second
    Analyze(AnalyzeArgs),
    /// Narrate a run's causes and advice, or document a finding code
    Explain(ExplainArgs),
    /// Named targets: add, list, show, rm, test
    #[command(subcommand)]
    Target(TargetCommand),
    /// Check endpoint reachability, TLS, clock skew, credentials and permissions
    Doctor(DoctorArgs),
    /// Measure DNS / TCP / proxy / TLS handshakes and cold vs warm TTFB of an endpoint
    Probe(ProbeArgs),
    /// Continuous low-rate probing with SLOs and a Prometheus endpoint
    Watch(WatchArgs),
    /// Run the same workload across targets / knob values and rank them
    Matrix(MatrixArgs),
    /// Correctness & compatibility suite for a target
    Compat(CompatArgs),
    /// Named baselines: set, list, rm, show
    #[command(subcommand)]
    Baseline(BaselineCommand),
    /// Start a local S3-compatible server with programmable faults
    #[command(visible_alias = "serve-s3")]
    Lab(LabArgs),
    /// MCP server for agents (stdio, or --http ADDR)
    Mcp(McpArgs),
    /// Print the JSON Schema of a machine output (report, events, …)
    Schema(SchemaArgs),
    /// Describe what this oprobe can do (machine-readable)
    Capabilities(CapabilitiesArgs),
    /// Configuration: show (with sources), path, edit
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Remove test data left behind by interrupted runs
    Cleanup(CleanupArgs),
    /// Check, download, verify and install the latest release
    SelfUpdate(SelfUpdateArgs),
    /// Interactive workbench in this terminal: run, tune and probe with live charts
    #[command(visible_alias = "tui")]
    Top(TopArgs),
    /// The same workbench in a native window (macOS, Windows, Linux with GTK 4)
    #[command(visible_alias = "app")]
    Gui(TopArgs),
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Generate the man page
    Man {
        /// Output directory (one page per command)
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Print the CLI reference as Markdown instead (docs/cli.md)
        #[arg(long, conflicts_with = "out_dir")]
        markdown: bool,
    },
    /// Print a commented profile template
    #[command(hide = true)]
    Init(InitArgs),
}

#[derive(Subcommand, Debug)]
pub enum RunsCommand {
    /// List saved runs, newest first
    List(RunsListArgs),
    /// Show a run's full report (-o md|html|json for documents)
    Show(RunsShowArgs),
    /// Open a run in the app (native window)
    Open(RunRefArg),
    /// Export a run (html, md, json, ndjson, csv, har, otlp)
    Export(ExportArgs),
    /// Delete saved runs
    Rm(RunsRmArgs),
    /// Delete old runs (keeps the newest and every baseline run)
    Gc(RunsGcArgs),
}

#[derive(Args, Debug)]
pub struct RunsListArgs {
    /// Show at most N runs
    #[arg(short = 'n', long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct RunsShowArgs {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe / report .json file
    #[arg(value_name = "RUNREF", default_value = "last")]
    pub run: String,
    /// Every column, per-stage and per-verb breakdowns, the timeline and the 5 slowest operations
    #[arg(long)]
    pub detail: bool,
}

/// `oprobe demo`: no flags needed.
#[derive(Args, Debug, Clone)]
pub struct DemoArgs {
    /// Measurement window of the demo run
    #[arg(short, long, default_value = "8s", hide = true)]
    pub duration: String,
}

impl Default for DemoArgs {
    fn default() -> Self {
        Self {
            duration: "8s".into(),
        }
    }
}

/// `oprobe self-update`.
#[derive(Args, Debug, Clone, Default)]
pub struct SelfUpdateArgs {
    /// Only check whether a newer release exists (exit 0 either way)
    #[arg(long)]
    pub check: bool,
    /// Install this release tag instead of the latest (e.g. v3.0.1)
    #[arg(long, value_name = "TAG")]
    pub to: Option<String>,
    /// Reinstall even when already on that version
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RunRefArg {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe / report .json file
    #[arg(value_name = "RUNREF")]
    pub run: String,
}

#[derive(Args, Debug)]
pub struct RunsRmArgs {
    /// Runs to delete (RUNREFs)
    #[arg(value_name = "RUNREF", required = true)]
    pub runs: Vec<String>,
}

#[derive(Args, Debug)]
pub struct RunsGcArgs {
    /// Keep the newest N runs
    #[arg(long, default_value_t = 50)]
    pub keep: usize,
    /// Only delete runs older than this, e.g. 30d, 12h, 2w
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,
    /// Show what would be deleted
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug)]
pub enum TargetCommand {
    /// Add (or replace) a named target
    Add(TargetAddArgs),
    /// List configured targets
    List,
    /// Show one target
    Show(TargetNameArg),
    /// Remove a target
    Rm(TargetNameArg),
    /// Check a target (doctor: reachability, TLS, clock, credentials, permissions)
    Test(TargetTestArgs),
}

#[derive(Args, Debug)]
pub struct TargetAddArgs {
    /// Name, e.g. prod
    pub name: String,
    /// Storage URL, e.g. s3://bucket/prefix
    pub url: String,
    #[arg(long)]
    pub region: Option<String>,
    #[arg(long)]
    pub endpoint: Option<String>,
    /// env | profile:NAME | anonymous
    #[arg(long)]
    pub credentials: Option<String>,
    /// OpenDAL option, repeatable (KEY=VALUE)
    #[arg(long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,
    /// Save without the quick read-only check
    #[arg(long)]
    pub no_test: bool,
}

#[derive(Args, Debug)]
pub struct TargetTestArgs {
    /// Target name (or a storage URL)
    pub name: String,
    /// Only check reachability and listing; never write
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Args, Debug)]
pub struct TargetNameArg {
    pub name: String,
}

#[derive(Subcommand, Debug)]
pub enum BaselineCommand {
    /// Create or update a named baseline
    Set(BaselineSetArgs),
    /// List baselines
    List,
    /// Delete a baseline
    Rm(BaselineNameArg),
    /// Show a baseline's runs and their spread
    Show(BaselineNameArg),
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Effective settings and where each comes from
    Show,
    /// Paths of the config file and data directory
    Path,
    /// Open the config file in $VISUAL / $EDITOR (validated when the editor exits)
    Edit,
}

/// Safety rails of commands that touch storage (design §10).
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Safety")]
pub struct SafetyArgs {
    /// Print the plan (requests, bytes, objects written/deleted, cost, ETA) and exit
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse every write and delete (exit 5); implied by --existing
    #[arg(long)]
    pub read_only: bool,
    /// Stop when the estimated request cost exceeds this (USD; exit 4)
    #[arg(long, value_name = "USD")]
    pub max_cost_usd: Option<f64>,
    /// Stop after this many backend requests (exit 4)
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Stop after transferring this many bytes, e.g. 10GiB (exit 4)
    #[arg(long, value_name = "SIZE")]
    pub max_bytes: Option<String>,
    /// Stop after this much wall time, e.g. 5m (exit 4)
    #[arg(long, value_name = "DURATION")]
    pub max_duration: Option<String>,
    /// Name the run prefix after KEY: a retried command reuses and cleans it up
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

/// Where to benchmark.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target")]
pub struct TargetArgs {
    /// Target name (see `oprobe target list`) or storage URL: s3://bucket/prefix, gcs://bucket,
    /// azblob://container, oss://, cos://, b2://, fs:///path, memory://
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Load a profile (TOML); flags override its values
    #[arg(short, long, value_name = "FILE")]
    pub profile: Option<PathBuf>,
    /// Custom endpoint (MinIO, R2, Ceph, private clouds)
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Region of the bucket (e.g. us-east-1); most S3-compatible services accept any value
    #[arg(long)]
    pub region: Option<String>,
    /// Extra backend option, repeatable (e.g. --option enable_virtual_host_style=true)
    #[arg(long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,
    /// Access a public bucket without credentials (s3, gcs, azblob, oss: unsigned requests)
    #[arg(long)]
    pub anonymous: bool,
    /// Trust an extra PEM CA bundle
    #[arg(long, value_name = "PEM")]
    pub ca_cert: Option<PathBuf>,
    /// Skip TLS certificate verification (development only)
    #[arg(long)]
    pub insecure: bool,
    /// Egress proxy URL, or `none` to ignore HTTPS_PROXY/HTTP_PROXY
    #[arg(long)]
    pub proxy: Option<String>,
}

/// What to run.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Workload")]
pub struct WorkloadArgs {
    /// Workload preset: parquet, checkpoint, small-files, metadata, mixed
    #[arg(long)]
    pub preset: Option<String>,
    /// Measurement window (e.g. 30s, 2m)
    #[arg(short, long)]
    pub duration: Option<String>,
    /// Warmup before measuring (connections, caches)
    #[arg(long)]
    pub warmup: Option<String>,
    /// Stop after N measured operations
    #[arg(long)]
    pub max_ops: Option<u64>,
    /// Open-loop arrival rate (ops/s); latency is measured from the intended start time
    #[arg(long)]
    pub rate: Option<f64>,
    /// Benchmark existing objects under this prefix (read-only)
    #[arg(long, value_name = "PREFIX")]
    pub existing: Option<String>,
    /// Number of dataset objects to generate
    #[arg(long)]
    pub objects: Option<usize>,
    /// Size of each generated object (e.g. 16MiB)
    #[arg(long)]
    pub object_size: Option<String>,
    /// Key popularity: uniform, zipf:1.1, hotset:0.2/0.8
    #[arg(long)]
    pub access: Option<String>,
    /// Keep the generated dataset for later runs (oprobe-dataset/<fingerprint>/)
    #[arg(long)]
    pub persist_dataset: bool,
    /// Skip CRC32C verification
    #[arg(long)]
    pub no_verify: bool,
    /// Do not delete test data when the run ends
    #[arg(long)]
    pub keep_data: bool,
}

/// Client configuration under test.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Client knobs")]
pub struct KnobArgs {
    /// Concurrent operations (workers)
    #[arg(short = 'c', long, value_parser = at_least::<1>)]
    pub concurrency: Option<usize>,
    /// Per-operation reader/writer parallelism
    #[arg(long, value_parser = at_least::<1>)]
    pub io_concurrency: Option<usize>,
    /// Reader/writer chunk and multipart part size (e.g. 8MiB)
    #[arg(long)]
    pub chunk_size: Option<String>,
    /// Merge vectored ranges closer than this (e.g. 64KiB)
    #[arg(long)]
    pub range_gap: Option<String>,
    /// Cap in-flight HTTP requests (0 = unlimited)
    #[arg(long)]
    pub max_inflight: Option<usize>,
    /// HTTP version
    #[arg(long, value_enum)]
    pub http: Option<HttpArg>,
    /// Open a new connection for every request
    #[arg(long)]
    pub no_keep_alive: bool,
    /// Use oprobe's instrumented connection pool instead of OpenDAL's own HTTP client:
    /// exact pool queueing, per-dial DNS/TCP/proxy/TLS and HTTP/2 streams, at the cost of
    /// measuring a different client than production runs
    #[arg(long)]
    pub microscope: bool,
    /// Enable the in-memory read cache with this size (e.g. 512MiB)
    #[arg(long, value_name = "SIZE")]
    pub cache: Option<String>,
    /// Retry attempts for temporary errors
    #[arg(long)]
    pub retries: Option<usize>,
    /// Cancel and retry straggler requests (TailCutLayer)
    #[arg(long)]
    pub tail_cut: bool,
    /// S3 bucket addressing: auto (as configured), virtual (bucket.host) or path (host/bucket)
    #[arg(long, value_name = "STYLE")]
    pub addressing: Option<String>,
    /// Checksum computed by the client and verified by the server on writes (S3: crc32c)
    #[arg(long, value_name = "ALGO")]
    pub checksum: Option<String>,
    /// Server-side encryption of written objects (S3: AES256, aws:kms)
    #[arg(long, value_name = "MODE")]
    pub sse: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum HttpArg {
    Auto,
    H1,
    H2,
}

impl From<HttpArg> for HttpVersion {
    fn from(h: HttpArg) -> Self {
        match h {
            HttpArg::Auto => HttpVersion::Auto,
            HttpArg::H1 => HttpVersion::H1,
            HttpArg::H2 => HttpVersion::H2,
        }
    }
}

/// Cost projection inputs.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Cost")]
pub struct CostArgs {
    /// Pricing preset: aws-s3, gcs, azure-blob, cloudflare-r2, backblaze-b2, aliyun-oss
    #[arg(long)]
    pub provider: Option<String>,
    /// Logical operations per month to project costs for
    #[arg(long)]
    pub monthly_ops: Option<f64>,
    /// Fraction of downloads leaving the cloud (0..1)
    #[arg(long)]
    pub egress_fraction: Option<f64>,
    /// Stored data in GiB
    #[arg(long)]
    pub stored_gib: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, ValueEnum)]
pub enum FailOn {
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SlaMode {
    /// Exit with code 3 when the SLA is breached (the default)
    Enforce,
    /// Report the SLA verdict but never fail on it
    None,
}

/// Service level objectives (`[sla]` in a profile). By default `oprobe run` / `tune` exit with
/// code 3 when more than 1% of operations fail or any data is corrupted.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "SLA (exit code 3 when breached)")]
pub struct SlaArgs {
    /// `none` reports the SLA verdict without failing the command
    #[arg(long, value_enum, value_name = "MODE")]
    pub sla: Option<SlaMode>,
    /// Max share of failed operations, e.g. 1% or 0.01 (default 1%)
    #[arg(long, value_name = "RATE")]
    pub max_error_rate: Option<String>,
    /// Max share of throttled (429/503) operations, e.g. 0% (default: not checked by `run`;
    /// `tune` rejects any throttling)
    #[arg(long, value_name = "RATE")]
    pub max_throttle_rate: Option<String>,
    /// Max p99 latency per operation in ms (tune rejects trials above it)
    #[arg(long, value_name = "MS")]
    pub p99_ms: Option<f64>,
}

#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Output")]
pub struct OutputArgs {
    /// Also save the report as JSON (input for `oprobe compare`)
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
    /// Also save a Markdown report
    #[arg(long, value_name = "FILE")]
    pub markdown: Option<PathBuf>,
    /// Do not save the run in the run store
    #[arg(long)]
    pub no_save: bool,
    /// Also exit with code 3 if the diagnosis has findings of this severity or worse
    /// (SLA breaches always do, unless `--sla none`)
    #[arg(long, value_enum)]
    pub fail_on: Option<FailOn>,
    /// Label stored in the report (e.g. a git SHA)
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub workload: WorkloadArgs,
    #[command(flatten)]
    pub knobs: KnobArgs,
    #[command(flatten)]
    pub cost: CostArgs,
    #[command(flatten)]
    pub sla: SlaArgs,
    #[command(flatten)]
    pub output: OutputArgs,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Repeat the run N times and report the spread
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = at_least::<1>)]
    pub repeat: usize,
    /// Compare against (and add to) a named baseline
    #[arg(long, value_name = "NAME")]
    pub baseline: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ObjectiveArg {
    Throughput,
    Latency,
    Cost,
}

#[derive(Args, Debug)]
pub struct TuneArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub workload: WorkloadArgs,
    #[command(flatten)]
    pub knobs: KnobArgs,
    #[command(flatten)]
    pub cost: CostArgs,
    #[command(flatten)]
    pub sla: SlaArgs,
    #[command(flatten)]
    pub output: OutputArgs,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// What to optimize
    #[arg(long, value_enum, default_value_t = ObjectiveArg::Throughput, help_heading = "Tuning")]
    pub objective: ObjectiveArg,
    /// Measurement window per trial
    #[arg(long, default_value = tune_defaults::TRIAL_DURATION, help_heading = "Tuning")]
    pub trial_duration: String,
    /// Warmup per trial
    #[arg(long, default_value = tune_defaults::TRIAL_WARMUP, help_heading = "Tuning")]
    pub trial_warmup: String,
    /// Maximum number of trials
    #[arg(long, default_value_t = tune_defaults::MAX_TRIALS, value_parser = at_least::<{ tune_defaults::MIN_TRIALS }>, help_heading = "Tuning")]
    pub max_trials: usize,
    /// Highest concurrency to try
    #[arg(long, default_value_t = tune_defaults::MAX_CONCURRENCY, value_parser = at_least::<1>, help_heading = "Tuning")]
    pub max_concurrency: usize,
    /// Pause after a throttled trial
    #[arg(long, default_value = tune_defaults::COOLDOWN, help_heading = "Tuning")]
    pub cooldown: String,
    /// Total wall-time budget of the tuner, e.g. 5m: caps the trials so the estimate fits
    /// (add --max-duration for a hard stop)
    #[arg(long, value_name = "DURATION", help_heading = "Tuning")]
    pub budget: Option<String>,
    /// Write the tuned profile (TOML)
    #[arg(long, value_name = "FILE", help_heading = "Tuning")]
    pub emit_profile: Option<PathBuf>,
    /// Write a Rust/OpenDAL snippet with the tuned settings
    #[arg(long, value_name = "FILE", help_heading = "Tuning")]
    pub emit_rust: Option<PathBuf>,
    /// Write a Python/OpenDAL snippet with the tuned settings
    #[arg(long, value_name = "FILE", help_heading = "Tuning")]
    pub emit_python: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct CompareArgs {
    /// Baseline: RUNREF (id, prefix, last~N, label) or a .oprobe / report .json file
    #[arg(value_name = "A")]
    pub baseline: Option<String>,
    /// Candidate: RUNREF or file (default: `last`)
    #[arg(value_name = "B")]
    pub candidate: Option<String>,
    /// Compare against a named baseline instead of A
    #[arg(long = "baseline", value_name = "NAME")]
    pub baseline_name: Option<String>,
    /// Max p99 increase per operation (%)
    #[arg(long, default_value_t = 20.0)]
    pub max_p99_increase: f64,
    /// Max p50 increase per operation (%)
    #[arg(long, default_value_t = 20.0)]
    pub max_p50_increase: f64,
    /// Ignore latency changes smaller than this (ms)
    #[arg(long, default_value_t = 1.0)]
    pub min_latency_delta_ms: f64,
    /// Max throughput drop (%)
    #[arg(long, default_value_t = 10.0)]
    pub max_throughput_drop: f64,
    /// Max increase of failed operations (percentage points)
    #[arg(long, default_value_t = 0.5)]
    pub max_error_increase: f64,
    /// Max increase of requests per operation (%)
    #[arg(long, default_value_t = 10.0)]
    pub max_request_amp_increase: f64,
    /// Max increase of read amplification (%)
    #[arg(long, default_value_t = 10.0)]
    pub max_read_amp_increase: f64,
    /// Max increase of cost per operation (%)
    #[arg(long, default_value_t = 10.0)]
    pub max_cost_increase: f64,
    /// Minimum samples per operation for latency checks
    #[arg(long, default_value_t = 30)]
    pub min_samples: u64,
    /// Do not fail when the candidate starts getting throttled
    #[arg(long)]
    pub allow_throttling: bool,
    /// Do not fail when the workload differs (preset, mix, dataset, access, rate)
    #[arg(long)]
    pub allow_workload_change: bool,
    /// Do not fail when the target differs (scheme, bucket/prefix, endpoint, options)
    #[arg(long)]
    pub allow_target_change: bool,
    /// Write a Markdown summary (PR comment / job summary)
    #[arg(long, value_name = "FILE")]
    pub markdown: Option<PathBuf>,
    /// Write JUnit XML
    #[arg(long, value_name = "FILE")]
    pub junit: Option<PathBuf>,
    /// Write the comparison as JSON
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// Storage URL, e.g. s3://bucket/prefix, memory://
    pub target: Option<String>,
    /// Custom endpoint (MinIO, R2, oprobe lab)
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Workload preset: parquet | checkpoint | small-files | metadata | mixed
    #[arg(long)]
    pub preset: Option<String>,
    /// Measurement window, e.g. 30s
    #[arg(short, long)]
    pub duration: Option<String>,
    /// Concurrent operations
    #[arg(short, long, value_parser = at_least::<1>)]
    pub concurrency: Option<usize>,
    /// Region of the bucket (e.g. us-east-1)
    #[arg(long)]
    pub region: Option<String>,
    /// Trust an extra PEM CA bundle
    #[arg(long, value_name = "PEM")]
    pub ca_cert: Option<PathBuf>,
    /// Skip TLS certificate verification
    #[arg(long)]
    pub insecure: bool,
    /// Egress proxy URL, or `none` to ignore HTTPS_PROXY/HTTP_PROXY
    #[arg(long)]
    pub proxy: Option<String>,
    /// Public bucket, no credentials
    #[arg(long)]
    pub anonymous: bool,
    /// Open a saved run (.oprobe file) instead of starting with a new run
    #[arg(long, value_name = "FILE")]
    pub open: Option<PathBuf>,
}

#[derive(Args, Debug, Default)]
pub struct CompatArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// `--read-only` refuses (compat writes test objects); budgets stop the suite (exit 4)
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Exit 3 when the score (percent of the checks that ran) is below this
    #[arg(long, value_name = "PCT")]
    pub min_score: Option<f64>,
}

#[derive(Args, Debug, Default)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Only check reachability and listing; never write
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// http(s) endpoint, or a storage URL whose endpoint should be probed
    pub endpoint: String,
    #[arg(short = 'n', long, default_value_t = 5)]
    pub samples: usize,
    #[arg(long, value_enum, default_value_t = HttpArg::Auto)]
    pub http: HttpArg,
    /// Use GET instead of HEAD for the cold/warm requests
    #[arg(long)]
    pub get: bool,
    #[arg(long, default_value = "10s")]
    pub timeout: String,
    #[arg(long, value_name = "PEM")]
    pub ca_cert: Option<PathBuf>,
    #[arg(long)]
    pub insecure: bool,
    #[arg(long)]
    pub proxy: Option<String>,
    /// Also save the result as JSON
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct LabArgs {
    #[arg(long, default_value = "127.0.0.1:9000")]
    pub bind: String,
    /// Start from a fault preset: clean, demo, throttle, stragglers, lossy, flaky (the flags
    /// below override it)
    #[arg(long)]
    pub scenario: Option<String>,
    /// Answer every Nth request with 500 InternalError
    #[arg(long)]
    pub error_every: Option<usize>,
    /// Drop the connection on every Nth request
    #[arg(long)]
    pub reset_every: Option<usize>,
    /// Serve HTTPS (certificate for localhost / 127.0.0.1)
    #[arg(long)]
    pub tls: bool,
    /// Disable HTTP/2
    #[arg(long)]
    pub h1_only: bool,
    /// Added time-to-first-byte for every request (e.g. 20ms)
    #[arg(long, default_value = "0ms")]
    pub latency: String,
    /// Make every Nth request a straggler
    #[arg(long, default_value_t = 0)]
    pub straggler_every: usize,
    /// Extra delay for stragglers
    #[arg(long, default_value = "200ms")]
    pub straggler: String,
    /// Return 503 SlowDown when more than N requests are in flight
    #[arg(long)]
    pub throttle_above: Option<usize>,
    /// Throttle every Nth request
    #[arg(long)]
    pub throttle_every: Option<usize>,
    /// Status code used for throttling (429 or 503)
    #[arg(long, default_value_t = 503)]
    pub throttle_status: u16,
    /// Cap response bandwidth per request (KiB/s)
    #[arg(long)]
    pub bandwidth_kib: Option<u64>,
    /// Answer throttled requests without reading the upload and close the connection (as real
    /// gateways do), so clients see resets mid-upload
    #[arg(long)]
    pub throttle_early_close: bool,
    /// Delay for CompleteMultipartUpload
    #[arg(long, default_value = "0ms")]
    pub complete_delay: String,
    /// Write the lab CA certificate (for --ca-cert) to this file
    #[arg(long, value_name = "FILE")]
    pub write_ca: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct CleanupArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Only remove run prefixes older than this
    #[arg(long, default_value = "1h")]
    pub older_than: String,
    /// Also remove persisted datasets (oprobe-dataset/)
    #[arg(long)]
    pub datasets: bool,
    /// Show what would be deleted
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse to delete anything (implies --dry-run)
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Preset for the template
    #[arg(long, default_value = "parquet")]
    pub preset: String,
    /// Target URL to fill in
    #[arg(long)]
    pub target: Option<String>,
    /// Write to a file instead of stdout
    #[arg(long = "out", value_name = "FILE")]
    pub file: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ExplainArgs {
    /// A RUNREF (id, prefix, `last`, `last~N`, label, .oprobe path) or a finding code
    /// (e.g. `throttling`); `oprobe explain --list` lists the codes
    #[arg(value_name = "RUNREF|FINDING_CODE", required_unless_present = "list")]
    pub subject: Option<String>,
    /// List every finding code
    #[arg(long, conflicts_with = "subject")]
    pub list: bool,
    /// Findings to narrate for a run
    #[arg(long, default_value_t = 3)]
    pub top: usize,
}

#[derive(Args, Debug)]
pub struct SchemaArgs {
    /// Schema name (`report`, `events`, `run`, …); omit to list the names
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct CapabilitiesArgs {}

#[derive(Args, Debug, Default, Clone)]
pub struct McpArgs {
    /// Serve MCP over Streamable HTTP on this address (e.g. 127.0.0.1:8765) instead of stdio
    #[arg(long, value_name = "ADDR")]
    pub http: Option<String>,
    /// Allow --http on a non-loopback address (anyone who can reach it can use the tools and
    /// the configured credentials; there is no authentication)
    #[arg(long, requires = "http")]
    pub http_allow_remote: bool,
    /// Enable tools that write test data (runs that upload a dataset, tune, cleanup)
    #[arg(long)]
    pub allow_write: bool,
    /// Only these targets: a configured target name or a URL prefix (repeatable)
    #[arg(long = "allow-target", value_name = "NAME|URL-PREFIX")]
    pub allow_targets: Vec<String>,
    /// Budget applied to every run the server starts: estimated request cost (USD)
    #[arg(long, value_name = "USD")]
    pub max_cost_usd: Option<f64>,
    /// Budget applied to every run: backend requests
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Budget applied to every run: bytes transferred (e.g. 1GiB)
    #[arg(long, value_name = "SIZE")]
    pub max_bytes: Option<String>,
    /// Budget applied to every run: wall time (e.g. 2m)
    #[arg(long, value_name = "DURATION")]
    pub max_duration: Option<String>,
    /// Runs executing at the same time (others wait)
    #[arg(long, default_value_t = 1, value_parser = at_least::<1>)]
    pub max_concurrent_runs: usize,
}

#[derive(Args, Debug)]
pub struct BaselineSetArgs {
    /// Baseline name (letters, digits, - _ .)
    pub name: String,
    /// Runs: RUNREFs, comma lists, or `all:LABEL`
    #[arg(value_name = "RUNREF")]
    pub runs: Vec<String>,
    /// Use the newest N stored runs
    #[arg(long, value_name = "N")]
    pub last: Option<usize>,
    /// Keep at most this many runs (rolling window; default 10, or the number given)
    #[arg(long, value_name = "N")]
    pub window: Option<usize>,
    /// Add to the baseline's runs instead of replacing them
    #[arg(long)]
    pub append: bool,
}

#[derive(Args, Debug)]
pub struct BaselineNameArg {
    pub name: String,
}

#[derive(Args, Debug)]
pub struct AnalyzeArgs {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe file
    #[arg(value_name = "RUNREF", default_value = "last")]
    pub run: String,
    /// Group requests by: api, conn, remote, stage, second, status
    #[arg(long, value_name = "KEY")]
    pub by: Option<String>,
    /// Only requests started in this window, seconds from the window start (e.g. 5..10)
    #[arg(long, value_name = "A..B")]
    pub window: Option<String>,
    /// Only requests matching all conditions, e.g. api=GetObject,status=503 (repeatable)
    #[arg(long = "where", value_name = "K=V,…")]
    pub r#where: Vec<String>,
    /// Attribute the slowest PCT percent of the (filtered) requests (default 1)
    #[arg(long, value_name = "PCT")]
    pub slowest: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    Json,
    Ndjson,
    Csv,
    Har,
    Otlp,
    Md,
    Html,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe file
    #[arg(value_name = "RUNREF")]
    pub run: String,
    /// Export format
    #[arg(long, short = 'f', value_enum)]
    pub format: ExportFormat,
    /// Write to this file (default: stdout)
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// OTLP/HTTP collector to push to (e.g. http://localhost:4318); with --format otlp
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,
    /// CSV rows: requests or ops
    #[arg(long, default_value = "requests")]
    pub table: String,
    /// OTLP signal: one OTLP/JSON document per signal; `all` needs --out DIR (two files) or
    /// --endpoint (posts /v1/traces and /v1/metrics)
    #[arg(long, value_enum, default_value_t = OtlpSignal::Traces)]
    pub signal: OtlpSignal,
}

/// `runs export --format otlp --signal`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OtlpSignal {
    #[default]
    Traces,
    Metrics,
    All,
}

impl OtlpSignal {
    /// The OTLP signals this selects (`traces`, `metrics`).
    pub fn signals(self) -> &'static [&'static str] {
        match self {
            Self::Traces => &["traces"],
            Self::Metrics => &["metrics"],
            Self::All => &["traces", "metrics"],
        }
    }
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub workload: WorkloadArgs,
    #[command(flatten)]
    pub knobs: KnobArgs,
    #[command(flatten)]
    pub cost: CostArgs,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Time between probe windows (each window lasts --duration, default 5s)
    #[arg(long, default_value = "10s", help_heading = "Watch")]
    pub interval: String,
    /// Rolling window the SLOs are evaluated over
    #[arg(long, default_value = "5m", help_heading = "Watch")]
    pub window: String,
    /// Service level objective, repeatable: p99<200ms, p50<50ms, errors<1%, throttle<1%,
    /// availability>99.9%
    #[arg(long, value_name = "SLO", help_heading = "Watch")]
    pub slo: Vec<String>,
    /// Serve Prometheus metrics on ADDR (/metrics), e.g. 127.0.0.1:9464
    #[arg(long, value_name = "ADDR", help_heading = "Watch")]
    pub listen: Option<String>,
    /// Push OTLP metrics after every window (collector base URL, e.g. http://localhost:4318)
    #[arg(long, value_name = "URL", help_heading = "Watch")]
    pub otlp_endpoint: Option<String>,
    /// Stop after N probe windows (default: until Ctrl-C)
    #[arg(long, value_name = "N", help_heading = "Watch")]
    pub count: Option<u64>,
}

impl WatchArgs {
    /// Probe rate when `--rate` is not given (ops/s): low enough to watch production.
    pub fn default_rate(&self) -> f64 {
        2.0
    }
}

#[derive(Args, Debug)]
pub struct MatrixArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub workload: WorkloadArgs,
    #[command(flatten)]
    pub knobs: KnobArgs,
    #[command(flatten)]
    pub cost: CostArgs,
    #[command(flatten)]
    pub sla: SlaArgs,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Targets to compare (names or URLs, comma-separated or repeated)
    #[arg(long, value_name = "A,B", help_heading = "Matrix")]
    pub targets: Vec<String>,
    /// A knob and its values, repeatable: concurrency=8,16,32 · http=h1,h2 ·
    /// addressing=virtual,path · io_concurrency · chunk_size · range_gap · max_inflight ·
    /// keep_alive=on,off · cache=off,256MiB · retries · tail_cut=on,off · preset
    #[arg(long, value_name = "KNOB=V1,V2", help_heading = "Matrix")]
    pub vary: Vec<String>,
    /// Rank cells by throughput, mib, p50, p99 or cost
    #[arg(long, default_value = "throughput", help_heading = "Matrix")]
    pub rank_by: String,
    /// Label of the stored runs (default matrix-<id>)
    #[arg(long, help_heading = "Matrix")]
    pub label: Option<String>,
}
