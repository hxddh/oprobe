//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::net::HttpVersion;

const EXAMPLES: &str = "\
Examples:
  oprobe run memory://                                  # zero-setup demo
  oprobe run s3://my-bucket/bench --preset parquet -d 30s --json base.json
  oprobe run s3://my-bucket/data --existing tables/events/ -c 64
  oprobe tune s3://my-bucket/bench --emit-profile tuned.toml --emit-rust tuned.rs
  oprobe diff base.json candidate.json --junit gate.xml
  oprobe top s3://my-bucket/bench                       # live terminal workbench (oprobe-app)
  oprobe doctor s3://my-bucket/bench                    # connectivity & permission checks
  oprobe probe https://s3.us-east-1.amazonaws.com       # DNS/TCP/TLS/TTFB microscope
  oprobe lab --tls --latency 20ms --throttle-above 32   # local S3 with faults

Docs: https://github.com/hxddh/oprobe/tree/main/docs";

#[derive(Parser, Debug)]
#[command(
    name = "oprobe",
    version,
    about = "See which layer makes your object-storage I/O slow or expensive — then fix it.",
    long_about = "oprobe drives realistic workloads through Apache OpenDAL and traces every backend request: \
queue wait, DNS/TCP/TLS handshakes, request send, time-to-first-byte, transfer and consumer stall. \
It explains the bottleneck, projects the cloud bill, tunes the client configuration and gates regressions in CI.",
    after_help = EXAMPLES,
    arg_required_else_help = true,
    propagate_version = true
)]
pub struct Cli {
    /// When to use colors.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a workload and report where time and money go
    Run(RunArgs),
    /// Find the best client configuration for a workload (throttle-aware)
    Tune(TuneArgs),
    /// Compare two JSON reports and fail on regressions (CI gate)
    Diff(DiffArgs),
    /// Live terminal workbench: run, watch and adjust a workload interactively
    #[command(visible_alias = "tui")]
    Top(TopArgs),
    /// Check endpoint reachability, TLS, clock skew, credentials and permissions
    Doctor(DoctorArgs),
    /// Measure DNS / TCP / proxy / TLS handshakes and cold vs warm TTFB of an endpoint
    Probe(ProbeArgs),
    /// Start a local S3-compatible server with programmable faults
    #[command(visible_alias = "serve-s3")]
    Lab(LabArgs),
    /// Remove test data left behind by interrupted runs
    Cleanup(CleanupArgs),
    /// Print a commented profile template
    Init(InitArgs),
    /// Re-render a saved JSON report
    Report(ReportArgs),
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Generate the man page
    #[command(hide = true)]
    Man {
        /// Output directory (one page per command)
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
}

/// Where to benchmark.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Target")]
pub struct TargetArgs {
    /// Storage URL: s3://bucket/prefix, gcs://bucket, azblob://container, oss://, cos://, b2://, fs:///path, memory://
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Load a profile (TOML); flags override its values
    #[arg(short, long, value_name = "FILE")]
    pub profile: Option<PathBuf>,
    /// Custom endpoint (MinIO, R2, Ceph, private clouds)
    #[arg(long)]
    pub endpoint: Option<String>,
    #[arg(long)]
    pub region: Option<String>,
    /// Extra backend option, repeatable (e.g. -o enable_virtual_host_style=true)
    #[arg(short = 'o', long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,
    /// Access a public bucket without credentials
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
    #[arg(short = 'c', long)]
    pub concurrency: Option<usize>,
    /// Per-operation reader/writer parallelism
    #[arg(long)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Json,
    Markdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, ValueEnum)]
pub enum FailOn {
    Warning,
    Critical,
}

#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Output")]
pub struct OutputArgs {
    /// Save the full report as JSON (input for `oprobe diff`)
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
    /// Save a Markdown report
    #[arg(long, value_name = "FILE")]
    pub markdown: Option<PathBuf>,
    /// Format printed to stdout
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    /// No live progress on stderr
    #[arg(short, long)]
    pub quiet: bool,
    /// Exit with code 3 if the diagnosis has findings of this severity or worse
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
    pub output: OutputArgs,
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
    pub output: OutputArgs,
    /// What to optimize
    #[arg(long, value_enum, default_value_t = ObjectiveArg::Throughput, help_heading = "Tuning")]
    pub objective: ObjectiveArg,
    /// Measurement window per trial
    #[arg(long, default_value = "5s", help_heading = "Tuning")]
    pub trial_duration: String,
    /// Warmup per trial
    #[arg(long, default_value = "1s", help_heading = "Tuning")]
    pub trial_warmup: String,
    /// Maximum number of trials
    #[arg(long, default_value_t = 24, help_heading = "Tuning")]
    pub max_trials: usize,
    /// Highest concurrency to try
    #[arg(long, default_value_t = 256, help_heading = "Tuning")]
    pub max_concurrency: usize,
    /// Pause after a throttled trial
    #[arg(long, default_value = "5s", help_heading = "Tuning")]
    pub cooldown: String,
    /// p99 latency SLA in ms (trials above it are rejected)
    #[arg(long, help_heading = "Tuning")]
    pub p99_ms: Option<f64>,
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
pub struct DiffArgs {
    /// Baseline report (JSON)
    pub baseline: PathBuf,
    /// Candidate report (JSON)
    pub candidate: PathBuf,
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
    /// Write a Markdown summary (PR comment / job summary)
    #[arg(long, value_name = "FILE")]
    pub markdown: Option<PathBuf>,
    /// Write JUnit XML
    #[arg(long, value_name = "FILE")]
    pub junit: Option<PathBuf>,
    /// Write the diff as JSON
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
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
    #[arg(short, long)]
    pub concurrency: Option<usize>,
    /// Skip TLS certificate verification
    #[arg(long)]
    pub insecure: bool,
    /// Public bucket, no credentials
    #[arg(long)]
    pub anonymous: bool,
}

#[derive(Args, Debug)]
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
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}

#[derive(Args, Debug)]
pub struct LabArgs {
    #[arg(long, default_value = "127.0.0.1:9000")]
    pub bind: String,
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
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    pub file: PathBuf,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
}
