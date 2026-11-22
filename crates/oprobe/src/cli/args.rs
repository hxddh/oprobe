//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::input::clap as parse;
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

/// Defaults of `oprobe tune` (defined with every other default in [`crate::config`]).
pub use crate::config::tune_defaults;

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

/// `--lang`: a language the message catalog has (`en`, `zh-CN`, or `auto`).
fn parse_lang(v: &str) -> Result<String, String> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok("auto".into());
    }
    crate::i18n::Lang::parse(v)
        .map(|l| l.tag().to_owned())
        .ok_or_else(|| format!("unknown language `{v}` (en, zh-CN)"))
}

/// `-o/--output` (the clap side of [`OutputFormat`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    Text,
    Json,
    Ndjson,
    Md,
    Html,
}

impl From<OutputArg> for OutputFormat {
    fn from(o: OutputArg) -> Self {
        match o {
            OutputArg::Text => Self::Text,
            OutputArg::Json => Self::Json,
            OutputArg::Ndjson => Self::Ndjson,
            OutputArg::Md => Self::Md,
            OutputArg::Html => Self::Html,
        }
    }
}

impl Cli {
    /// `-o/--output` (or `OPROBE_OUTPUT`), when given.
    pub fn output_format(&self) -> Option<OutputFormat> {
        self.output.map(Into::into)
    }
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
    pub output: Option<OutputArg>,

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

    /// Language of text output: en, zh-CN (default: LC_ALL / LC_MESSAGES / LANG); JSON stays
    /// English
    #[arg(long, global = true, value_name = "LANG", env = "OPROBE_LANG", value_parser = parse_lang)]
    pub lang: Option<String>,

    /// Print help with every flag (`--help` shows the common ones of large commands)
    #[arg(long = "help-all", global = true)]
    pub help_all: bool,

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
    /// Test a finding's advice: replay the run with and without the change (A B A B…) and
    /// answer confirmed, refuted or inconclusive
    Verify(VerifyArgs),
    /// Diagnose a target, tune it and verify the recommendation; prints a profile patch
    Fix(FixArgs),
    /// Saved runs: list, show, open, export, rm, gc
    #[command(subcommand)]
    Runs(RunsCommand),
    /// Re-slice a run by API, connection, remote, stage or second
    Analyze(AnalyzeArgs),
    /// Narrate a run's causes and advice, or document a finding code
    Explain(ExplainArgs),
    /// Named targets: add, list, show, rm, test, discover (credential sources), sample
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
    /// Serve load for distributed runs (`oprobe run --workers`)
    Worker(WorkerArgs),
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
    /// Generate shell completions (dynamic: target names and run ids complete live)
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
        /// Print a static script (no target or run completion; for shells that cannot call
        /// back into oprobe)
        #[arg(long = "static")]
        static_script: bool,
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
    #[arg(value_name = "RUNREF", add = super::complete::runs(), default_value = "last", value_parser = parse::run_ref)]
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
    /// After the SHA-256 check: verify the build provenance (gh) or the minisign signature
    /// when possible (auto), insist on one of them (require), or skip both (checksum)
    #[arg(long, value_enum, default_value_t = VerifyPolicy::Auto, env = "OPROBE_VERIFY")]
    pub verify: VerifyPolicy,
}

/// `self-update --verify` (and `OPROBE_VERIFY`, shared with install.sh / install.ps1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum VerifyPolicy {
    /// Verify what this machine can; warn when only the checksum could be checked
    #[default]
    Auto,
    /// Refuse to install unless the provenance or the signature verified
    Require,
    /// Only the SHA-256 checksum
    Checksum,
}

#[derive(Args, Debug)]
pub struct RunRefArg {
    /// Run id, id prefix, `last`, `last~N`, label, or path to a .oprobe / report .json file
    #[arg(value_name = "RUNREF", add = super::complete::runs())]
    pub run: String,
}

#[derive(Args, Debug)]
pub struct RunsRmArgs {
    /// Runs to delete (RUNREFs)
    #[arg(value_name = "RUNREF", add = super::complete::runs(), required = true)]
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
    /// Add a named target (`--replace` overwrites one of the same name)
    Add(TargetAddArgs),
    /// List configured targets
    List,
    /// Show one target
    Show(TargetNameArg),
    /// Remove a target
    Rm(TargetNameArg),
    /// Check a target (doctor: reachability, TLS, clock, credentials, permissions)
    Test(TargetTestArgs),
    /// Credential sources on this machine (AWS profiles and environment, GCP ADC, Azure
    /// environment), by name only, with the targets they can reach
    Discover,
    /// What a target holds (read-only): prefixes, object counts, size distribution, examples
    Sample(TargetSampleArgs),
}

#[derive(Args, Debug)]
pub struct TargetSampleArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// List under this prefix (relative to the target root)
    #[arg(long, value_name = "PREFIX")]
    pub prefix: Option<String>,
    /// List at most N objects
    #[arg(long, value_name = "N", default_value_t = crate::app::sample::DEFAULT_LIMIT, value_parser = at_least::<1>)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct TargetAddArgs {
    /// Name, e.g. prod
    pub name: String,
    /// Storage URL, e.g. s3://bucket/prefix
    #[arg(value_parser = parse::storage_url)]
    pub url: String,
    #[arg(long)]
    pub region: Option<String>,
    #[arg(long, value_parser = parse::endpoint)]
    pub endpoint: Option<String>,
    /// env | profile:NAME | anonymous
    #[arg(long)]
    pub credentials: Option<String>,
    /// OpenDAL option, repeatable (KEY=VALUE)
    #[arg(long = "option", value_name = "KEY=VALUE", value_parser = parse::key_value)]
    pub options: Vec<String>,
    /// Save without the quick read-only check
    #[arg(long)]
    pub no_test: bool,
    /// Replace a target of the same name (otherwise an existing name is refused)
    #[arg(long)]
    pub replace: bool,
}

#[derive(Args, Debug)]
pub struct TargetTestArgs {
    /// Target name (or a storage URL)
    #[arg(add = super::complete::targets())]
    pub name: String,
    /// Only check reachability and listing; never write (the default)
    #[arg(long, conflicts_with = "write_probe")]
    pub read_only: bool,
    /// Also write, read back and delete a probe object
    #[arg(long)]
    pub write_probe: bool,
}

#[derive(Args, Debug)]
pub struct TargetNameArg {
    #[arg(add = super::complete::targets())]
    pub name: String,
}

// Parsed once per process.
#[allow(clippy::large_enum_variant)]
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
    /// Learn the target's noise floor from repeated identical runs (<data>/noise.toml)
    Calibrate(CalibrateArgs),
}

/// `oprobe verify RUNREF [CODE|--all] [--repeat N]`.
#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// The run whose findings to test (RUNREF: id, prefix, last, last~N, label)
    #[arg(value_name = "RUNREF", add = super::complete::runs(), value_parser = parse::run_ref)]
    pub run: String,
    /// The finding code to verify (default: every finding with a verification)
    #[arg(value_name = "CODE", conflicts_with = "all")]
    pub code: Option<String>,
    /// Verify every finding that has a verification
    #[arg(long)]
    pub all: bool,
    /// Baseline / candidate pairs to run (interleaved A B A B…)
    #[arg(long, value_name = "N", default_value_t = 3, value_parser = at_least::<1>)]
    pub repeat: usize,
    /// Do not store the outcomes or calibrate the capacity history and noise floor
    #[arg(long)]
    pub no_calibrate: bool,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Do not save the replayed runs in the run store
    #[arg(long)]
    pub no_save: bool,
}

/// `oprobe fix TARGET`: diagnose → tune → verify.
#[derive(Args, Debug)]
pub struct FixArgs {
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
    #[command(flatten)]
    pub search: TuneSearchArgs,
    /// Baseline / candidate pairs of the verification
    #[arg(long, value_name = "N", default_value_t = 2, value_parser = at_least::<1>, help_heading = "Verification")]
    pub repeat: usize,
    /// Write the recommended profile (TOML) to this file
    #[arg(long, value_name = "FILE", help_heading = "Verification")]
    pub write_profile: Option<PathBuf>,
    /// Total wall-time budget of the tuning step, e.g. 5m (caps the trials so the estimate fits)
    #[arg(long, value_name = "DURATION", value_parser = parse::positive_duration, help_heading = "Tuning")]
    pub budget: Option<String>,
    /// Fix the workload of a stored run (RUNREF): its spec, with credentials resolved again
    /// from the target; other flags and --set override it (as `run --from`)
    #[arg(long, value_name = "RUNREF", add = super::complete::runs(), value_parser = parse::run_ref, conflicts_with = "profile", help_heading = "Replay")]
    pub from: Option<String>,
    /// Override one setting of the workload, repeatable (as `run --set`)
    #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse::set, help_heading = "Replay")]
    pub set: Vec<String>,
}

/// `oprobe baseline calibrate TARGET`.
#[derive(Args, Debug)]
pub struct CalibrateArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub workload: WorkloadArgs,
    #[command(flatten)]
    pub knobs: KnobArgs,
    #[command(flatten)]
    pub safety: SafetyArgs,
    /// Identical runs to measure (N − 1 A/A pairs)
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = at_least::<2>)]
    pub repeat: usize,
    /// Calibrate the workload of a stored run instead (RUNREF; its spec is replayed)
    #[arg(long, value_name = "RUNREF", add = super::complete::runs(), value_parser = parse::run_ref)]
    pub from: Option<String>,
    /// Do not save the calibration runs in the run store
    #[arg(long)]
    pub no_save: bool,
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
    #[arg(long, value_name = "USD", value_parser = parse::non_negative, allow_hyphen_values = true)]
    pub max_cost_usd: Option<f64>,
    /// Stop after this many backend requests (exit 4)
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Stop after transferring this many bytes, e.g. 10GiB (exit 4)
    #[arg(long, value_name = "SIZE", value_parser = parse::size, allow_hyphen_values = true)]
    pub max_bytes: Option<String>,
    /// Stop after this much wall time, e.g. 5m (exit 4)
    #[arg(long, value_name = "DURATION", value_parser = parse::duration, allow_hyphen_values = true)]
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
    #[arg(value_name = "TARGET", add = super::complete::targets())]
    pub target: Option<String>,
    /// Load a profile (TOML); flags override its values
    #[arg(short, long, value_name = "FILE")]
    pub profile: Option<PathBuf>,
    /// Custom endpoint (MinIO, R2, Ceph, private clouds)
    #[arg(long, value_parser = parse::endpoint)]
    pub endpoint: Option<String>,
    /// Region of the bucket (e.g. us-east-1); most S3-compatible services accept any value
    #[arg(long)]
    pub region: Option<String>,
    /// Extra backend option, repeatable (e.g. --option enable_virtual_host_style=true)
    #[arg(long = "option", value_name = "KEY=VALUE", value_parser = parse::key_value)]
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
    #[arg(long, value_parser = parse::preset)]
    pub preset: Option<String>,
    /// Measurement window (e.g. 30s, 2m)
    #[arg(short, long, value_parser = parse::duration, allow_hyphen_values = true)]
    pub duration: Option<String>,
    /// Warmup before measuring (connections, caches)
    #[arg(long, value_parser = parse::duration, allow_hyphen_values = true)]
    pub warmup: Option<String>,
    /// Stop after N measured operations
    #[arg(long)]
    pub max_ops: Option<u64>,
    /// Open-loop arrival rate (ops/s); latency is measured from the intended start time
    #[arg(long, value_parser = parse::positive, allow_hyphen_values = true)]
    pub rate: Option<f64>,
    /// Benchmark existing objects under this prefix (read-only)
    #[arg(long, value_name = "PREFIX")]
    pub existing: Option<String>,
    /// Number of dataset objects to generate
    #[arg(long)]
    pub objects: Option<usize>,
    /// Size of each generated object (e.g. 16MiB)
    #[arg(long, value_parser = parse::nonzero_size, allow_hyphen_values = true)]
    pub object_size: Option<String>,
    /// Key popularity: uniform, zipf:1.1, hotset:0.2/0.8
    #[arg(long, value_parser = parse::access)]
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
    #[arg(long, value_parser = parse::size, allow_hyphen_values = true)]
    pub chunk_size: Option<String>,
    /// Merge vectored ranges closer than this (e.g. 64KiB)
    #[arg(long, value_parser = parse::size, allow_hyphen_values = true)]
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
    #[arg(long, value_name = "SIZE", value_parser = parse::nonzero_size)]
    pub cache: Option<String>,
    /// Retry attempts for temporary errors
    #[arg(long)]
    pub retries: Option<usize>,
    /// Cancel and retry straggler requests (TailCutLayer)
    #[arg(long)]
    pub tail_cut: bool,
    /// S3 bucket addressing: auto (as configured), virtual (bucket.host) or path (host/bucket)
    #[arg(long, value_name = "STYLE", value_parser = parse::addressing)]
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
    /// (aliases: aws, gcp, google, azure, r2, b2, oss, aliyun)
    #[arg(long, value_parser = parse::provider)]
    pub provider: Option<String>,
    /// Logical operations per month to project costs for
    #[arg(long, value_parser = parse::non_negative, allow_hyphen_values = true)]
    pub monthly_ops: Option<f64>,
    /// Fraction of downloads leaving the cloud (0..1)
    #[arg(long, value_parser = parse::fraction, allow_hyphen_values = true)]
    pub egress_fraction: Option<f64>,
    /// Stored data in GiB
    #[arg(long, value_parser = parse::non_negative, allow_hyphen_values = true)]
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
    #[arg(long = "max-error-ratio", alias = "max-error-rate", value_name = "RATIO", value_parser = parse::share, allow_hyphen_values = true)]
    pub max_error_rate: Option<String>,
    /// Max share of throttled (429/503) requests, e.g. 0% or 0.001: an SLA of `run` (default:
    /// not checked) and the throttle budget of `tune` and `fix` (default 0.1%)
    #[arg(long = "max-throttle-ratio", alias = "max-throttle-rate", value_name = "RATIO", value_parser = parse::share, allow_hyphen_values = true)]
    pub max_throttle_rate: Option<String>,
    /// Max p99 latency per operation in ms (tune rejects trials above it)
    #[arg(long, value_name = "MS", value_parser = parse::positive, allow_hyphen_values = true)]
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

#[derive(Args, Debug, Default)]
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
    /// Replay a stored run (RUNREF): its spec, with credentials resolved again from the
    /// target; other flags and --set override it
    #[arg(long, value_name = "RUNREF", add = super::complete::runs(), value_parser = parse::run_ref, conflicts_with = "profile", help_heading = "Replay")]
    pub from: Option<String>,
    /// Override one setting, repeatable (e.g. --set concurrency=8 --set warmup=1s); keys:
    /// the profile's knob, workload, SLA and cost names, `endpoint`, `region`, `label`
    #[arg(long = "set", value_name = "KEY=VALUE", value_parser = parse::set, help_heading = "Replay")]
    pub set: Vec<String>,
    #[command(flatten)]
    pub load: LoadArgs,
}

/// What generates the load, and where (design v5 §7, §8).
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Load generation")]
pub struct LoadArgs {
    /// What drives the workload: opendal (default), aws-sdk, or exec:COMMAND (an adapter
    /// speaking the oprobe client protocol, see docs/clients.md). Not stored in the run's spec
    #[arg(long, value_name = "CLIENT")]
    pub client: Option<String>,
    /// Distribute the run over workers: host:port[*WEIGHT],… of `oprobe worker`, local:N to
    /// start N worker processes here, or config for the config file's [workers]
    #[arg(long, value_name = "WORKERS")]
    pub workers: Option<String>,
    /// The token file the remote workers were started with (default: [workers].token_file,
    /// then $OPROBE_WORKER_TOKEN)
    #[arg(long, value_name = "FILE", requires = "workers")]
    pub worker_token_file: Option<std::path::PathBuf>,
}

/// `oprobe worker`: serve load for distributed runs.
#[derive(Args, Debug, Clone)]
pub struct WorkerArgs {
    /// Address to listen on (host:port; port 0 picks a free one)
    #[arg(long, value_name = "ADDR")]
    pub listen: String,
    /// File holding the shared secret every coordinator request must carry
    #[arg(long, value_name = "FILE")]
    pub token_file: std::path::PathBuf,
    /// Serve TLS with this PEM certificate (needs --tls-key)
    #[arg(long, value_name = "FILE", requires = "tls_key")]
    pub tls_cert: Option<std::path::PathBuf>,
    /// PEM private key of --tls-cert
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    pub tls_key: Option<std::path::PathBuf>,
    /// Let coordinators run `--client exec:COMMAND` on this machine
    #[arg(long)]
    pub allow_exec: bool,
    /// Exit when stdin closes (local workers started by `--workers local:N`)
    #[arg(long, hide = true)]
    pub exit_with_stdin: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ObjectiveArg {
    /// Successful ops/s (or MiB/s) within the throttle budget (`throughput` is an alias)
    #[value(alias = "throughput")]
    Goodput,
    /// Lowest worst-case p99 at ≥ 80 % of the best goodput (`latency` is an alias)
    #[value(alias = "latency")]
    P99,
    /// Lowest cost per operation at ≥ 80 % of the best goodput
    Cost,
    /// Best goodput-to-p99 trade at ≥ 80 % of the best goodput
    Balanced,
}

impl ObjectiveArg {
    pub fn objective(self) -> crate::tune::Objective {
        use crate::tune::Objective;
        match self {
            ObjectiveArg::Goodput => Objective::Throughput,
            ObjectiveArg::P99 => Objective::Latency,
            ObjectiveArg::Cost => Objective::Cost,
            ObjectiveArg::Balanced => Objective::Balanced,
        }
    }
}

/// The tuner's search flags (shared by `tune` and `fix`).
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Tuning")]
pub struct TuneSearchArgs {
    /// What to optimize
    #[arg(long, value_enum, default_value_t = ObjectiveArg::Goodput)]
    pub objective: ObjectiveArg,
    /// Measurement window per trial
    #[arg(long, default_value = tune_defaults::TRIAL_DURATION, value_parser = parse::positive_duration)]
    pub trial_duration: String,
    /// Warmup per trial
    #[arg(long, default_value = tune_defaults::TRIAL_WARMUP, value_parser = parse::duration)]
    pub trial_warmup: String,
    /// Maximum number of trials
    #[arg(long, default_value_t = tune_defaults::MAX_TRIALS, value_parser = at_least::<{ tune_defaults::MIN_TRIALS }>)]
    pub max_trials: usize,
    /// Highest concurrency to try
    #[arg(long, default_value_t = tune_defaults::MAX_CONCURRENCY, value_parser = at_least::<1>)]
    pub max_concurrency: usize,
    /// Pause after a throttled trial
    #[arg(long, default_value = tune_defaults::COOLDOWN, value_parser = parse::duration)]
    pub cooldown: String,
    /// Runs of each bisection trial near the throttle boundary
    #[arg(long, default_value_t = tune_defaults::BOUNDARY_REPEATS, value_parser = at_least::<1>)]
    pub boundary_repeats: usize,
    /// In-flight headroom kept below the throttling ceiling (default 0.15)
    #[arg(long, value_name = "RATIO", value_parser = parse::fraction)]
    pub headroom_ratio: Option<f64>,
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
    #[command(flatten)]
    pub search: TuneSearchArgs,
    /// Total wall-time budget of the tuner, e.g. 5m: caps the trials so the estimate fits
    /// (add --max-duration for a hard stop)
    #[arg(long, value_name = "DURATION", value_parser = parse::positive_duration, help_heading = "Tuning")]
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
    #[arg(value_name = "A", add = super::complete::runs())]
    pub baseline: Option<String>,
    /// Candidate: RUNREF or file (default: `last`)
    #[arg(value_name = "B", add = super::complete::runs())]
    pub candidate: Option<String>,
    /// Compare against a named baseline instead of A
    #[arg(long = "baseline", value_name = "NAME")]
    pub baseline_name: Option<String>,
    /// Max p99 increase per operation (%)
    #[arg(
        long = "max-p99-increase-pct",
        alias = "max-p99-increase",
        value_name = "PCT",
        default_value_t = 20.0
    )]
    pub max_p99_increase: f64,
    /// Max p50 increase per operation (%)
    #[arg(
        long = "max-p50-increase-pct",
        alias = "max-p50-increase",
        value_name = "PCT",
        default_value_t = 20.0
    )]
    pub max_p50_increase: f64,
    /// Ignore latency changes smaller than this (ms)
    #[arg(long, value_name = "MS", default_value_t = 1.0)]
    pub min_latency_delta_ms: f64,
    /// Max throughput drop (%)
    #[arg(
        long = "max-throughput-drop-pct",
        alias = "max-throughput-drop",
        value_name = "PCT",
        default_value_t = 10.0
    )]
    pub max_throughput_drop: f64,
    /// Max increase of failed operations (percentage points)
    #[arg(
        long = "max-error-increase-pp",
        alias = "max-error-increase",
        value_name = "PP",
        default_value_t = 0.5
    )]
    pub max_error_increase: f64,
    /// Max increase of requests per operation (%)
    #[arg(
        long = "max-request-amp-increase-pct",
        alias = "max-request-amp-increase",
        value_name = "PCT",
        default_value_t = 10.0
    )]
    pub max_request_amp_increase: f64,
    /// Max increase of read amplification (%)
    #[arg(
        long = "max-read-amp-increase-pct",
        alias = "max-read-amp-increase",
        value_name = "PCT",
        default_value_t = 10.0
    )]
    pub max_read_amp_increase: f64,
    /// Max increase of cost per operation (%)
    #[arg(
        long = "max-cost-increase-pct",
        alias = "max-cost-increase",
        value_name = "PCT",
        default_value_t = 10.0
    )]
    pub max_cost_increase: f64,
    /// Minimum samples per operation for latency checks
    #[arg(long, value_name = "N", default_value_t = 30)]
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
    /// Runs of different targets or workloads are an error (exit 2) instead of a warning
    #[arg(long)]
    pub strict: bool,
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
    #[arg(add = super::complete::targets())]
    pub target: Option<String>,
    /// Custom endpoint (MinIO, R2, oprobe lab)
    #[arg(long, value_parser = parse::endpoint)]
    pub endpoint: Option<String>,
    /// Workload preset: parquet | checkpoint | small-files | metadata | mixed
    #[arg(long, value_parser = parse::preset)]
    pub preset: Option<String>,
    /// Measurement window, e.g. 30s
    #[arg(short, long, value_parser = parse::duration, allow_hyphen_values = true)]
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
    #[arg(long = "min-score-pct", alias = "min-score", value_name = "PCT")]
    pub min_score: Option<f64>,
}

#[derive(Args, Debug, Default)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Only check reachability and listing; never write (the default; kept for scripts)
    #[arg(long, conflicts_with = "write_probe")]
    pub read_only: bool,
    /// Also write, read back, stat and delete a 256 KiB probe object under
    /// `oprobe-run-<time>-doctor/` (the default is read-only)
    #[arg(long)]
    pub write_probe: bool,
    /// Show the raw backend error of failed checks
    #[arg(short, long)]
    pub verbose: bool,
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
    #[arg(long, default_value = "10s", value_parser = parse::positive_duration)]
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
    /// Start from a fault preset: clean, demo, throttle, stragglers, lossy, flaky, parquet (the
    /// flags below override it)
    #[arg(long, value_parser = parse::lab_scenario)]
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
    #[arg(long, default_value = "0ms", value_parser = parse::duration, allow_hyphen_values = true)]
    pub latency: String,
    /// Make every Nth request a straggler
    #[arg(long, default_value_t = 0)]
    pub straggler_every: usize,
    /// Extra delay for stragglers
    #[arg(long, default_value = "200ms", value_parser = parse::duration, allow_hyphen_values = true)]
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
    #[arg(long, default_value = "0ms", value_parser = parse::duration, allow_hyphen_values = true)]
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
    #[arg(long, default_value = "1h", value_parser = parse::duration)]
    pub older_than: String,
    /// Also remove persisted datasets (oprobe-dataset/)
    #[arg(long)]
    pub datasets: bool,
    /// Show what would be deleted (the default without --yes)
    #[arg(long, conflicts_with = "yes")]
    pub dry_run: bool,
    /// Refuse to delete anything (implies --dry-run)
    #[arg(long, conflicts_with = "yes")]
    pub read_only: bool,
    /// Delete what was found (without it, cleanup only lists it)
    #[arg(long, short = 'y')]
    pub yes: bool,
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
    #[arg(value_name = "RUNREF|FINDING_CODE", required_unless_present = "list", add = super::complete::explain())]
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
    #[arg(long, value_name = "USD", value_parser = parse::non_negative, allow_hyphen_values = true)]
    pub max_cost_usd: Option<f64>,
    /// Budget applied to every run: backend requests
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Budget applied to every run: bytes transferred (e.g. 1GiB)
    #[arg(long, value_name = "SIZE", value_parser = parse::size)]
    pub max_bytes: Option<String>,
    /// Budget applied to every run: wall time (e.g. 2m)
    #[arg(long, value_name = "DURATION", value_parser = parse::duration)]
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
    #[arg(value_name = "RUNREF", add = super::complete::runs())]
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
    #[arg(value_name = "RUNREF", add = super::complete::runs(), default_value = "last")]
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
    #[arg(value_name = "RUNREF", add = super::complete::runs())]
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
    #[arg(long, default_value = "10s", value_parser = parse::positive_duration, help_heading = "Watch")]
    pub interval: String,
    /// Rolling window the SLOs are evaluated over
    #[arg(long, default_value = "5m", value_parser = parse::positive_duration, help_heading = "Watch")]
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("oprobe").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    #[test]
    fn the_command_tree_is_valid() {
        // clap's own consistency checks: ids, conflicts, requirements, value names.
        Cli::command().debug_assert();
    }

    #[test]
    fn one_throttle_flag_and_unit_suffixed_names() {
        let Some(Command::Tune(t)) =
            parse(&["tune", "memory://", "--max-throttle-ratio", "0.5%"]).command
        else {
            panic!()
        };
        assert_eq!(t.sla.max_throttle_rate.as_deref(), Some("0.5%"));
        // The 4.x spellings still parse (hidden aliases).
        let Some(Command::Run(r)) = parse(&[
            "run",
            "memory://",
            "--max-throttle-rate",
            "1%",
            "--max-error-rate",
            "2%",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!(r.sla.max_throttle_rate.as_deref(), Some("1%"));
        assert_eq!(r.sla.max_error_rate.as_deref(), Some("2%"));
        let Some(Command::Compare(c)) = parse(&[
            "compare",
            "a",
            "b",
            "--max-p99-increase-pct",
            "30",
            "--max-error-increase",
            "1",
        ])
        .command
        else {
            panic!()
        };
        assert_eq!((c.max_p99_increase, c.max_error_increase), (30.0, 1.0));
        // No flag spells a unit-bearing quantity without its unit.
        let help = Cli::command()
            .find_subcommand("compare")
            .unwrap()
            .clone()
            .render_long_help()
            .to_string();
        assert!(help.contains("--max-p99-increase-pct") && !help.contains("--max-p99-increase "));
    }

    #[test]
    fn global_flags_parse_anywhere() {
        let cli = parse(&["runs", "list", "--lang", "zh-CN", "-o", "json"]);
        assert_eq!(cli.lang.as_deref(), Some("zh-CN"));
        assert!(Cli::try_parse_from(["oprobe", "--lang", "klingon", "runs", "list"]).is_err());
        assert_eq!(
            parse(&["--lang", "auto", "runs", "list"]).lang.as_deref(),
            Some("auto")
        );
    }

    #[test]
    fn self_update_verification_policy() {
        let Some(Command::SelfUpdate(a)) = parse(&["self-update", "--check"]).command else {
            panic!()
        };
        assert_eq!(a.verify, VerifyPolicy::Auto);
        let Some(Command::SelfUpdate(a)) = parse(&["self-update", "--verify", "require"]).command
        else {
            panic!()
        };
        assert_eq!(a.verify, VerifyPolicy::Require);
    }

    #[test]
    fn completions_default_to_dynamic() {
        let Some(Command::Completions { static_script, .. }) =
            parse(&["completions", "zsh"]).command
        else {
            panic!()
        };
        assert!(!static_script);
        let Some(Command::Completions { static_script, .. }) =
            parse(&["completions", "bash", "--static"]).command
        else {
            panic!()
        };
        assert!(static_script);
    }
}
