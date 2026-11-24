//! The command line: four commands and the bare app (design v6 §8).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::input::clap as parse;
pub use crate::output::OutputFormat;

const EXAMPLES: &str = concat!(
    "\
Examples:
  oprobe                                       # open the app (no command)
  oprobe demo                                  # profile the local lab (no credentials)
  oprobe run s3://my-bucket/bench              # profile an operator
  oprobe run op.toml --set layers.retry.max_times=5
  oprobe run s3://a/x r2://b/x --preset range-reads   # several operators, one workload
  oprobe show --list                           # stored runs
  oprobe show last --by stage                  # where the time went
  oprobe prove last                            # A/B-test the remedy of the top finding
  oprobe lab throttle                          # local S3 with faults

Docs: https://github.com/hxddh/oprobe/tree/v",
    env!("CARGO_PKG_VERSION"),
    "/docs"
);

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

/// `-o/--output` (the clap side of [`OutputFormat`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputArg {
    Text,
    Json,
}

impl From<OutputArg> for OutputFormat {
    fn from(o: OutputArg) -> Self {
        match o {
            OutputArg::Text => Self::Text,
            OutputArg::Json => Self::Json,
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
    about = "A profiler for OpenDAL operators.",
    long_about = "oprobe shows what your OpenDAL `Operator` really does on the wire, names the layer \
that fixes it, and proves the fix with an interleaved A/B test before you ship it.",
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

    /// When to use colors
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    /// No progress or informational messages on stderr
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Never prompt or redraw (implied when stdin/stdout is not a terminal)
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// Config file (default: $OPROBE_CONFIG or <config dir>/oprobe/config.toml)
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    pub config: Option<PathBuf>,

    /// Without a command, oprobe opens its app.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Profile one or more operators on the same workload
    Run(RunArgs),
    /// Profile the local lab (`oprobe run lab://demo`)
    Demo(DemoArgs),
    /// A/B-test a change of a run's operator: the proven layer diff
    Prove(ProveArgs),
    /// Show a stored run (or a run file); `--list` for the history
    Show(ShowArgs),
    /// Serve a local S3-compatible lab with faults
    Lab(LabArgs),
}

/// The workload (design v6 §8).
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Workload")]
pub struct WorkloadArgs {
    /// Workload preset: small-files, large-objects, range-reads, list-stat
    #[arg(long, value_parser = parse::preset)]
    pub preset: Option<String>,
    /// Concurrent operations
    #[arg(short = 'c', long, value_parser = at_least::<1>)]
    pub concurrency: Option<usize>,
    /// Measurement window (e.g. 30s, 2m)
    #[arg(short, long, value_parser = parse::positive_duration, allow_hyphen_values = true)]
    pub duration: Option<String>,
    /// Number of dataset objects to generate
    #[arg(long)]
    pub objects: Option<usize>,
    /// Size of each generated object (e.g. 16MiB)
    #[arg(long, value_parser = parse::object_size, allow_hyphen_values = true)]
    pub object_size: Option<String>,
    /// Open-loop arrival rate (ops/s); latency is measured from the intended start time
    #[arg(long, value_parser = parse::positive, allow_hyphen_values = true)]
    pub rate: Option<f64>,
    /// Profile existing objects under this prefix (read-only)
    #[arg(long, value_name = "PREFIX")]
    pub existing: Option<String>,
    /// Seed of the workload's random choices
    #[arg(long)]
    pub seed: Option<u64>,
    /// Warmup before measuring
    #[arg(long, value_parser = parse::duration, allow_hyphen_values = true, hide = true)]
    pub warmup: Option<String>,
}

/// Safety rails of a run.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Safety")]
pub struct SafetyArgs {
    /// Print the plan (requests, bytes, objects written and deleted, ETA) and exit
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse every write and delete (exit 5); implied by --existing
    #[arg(long)]
    pub read_only: bool,
    /// Stop after this many backend requests (exit 4)
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Stop after transferring this many bytes, e.g. 10GiB (exit 4)
    #[arg(long, value_name = "SIZE", value_parser = parse::size, allow_hyphen_values = true)]
    pub max_bytes: Option<String>,
    /// Stop after this much wall time, e.g. 5m (exit 4)
    #[arg(long, value_name = "DURATION", value_parser = parse::positive_duration, allow_hyphen_values = true)]
    pub max_duration: Option<String>,
}

/// What `run` and `demo` share.
#[derive(Args, Debug, Clone, Default)]
pub struct RunFlags {
    #[command(flatten)]
    pub workload: WorkloadArgs,
    /// Change one value of the operator (or the workload), repeatable: e.g.
    /// --set layers.retry.max_times=5, --set read.gap=1MiB, --set layers.cache=off
    #[arg(long = "set", value_name = "PATH=VALUE", value_parser = parse::key_value)]
    pub set: Vec<String>,
    #[command(flatten)]
    pub safety: SafetyArgs,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The operators: a URL (s3://bucket/prefix, lab://demo, fs:///tmp/x, memory://), an
    /// operator file (op.toml) or a name from the config file's [operators.NAME]
    #[arg(value_name = "OPERATOR", required = true)]
    pub operators: Vec<String>,
    #[command(flatten)]
    pub flags: RunFlags,
}

#[derive(Args, Debug)]
pub struct DemoArgs {
    #[command(flatten)]
    pub flags: RunFlags,
}

/// `--objective`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ObjectiveArg {
    /// Successful operations (or bytes) per second
    #[default]
    Goodput,
    /// The worst per-operation p99 latency
    P99,
}

#[derive(Args, Debug)]
pub struct ProveArgs {
    /// The run whose operator to change (id, id prefix, `last`, `last~N`, or a .oprobe file)
    #[arg(value_name = "RUN")]
    pub run: String,
    /// The finding whose remedy to prove (default: the top finding)
    #[arg(value_name = "FINDING")]
    pub finding: Option<String>,
    /// The change to prove instead of the finding's remedy, repeatable: PATH=VALUE
    #[arg(long = "with", value_name = "PATH=VALUE", value_parser = parse::key_value)]
    pub with: Vec<String>,
    /// What the change is judged on
    #[arg(long, value_enum, default_value_t = ObjectiveArg::Goodput)]
    pub objective: ObjectiveArg,
    /// Share of throttled requests a sized value may cause, e.g. 0.1% or 0.001 (default 0.1%)
    #[arg(long, value_name = "RATIO", value_parser = parse::share, allow_hyphen_values = true)]
    pub max_throttle: Option<String>,
    /// Measurement window of one trial (default 2s)
    #[arg(long, value_name = "DURATION", value_parser = parse::positive_duration, allow_hyphen_values = true, hide = true)]
    pub trial: Option<String>,
    /// A/B pairs at most (default 6; at least 3, the fewest that can decide)
    #[arg(long, value_name = "N", value_parser = at_least::<3>, hide = true)]
    pub pairs: Option<usize>,
    /// Save the proven operator to an op.toml file or [operators.NAME] when improved
    #[arg(long, value_name = "FILE|OPERATOR")]
    pub save: Option<String>,
    #[command(flatten)]
    pub safety: ProveSafetyArgs,
}

/// Safety rails of a prove: one wall-time budget for the whole proof.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Safety")]
pub struct ProveSafetyArgs {
    /// Print the plan of the whole proof (trials, requests, ETA) and exit
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse every write and delete (exit 5)
    #[arg(long)]
    pub read_only: bool,
    /// Stop after this many backend requests (exit 4)
    #[arg(long, value_name = "N")]
    pub max_requests: Option<u64>,
    /// Stop after transferring this many bytes, e.g. 10GiB (exit 4)
    #[arg(long, value_name = "SIZE", value_parser = parse::size, allow_hyphen_values = true)]
    pub max_bytes: Option<String>,
    /// Wall-time budget of the whole proof, e.g. 5m: no trial starts that would not end in it
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = parse::positive_duration,
        allow_hyphen_values = true
    )]
    pub max_duration: Option<String>,
    /// `--budget`: the 6.0–6.2 spelling of `--max-duration`, hidden; with both, the shorter
    /// bounds the proof.
    #[arg(
        long,
        hide = true,
        value_name = "DURATION",
        value_parser = parse::positive_duration,
        allow_hyphen_values = true
    )]
    pub budget: Option<String>,
}

impl ProveSafetyArgs {
    /// The one wall-time budget: `--max-duration`, `--budget`, or the shorter of the two.
    pub fn wall_budget(&self) -> Option<String> {
        let secs = |d: &String| {
            crate::util::parse_duration(d)
                .map(|d| d.as_secs_f64())
                .unwrap_or(f64::INFINITY)
        };
        match (&self.max_duration, &self.budget) {
            (Some(a), Some(b)) => Some(if secs(b) < secs(a) { b } else { a }.clone()),
            (a, b) => a.clone().or_else(|| b.clone()),
        }
    }
}

/// `--by`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ByArg {
    Stage,
    Api,
    Conn,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Stored run(s), file(s) or a prove id; pass two or more runs to compare side by side
    #[arg(value_name = "RUN|FILE")]
    pub runs: Vec<String>,
    /// List the stored runs (prove trials are hidden; --all lists them)
    #[arg(long, conflicts_with_all = ["rm", "detail", "by", "runs"])]
    pub list: bool,
    /// With --list: also the trial runs of every prove
    #[arg(long, requires = "list")]
    pub all: bool,
    /// Delete stored runs
    #[arg(long, value_name = "RUN", num_args = 1.., conflicts_with_all = ["detail", "by", "runs"])]
    pub rm: Vec<String>,
    /// Every section of the profile (or per-API breakdown when comparing runs)
    #[arg(long)]
    pub detail: bool,
    /// Group the requests: by stage, API or connection
    #[arg(long, value_enum)]
    pub by: Option<ByArg>,
    /// Only the time range from this far into the window (e.g. 3s): its numbers, requests by
    /// API, verbs and slowest operations, recomputed from the trace rows
    #[arg(long, value_name = "DURATION", value_parser = parse::duration, conflicts_with_all = ["list", "rm"])]
    pub since: Option<String>,
    /// Only the time range up to this far into the window (e.g. 8s; default: the window's end)
    #[arg(long, value_name = "DURATION", value_parser = parse::duration, conflicts_with_all = ["list", "rm"])]
    pub until: Option<String>,
    /// Show at most N runs (`--list`)
    #[arg(short = 'n', long, default_value_t = 20, hide = true)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct LabArgs {
    /// The faults: demo, throttle, stragglers, bandwidth, flaky or ratelimit (default: none)
    #[arg(value_name = "SCENARIO", value_parser = parse::lab_scenario)]
    pub scenario: Option<String>,
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1:9000", value_name = "ADDR")]
    pub bind: String,
    /// Allow binding to a non-loopback address (unsafe on untrusted networks)
    #[arg(long)]
    pub allow_remote: bool,
    /// Cap total in-memory object bytes stored by the lab (default 512MiB)
    #[arg(long, value_name = "SIZE", value_parser = parse::nonzero_size)]
    pub max_store_size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::ProveSafetyArgs;

    #[test]
    fn the_old_budget_spelling_still_bounds_the_proof() {
        let s = |max: Option<&str>, budget: Option<&str>| ProveSafetyArgs {
            max_duration: max.map(str::to_owned),
            budget: budget.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(s(Some("5m"), None).wall_budget().as_deref(), Some("5m"));
        assert_eq!(s(None, Some("2m")).wall_budget().as_deref(), Some("2m"));
        // Both (a 6.2 script that passed both): the shorter bounds it, no error.
        assert_eq!(
            s(Some("5m"), Some("90s")).wall_budget().as_deref(),
            Some("90s")
        );
        assert_eq!(s(None, None).wall_budget(), None);
    }
}
