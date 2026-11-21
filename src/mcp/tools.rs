//! MCP tools: definitions (input/output JSON Schemas, annotations) and handlers.
//!
//! Every tool returns `structuredContent` matching its `outputSchema` plus a short text
//! summary. Failures are tool errors (`isError: true`) whose text carries the classified
//! [`ErrorBody`] as JSON. Order of [`SPECS`] is the `tools/list` order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::McpConfig;
use super::explain::{self, ExplainRun};
use super::jobs::{Job, JobKind, JobProgress, JobSpec, JobStatus, Jobs};
use super::schemas::{Direction, schema_of};
use crate::analysis::outliers::{self, Attribution, Selection};
use crate::cli::Ctx;
use crate::cli::args::{CostArgs, HttpArg, KnobArgs, TargetArgs, WorkloadArgs};
use crate::config::Profile;
use crate::errors::{ErrorBody, ErrorCode, OprobeError};
use crate::events::EventKind;
use crate::guard::{Budgets, Plan};
use crate::model::{Report, RunId};
use crate::output::Output;
use crate::report::diff::Thresholds;
use crate::store::{RunSummary, TargetId};
use crate::util::fmt_ms;

// ---------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------

/// Where to point a tool.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct TargetIn {
    /// Configured target name (`list_targets`) or storage URL: `s3://bucket/prefix`,
    /// `gcs://bucket`, `azblob://container`, `fs:///path`, `memory://`.
    pub target: String,
    /// Custom endpoint (MinIO, R2, Ceph, an `oprobe lab` URL).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bucket region, e.g. `us-east-1`.
    #[serde(default)]
    pub region: Option<String>,
    /// Extra OpenDAL backend options (key → value).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Unsigned requests (public buckets).
    #[serde(default)]
    pub anonymous: bool,
    /// Skip TLS certificate verification (development only).
    #[serde(default)]
    pub insecure: bool,
    /// Egress proxy URL, or `none` to ignore HTTPS_PROXY/HTTP_PROXY.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// What to run and with which client settings (unset: the target's defaults).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct WorkloadIn {
    /// `parquet`, `checkpoint`, `small-files`, `metadata` or `mixed`.
    #[serde(default)]
    pub preset: Option<String>,
    /// Measurement window, e.g. `10s`.
    #[serde(default)]
    pub duration: Option<String>,
    /// Warmup before measuring, e.g. `2s`.
    #[serde(default)]
    pub warmup: Option<String>,
    /// Stop after this many measured operations.
    #[serde(default)]
    pub max_ops: Option<u64>,
    /// Open-loop arrival rate (operations per second).
    #[serde(default)]
    pub rate: Option<f64>,
    /// Benchmark existing objects under this prefix (read-only: nothing is written).
    #[serde(default)]
    pub existing: Option<String>,
    /// Dataset objects to generate.
    #[serde(default)]
    pub objects: Option<usize>,
    /// Size of each generated object, e.g. `16MiB`.
    #[serde(default)]
    pub object_size: Option<String>,
    /// Key popularity: `uniform`, `zipf:1.1`, `hotset:0.2/0.8`.
    #[serde(default)]
    pub access: Option<String>,
    /// Concurrent operations.
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Per-operation reader/writer parallelism.
    #[serde(default)]
    pub io_concurrency: Option<usize>,
    /// Chunk / multipart part size, e.g. `8MiB`.
    #[serde(default)]
    pub chunk_size: Option<String>,
    /// HTTP version: `auto`, `h1` or `h2`.
    #[serde(default)]
    pub http: Option<String>,
    /// Retry attempts for temporary errors.
    #[serde(default)]
    pub retries: Option<usize>,
    /// Pricing preset for cost estimates (`aws-s3`, `gcs`, …; see capabilities).
    #[serde(default)]
    pub provider: Option<String>,
    /// Label stored with the run (e.g. a git SHA).
    #[serde(default)]
    pub label: Option<String>,
}

/// Budgets for this call (the server's budgets still apply; the stricter wins).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct BudgetIn {
    /// Stop when the estimated request cost exceeds this (USD).
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Stop after this many backend requests.
    #[serde(default)]
    pub max_requests: Option<u64>,
    /// Stop after transferring this much, e.g. `1GiB`.
    #[serde(default)]
    pub max_bytes: Option<String>,
    /// Stop after this much wall time, e.g. `2m`.
    #[serde(default)]
    pub max_duration: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct DoctorIn {
    #[serde(flatten)]
    pub target: TargetIn,
    /// Only check reachability and listing (forced on a read-only server).
    #[serde(default)]
    pub read_only: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ProbeIn {
    /// http(s) endpoint, or a storage URL whose endpoint should be probed.
    pub endpoint: String,
    /// Samples per measurement (default 3).
    #[serde(default)]
    pub samples: Option<usize>,
    /// `auto`, `h1` or `h2`.
    #[serde(default)]
    pub http: Option<String>,
    /// GET instead of HEAD for the cold/warm requests.
    #[serde(default)]
    pub get: bool,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub proxy: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct PlanIn {
    #[serde(flatten)]
    pub target: TargetIn,
    #[serde(flatten)]
    pub workload: WorkloadIn,
    #[serde(flatten)]
    pub budgets: BudgetIn,
    /// Refuse anything that writes (implied by `existing`).
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct StartRunIn {
    #[serde(flatten)]
    pub plan: PlanIn,
    /// Block until the run ends (with progress notifications) instead of returning at once.
    /// Only for short runs: clients time tool calls out.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct GetRunIn {
    /// Run id from `start_run`, or any RUNREF: id prefix, `last`, `last~N`, label.
    pub run: String,
    /// Include the full report when the run is finished (default true).
    #[serde(default)]
    pub include_report: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct CancelIn {
    /// Run id returned by `start_run` / `start_tune`.
    pub run_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ListRunsIn {
    /// At most this many stored runs, newest first (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ExplainIn {
    /// RUNREF: run id, id prefix, `last`, `last~N`, label.
    pub run: String,
    /// Findings to narrate (default 3).
    #[serde(default)]
    pub top: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct AnalyzeIn {
    /// RUNREF: run id, id prefix, `last`, `last~N`, label.
    pub run: String,
    /// Which requests to explain (time window, minimum latency, API). Default: the slowest
    /// `slowest_pct` percent.
    #[serde(default)]
    pub selection: Option<Selection>,
    /// Select the slowest N percent of requests when no selection is given (default 1).
    #[serde(default)]
    pub slowest_pct: Option<f64>,
}

/// Regression limits (unset: `oprobe compare` defaults).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ThresholdsIn {
    #[serde(default)]
    pub max_p99_increase_pct: Option<f64>,
    #[serde(default)]
    pub max_p50_increase_pct: Option<f64>,
    #[serde(default)]
    pub min_latency_delta_ms: Option<f64>,
    #[serde(default)]
    pub max_throughput_drop_pct: Option<f64>,
    /// Percentage points.
    #[serde(default)]
    pub max_error_increase_pp: Option<f64>,
    #[serde(default)]
    pub max_cost_increase_pct: Option<f64>,
    #[serde(default)]
    pub min_samples: Option<u64>,
    #[serde(default)]
    pub allow_throttling: Option<bool>,
    #[serde(default)]
    pub allow_workload_change: Option<bool>,
    #[serde(default)]
    pub allow_target_change: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct CompareIn {
    /// Baseline RUNREF.
    pub baseline: String,
    /// Candidate RUNREF (default `last`).
    #[serde(default)]
    pub candidate: Option<String>,
    #[serde(default)]
    pub thresholds: Option<ThresholdsIn>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct StartTuneIn {
    #[serde(flatten)]
    pub plan: PlanIn,
    /// `throughput` (default), `latency` or `cost`.
    #[serde(default)]
    pub objective: Option<String>,
    /// Measurement window per trial (default 5s).
    #[serde(default)]
    pub trial_duration: Option<String>,
    /// Warmup per trial (default 1s).
    #[serde(default)]
    pub trial_warmup: Option<String>,
    /// Maximum number of trials (default 24).
    #[serde(default)]
    pub max_trials: Option<usize>,
    /// Highest concurrency to try (default 256).
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    /// Block until the tune ends.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct CleanupIn {
    #[serde(flatten)]
    pub target: TargetIn,
    /// Only run prefixes older than this (default `1h`).
    #[serde(default)]
    pub older_than: Option<String>,
    /// Also remove persisted datasets.
    #[serde(default)]
    pub datasets: bool,
    /// Actually delete (needs a server started with `--allow-write`); otherwise a dry run.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct EmptyIn {}

// ---------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------

/// Result of `plan_run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PlanOut {
    pub plan: Plan,
    /// Budgets the run would get (server and request, the stricter of each).
    pub budgets: Budgets,
    /// The run writes test data.
    pub writes: bool,
    /// `start_run` would accept it on this server.
    pub allowed: bool,
    /// Why not, when not allowed.
    pub refusals: Vec<ErrorBody>,
    /// Plan estimates already above a budget (the run would stop early).
    pub budget_warnings: Vec<String>,
}

/// Status of a run started by this server (or a stored run).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunStatusOut {
    pub run_id: String,
    pub kind: JobKind,
    pub status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<JobProgress>,
    /// Wall time so far (or in total once ended).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budgets: Option<Budgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    /// The saved run file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<RunSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Box<Report>>,
    /// Suggested next steps (each with its MCP call and CLI command).
    pub next: Vec<crate::output::NextStep>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ActiveRun {
    pub run_id: String,
    pub kind: JobKind,
    pub status: JobStatus,
}

/// Result of `list_runs`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ListRunsOut {
    pub runs: Vec<RunSummary>,
    pub total_count: u64,
    /// Runs this server started (any status).
    pub started: Vec<ActiveRun>,
}

/// Result of `analyze_run`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AnalyzeOut {
    pub run_id: String,
    pub selection: Selection,
    pub attribution: Attribution,
    /// The top factors in words.
    pub factors_text: Vec<String>,
}

/// Result of `estimate_cost`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct EstimateOut {
    /// A price table applies (cloud provider detected or `provider` given).
    pub applicable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_per_request: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub estimated_requests_count: u64,
    pub estimated_bytes: u64,
    pub eta_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within_budget: Option<bool>,
    pub assumptions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TargetOut {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// Options with secrets redacted.
    pub options: BTreeMap<String, String>,
    /// Tools may use it on this server (`--allow-target`).
    pub allowed: bool,
}

/// Result of `list_targets`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TargetsOut {
    pub targets: Vec<TargetOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    /// The server's `--allow-target` list (empty: every target).
    pub allow_targets: Vec<String>,
    /// The server accepts runs that write test data.
    pub allow_write: bool,
    /// Budgets every run gets.
    pub server_budgets: Budgets,
}

// ---------------------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------------------

/// A tool definition.
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Writes test data (needs `--allow-write`, except `memory://` and `existing`).
    pub writes: bool,
    /// Never changes anything anywhere.
    pub read_only: bool,
    /// Talks to the storage target.
    pub open_world: bool,
    pub input: fn() -> Map<String, Value>,
    pub output: fn() -> Map<String, Value>,
}

fn input<T: JsonSchema>() -> Map<String, Value> {
    let mut m = schema_of::<T>(Direction::Input);
    m.insert("type".into(), "object".into());
    m
}

fn output<T: JsonSchema>() -> Map<String, Value> {
    let mut m = schema_of::<T>(Direction::Output);
    m.insert("type".into(), "object".into());
    m
}

/// Every tool, in `tools/list` order.
pub const SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "doctor",
        title: "Check a target",
        description: "Preflight a storage target: configuration, DNS/TCP/TLS, clock skew, credentials and permissions (list; write/read/delete only on a server with --allow-write). Run this first.",
        writes: false,
        read_only: true,
        open_world: true,
        input: input::<DoctorIn>,
        output: output::<crate::output::Envelope<crate::cli::doctor::DoctorOutput>>,
    },
    ToolSpec {
        name: "probe_endpoint",
        title: "Probe an endpoint",
        description: "Measure DNS, TCP, proxy and TLS handshakes and cold vs warm time-to-first-byte of an http(s) endpoint (HEAD requests only).",
        writes: false,
        read_only: true,
        open_world: true,
        input: input::<ProbeIn>,
        output: output::<crate::output::Envelope<crate::net::probe::ProbeReport>>,
    },
    ToolSpec {
        name: "plan_run",
        title: "Plan a run (dry run)",
        description: "Dry-run plan of a measurement without touching the target: estimated operations, requests, bytes, objects written/deleted, cost and ETA, the budgets that would apply, and whether this server would accept it.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<PlanIn>,
        output: output::<PlanOut>,
    },
    ToolSpec {
        name: "start_run",
        title: "Start a run",
        description: "Start a measurement in the background and return its run id at once (set wait=true to block for short runs). Budgets of the server and the call apply; progress notifications are sent when the call carries a progressToken. Poll get_run for the result.",
        writes: true,
        read_only: false,
        open_world: true,
        input: input::<StartRunIn>,
        output: output::<RunStatusOut>,
    },
    ToolSpec {
        name: "get_run",
        title: "Get a run",
        description: "Status, progress and (when finished) the full report of a run started here, or of any stored run (RUNREF: id, prefix, last, last~N, label).",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<GetRunIn>,
        output: output::<RunStatusOut>,
    },
    ToolSpec {
        name: "cancel_run",
        title: "Cancel a run",
        description: "Stop a run or tune started by this server. Cleanup still runs and the partial run is saved.",
        writes: false,
        read_only: false,
        open_world: false,
        input: input::<CancelIn>,
        output: output::<RunStatusOut>,
    },
    ToolSpec {
        name: "list_runs",
        title: "List runs",
        description: "Stored runs, newest first (verdict, headline, ops/s, p50/p99, error ratio, cost), plus the runs this server started.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<ListRunsIn>,
        output: output::<ListRunsOut>,
    },
    ToolSpec {
        name: "explain_run",
        title: "Explain a run",
        description: "Narrated explanation of a stored run: verdict, top findings with evidence and documentation, outlier attribution of the slowest 1% of requests, and suggested next commands.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<ExplainIn>,
        output: output::<crate::output::Envelope<ExplainRun>>,
    },
    ToolSpec {
        name: "analyze_run",
        title: "Attribute outliers",
        description: "Rank request attributes (API, status, connection cold/warm, remote IP, retry, error, dominant stage) over-represented among selected requests (default: the slowest 1%) versus the rest.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<AnalyzeIn>,
        output: output::<AnalyzeOut>,
    },
    ToolSpec {
        name: "compare_runs",
        title: "Compare two runs",
        description: "Regression gate between a baseline and a candidate run: per-operation latency, throughput, errors, amplification and cost checks; `passed` is false on a regression.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<CompareIn>,
        output: output::<crate::analysis::compare::Comparison>,
    },
    ToolSpec {
        name: "start_tune",
        title: "Start tuning",
        description: "Start a throttle-aware search for the best client configuration in the background (needs --allow-write unless `existing` is set); returns a run id. The winner's report includes the trial table.",
        writes: true,
        read_only: false,
        open_world: true,
        input: input::<StartTuneIn>,
        output: output::<RunStatusOut>,
    },
    ToolSpec {
        name: "estimate_cost",
        title: "Estimate cost",
        description: "Estimate the request cost of a run at list prices (provider detected from the target or given), against the cost budget.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<PlanIn>,
        output: output::<EstimateOut>,
    },
    ToolSpec {
        name: "cleanup",
        title: "Clean up test data",
        description: "Find test data left behind by interrupted runs (oprobe-run-*/ prefixes). A dry run unless confirm=true on a server started with --allow-write.",
        writes: true,
        read_only: false,
        open_world: true,
        input: input::<CleanupIn>,
        output: output::<crate::output::Envelope<crate::cli::misc::CleanupOutput>>,
    },
    ToolSpec {
        name: "list_targets",
        title: "List targets",
        description: "Configured targets (secrets redacted), which ones this server allows, whether it accepts writes, and its budgets.",
        writes: false,
        read_only: true,
        open_world: false,
        input: input::<EmptyIn>,
        output: output::<TargetsOut>,
    },
];

pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    SPECS.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

/// A progress notification sink: `(progress, total, message)`.
pub type ProgressSink = Arc<dyn Fn(f64, Option<f64>, Option<String>) + Send + Sync>;

/// Per-call context.
#[derive(Clone, Default)]
pub struct CallCx {
    pub progress: Option<ProgressSink>,
    pub ct: CancellationToken,
}

/// A tool's successful result.
pub struct Reply {
    pub structured: Value,
    pub text: String,
}

fn reply<T: Serialize>(v: &T, text: impl Into<String>) -> anyhow::Result<Reply> {
    Ok(Reply {
        structured: serde_json::to_value(v)?,
        text: text.into(),
    })
}

/// Shared server state.
pub struct State {
    pub cfg: McpConfig,
    pub jobs: Jobs,
}

impl State {
    pub fn new(cfg: McpConfig) -> Self {
        let jobs = Jobs::new(cfg.max_concurrent_runs);
        Self { cfg, jobs }
    }

    fn ctx(&self) -> Ctx {
        Ctx::new(Output::silent(), self.cfg.paths.clone())
    }

    fn store(&self) -> crate::store::RunStore {
        self.cfg.paths.run_store()
    }
}

fn args<T: DeserializeOwned>(v: Map<String, Value>) -> anyhow::Result<T> {
    serde_json::from_value(Value::Object(v)).map_err(|e| {
        OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("invalid arguments: {e}"),
        )
        .with_hint("see the tool's inputSchema (tools/list)")
        .into()
    })
}

/// Call a tool. Errors are classified [`ErrorBody`]s.
pub async fn call(
    st: &Arc<State>,
    name: &str,
    a: Map<String, Value>,
    cx: CallCx,
) -> Result<Reply, ErrorBody> {
    let r = match name {
        "doctor" => doctor(st, args(a).map_err(err)?).await,
        "probe_endpoint" => probe(st, args(a).map_err(err)?).await,
        "plan_run" => plan_run(st, args(a).map_err(err)?),
        "start_run" => start_run(st, args(a).map_err(err)?, cx).await,
        "get_run" => get_run(st, args(a).map_err(err)?),
        "cancel_run" => cancel_run(st, args(a).map_err(err)?),
        "list_runs" => list_runs(st, args(a).map_err(err)?),
        "explain_run" => explain_run(st, args(a).map_err(err)?),
        "analyze_run" => analyze_run(st, args(a).map_err(err)?),
        "compare_runs" => compare_runs(st, args(a).map_err(err)?),
        "start_tune" => start_tune(st, args(a).map_err(err)?, cx).await,
        "estimate_cost" => estimate_cost(st, args(a).map_err(err)?),
        "cleanup" => cleanup(st, args(a).map_err(err)?).await,
        "list_targets" => list_targets(st),
        other => Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("unknown tool `{other}`"),
        )
        .into()),
    };
    r.map_err(err)
}

fn err(e: anyhow::Error) -> ErrorBody {
    crate::errors::classify(&e, "s3")
}

// --- helpers --------------------------------------------------------------------------

fn http_arg(v: Option<&str>) -> anyhow::Result<Option<HttpArg>> {
    Ok(match v {
        None => None,
        Some(s) => Some(match crate::input::http_version(s)? {
            crate::net::HttpVersion::Auto => HttpArg::Auto,
            crate::net::HttpVersion::H1 => HttpArg::H1,
            crate::net::HttpVersion::H2 => HttpArg::H2,
        }),
    })
}

fn target_args(t: &TargetIn) -> TargetArgs {
    TargetArgs {
        target: Some(t.target.clone()),
        profile: None,
        endpoint: t.endpoint.clone(),
        region: t.region.clone(),
        options: t.options.iter().map(|(k, v)| format!("{k}={v}")).collect(),
        anonymous: t.anonymous,
        ca_cert: None,
        insecure: t.insecure,
        proxy: t.proxy.clone(),
    }
}

/// The CLI arguments selecting `t` (for child commands).
fn target_cli(t: &TargetIn) -> Vec<String> {
    let mut v = vec![t.target.clone()];
    if let Some(e) = &t.endpoint {
        v.extend(["--endpoint".into(), e.clone()]);
    }
    if let Some(r) = &t.region {
        v.extend(["--region".into(), r.clone()]);
    }
    for (k, val) in &t.options {
        v.extend(["--option".into(), format!("{k}={val}")]);
    }
    if t.anonymous {
        v.push("--anonymous".into());
    }
    if t.insecure {
        v.push("--insecure".into());
    }
    if let Some(p) = &t.proxy {
        v.extend(["--proxy".into(), p.clone()]);
    }
    v
}

fn profile_of(
    st: &State,
    t: &TargetIn,
    w: &WorkloadIn,
) -> anyhow::Result<(Profile, Option<String>)> {
    if t.target.trim().is_empty() {
        return Err(
            OprobeError::new(ErrorCode::UsageUnknownTarget, "no target given")
                .with_hint(
                    "pass `target`: a configured name (list_targets) or a URL such as memory://",
                )
                .into(),
        );
    }
    let wa = WorkloadArgs {
        preset: w.preset.clone(),
        duration: w.duration.clone(),
        warmup: w.warmup.clone(),
        max_ops: w.max_ops,
        rate: w.rate,
        existing: w.existing.clone(),
        objects: w.objects,
        object_size: w.object_size.clone(),
        access: w.access.clone(),
        persist_dataset: false,
        no_verify: false,
        keep_data: false,
    };
    let ka = KnobArgs {
        concurrency: w.concurrency,
        io_concurrency: w.io_concurrency,
        chunk_size: w.chunk_size.clone(),
        http: http_arg(w.http.as_deref())?,
        retries: w.retries,
        ..Default::default()
    };
    let ca = CostArgs {
        provider: w.provider.clone(),
        ..Default::default()
    };
    let ctx = st.ctx();
    // The same parsers and codes as the CLI: no remapping here (design v4 §8).
    crate::cli::resolve_profile(&ctx, &target_args(t), &wa, &ka, &ca)
}

/// The identity of a configured target (its URL, endpoint and options).
fn entry_id(e: &crate::store::TargetEntry) -> Option<TargetId> {
    if let Some(id) = TargetId::parse(&e.url).ok().filter(|i| i.scheme == "lab") {
        return Some(id);
    }
    let mut t = crate::config::TargetSpec::from_url(&e.url).ok()?;
    if let Some(ep) = &e.endpoint {
        t.options.insert("endpoint".into(), ep.clone());
    }
    for (k, v) in &e.options {
        t.options.insert(k.clone(), v.clone());
    }
    TargetId::of(&t).ok()
}

/// Whether one `--allow-target` entry allows `given` (as typed) at identity `id`: a URL entry
/// (or a local path) contains `id` on whole path segments after both are normalised
/// (`store::TargetId`); a name entry matches the name, or contains `id` as its target does.
fn entry_allows(
    entry: &str,
    config: Option<&crate::store::ConfigFile>,
    given: &str,
    id: Option<&TargetId>,
) -> bool {
    let named = config.and_then(|c| c.target(entry));
    if named.is_none() && (entry.contains("://") || entry.contains('/') || entry.contains('\\')) {
        return match (TargetId::parse(entry), id) {
            (Ok(a), Some(i)) => a.contains(i),
            _ => false,
        };
    }
    entry == given
        || named
            .and_then(entry_id)
            .zip(id)
            .is_some_and(|(a, i)| a.contains(i))
}

/// `--allow-target`: `given` (as the caller typed it) at identity `id` must be allowed.
fn check_allowed(
    cfg: &McpConfig,
    config: Option<&crate::store::ConfigFile>,
    given: &str,
    id: Option<&TargetId>,
) -> anyhow::Result<()> {
    if allowed(cfg, config, given, id) {
        return Ok(());
    }
    Err(OprobeError::new(
        ErrorCode::SafetyRefused,
        format!(
            "target `{given}` is not allowed on this MCP server{}",
            id.map(|i| format!(" (it resolves to {i})"))
                .unwrap_or_default()
        ),
    )
    .with_hint(format!("allowed: {}", cfg.allow_targets.join(", ")))
    .into())
}

fn allowed(
    cfg: &McpConfig,
    config: Option<&crate::store::ConfigFile>,
    given: &str,
    id: Option<&TargetId>,
) -> bool {
    cfg.allow_targets.is_empty()
        || cfg
            .allow_targets
            .iter()
            .any(|a| entry_allows(a, config, given, id))
}

/// A target identity, or the `safety.refused` error of a target that leaves its root.
fn identity(t: &crate::config::TargetSpec, given: &str) -> anyhow::Result<TargetId> {
    TargetId::of(t).map_err(|e| {
        OprobeError::new(
            ErrorCode::SafetyRefused,
            format!(
                "target `{given}` is refused on this MCP server: {}",
                e.message
            ),
        )
        .with_hint("pass the target without `..` segments")
        .into()
    })
}

/// Options that never change where requests go or which bucket/root they touch.
const HARMLESS_OPTIONS: [&str; 2] = ["region", "enable_virtual_host_style"];

/// What makes two targets the same place: every option except [`HARMLESS_OPTIONS`], the
/// scheme, TLS trust and the proxy. Returns the names that differ (never their values).
fn identity_diff(a: &crate::config::TargetSpec, b: &crate::config::TargetSpec) -> Vec<String> {
    let mut d = Vec::new();
    if a.scheme != b.scheme {
        d.push("scheme".to_owned());
    }
    let keys: std::collections::BTreeSet<&String> =
        a.options.keys().chain(b.options.keys()).collect();
    for k in keys {
        if !HARMLESS_OPTIONS.contains(&k.as_str()) && a.options.get(k) != b.options.get(k) {
            d.push(k.clone());
        }
    }
    if a.insecure != b.insecure {
        d.push("insecure".into());
    }
    if a.ca_cert != b.ca_cert {
        d.push("ca_cert".into());
    }
    if a.proxy != b.proxy {
        d.push("proxy".into());
    }
    d
}

/// `--allow-target` for a tool call: the target as configured (or typed, without a query
/// string) must be allowed, and the call's overrides (`endpoint`, `options`, `proxy`,
/// `insecure`, query parameters) must not move it to another host, bucket or root.
/// `region` and `anonymous` are accepted (they never retarget the allowed credentials).
fn check_target(st: &State, t: &TargetIn, effective: &Profile) -> anyhow::Result<()> {
    if st.cfg.allow_targets.is_empty() {
        return Ok(());
    }
    let given = t.target.trim();
    let bare_given = given.split(['?', '#']).next().unwrap_or(given).to_owned();
    let bare = TargetIn {
        target: bare_given.clone(),
        region: t.region.clone(),
        anonymous: t.anonymous,
        ..Default::default()
    };
    let ctx = st.ctx();
    let config = ctx.config()?;
    let (ta, _) = crate::cli::resolve_target(&target_args(&bare), config)?;
    let reference = crate::cli::build_profile(
        &ta,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    // Both sides normalised (`..`, `//`, `./`, case, default ports, trailing `/`): what the
    // run would really touch must lie inside an allowed location (SEC-1, MCP-1).
    let id = identity(&effective.target, &bare_given)?;
    check_allowed(&st.cfg, Some(config), &bare_given, Some(&id))?;
    if let Some(prefix) = effective.workload.effective_dataset().existing_prefix {
        let leaves = prefix.split(['/', '\\']).any(|seg| seg == "..");
        let inside = id
            .join(&prefix)
            .is_ok_and(|e| allowed(&st.cfg, Some(config), &bare_given, Some(&e)));
        if leaves || !inside {
            return Err(OprobeError::new(
                ErrorCode::SafetyRefused,
                format!(
                    "`existing: {prefix}` leaves the allowed target `{bare_given}` on this MCP server"
                ),
            )
            .with_hint("pass a prefix below the target root, without `..` segments")
            .into());
        }
    }
    let diff = identity_diff(&reference.target, &effective.target);
    if diff.is_empty() {
        return Ok(());
    }
    Err(OprobeError::new(
        ErrorCode::SafetyRefused,
        format!(
            "target `{bare_given}` is allowed on this MCP server, but this call changes where it points ({}): refused",
            diff.join(", ")
        ),
    )
    .with_hint(
        "call the tool with the allowed target alone (only `region` and `anonymous` may be set), \
         or restart the server with an --allow-target entry for the other location",
    )
    .into())
}

/// `--allow-target` for `probe_endpoint`: the endpoint must be allowed, and no proxy or TLS
/// override may be given.
fn check_probe(cfg: &McpConfig, a: &ProbeIn) -> anyhow::Result<()> {
    let id = TargetId::parse(&a.endpoint).ok();
    let config = crate::store::ConfigFile::load(&cfg.paths.config_file).ok();
    check_allowed(cfg, config.as_ref(), &a.endpoint, id.as_ref())?;
    if !cfg.allow_targets.is_empty() && (a.insecure || a.proxy.is_some()) {
        return Err(OprobeError::new(
            ErrorCode::SafetyRefused,
            "this MCP server restricts targets (--allow-target): `proxy` and `insecure` overrides are refused",
        )
        .into());
    }
    Ok(())
}

/// The write rail of this server: refuses runs that write unless `--allow-write`
/// (`memory://` writes only into this process and is always allowed).
fn write_refusal(cfg: &McpConfig, p: &Profile, read_only: bool) -> Option<anyhow::Error> {
    let err = crate::guard::check_read_only(p).err()?;
    if read_only {
        return Some(err);
    }
    if cfg.allow_write || p.target.scheme == "memory" {
        return None;
    }
    Some(
        OprobeError::new(
            ErrorCode::SafetyReadOnly,
            format!(
                "this MCP server is read-only: the run would write test data to {}",
                p.target.display()
            ),
        )
        .with_hint(
            "benchmark existing objects with `existing: PREFIX`, or restart the server with `oprobe mcp --allow-write`",
        )
        .into(),
    )
}

fn min_opt<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y < x { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

fn bad(flag: &str, e: anyhow::Error) -> anyhow::Error {
    let b = crate::errors::classify(&e, "s3");
    let mut o = OprobeError::new(b.code, format!("invalid {flag}: {}", b.message));
    o.hint = b.hint;
    o.into()
}

fn budgets_of(server: &Budgets, b: &BudgetIn) -> anyhow::Result<Budgets> {
    if let Some(c) = b.max_cost_usd {
        anyhow::ensure!(
            c.is_finite() && c >= 0.0,
            OprobeError::new(ErrorCode::UsageInvalidArgument, "max_cost_usd must be >= 0")
        );
    }
    let bytes = b
        .max_bytes
        .as_deref()
        .map(|v| crate::input::size(v).map(|x| x.0))
        .transpose()
        .map_err(|e| bad("max_bytes", e.into()))?;
    let dur = b
        .max_duration
        .as_deref()
        .map(|v| crate::input::duration(v).map(|d| d.as_secs_f64()))
        .transpose()
        .map_err(|e| bad("max_duration", e.into()))?;
    Ok(Budgets {
        max_cost_usd: min_opt(server.max_cost_usd, b.max_cost_usd),
        max_requests_count: min_opt(server.max_requests_count, b.max_requests),
        max_bytes: min_opt(server.max_bytes, bytes),
        max_duration_s: min_opt(server.max_duration_s, dur),
    })
}

fn budget_warnings(p: &Plan, b: &Budgets) -> Vec<String> {
    let mut w = Vec::new();
    if let Some(m) = b.max_requests_count
        && p.estimated_requests_count > m
    {
        w.push(format!(
            "~{} requests planned, budget {m}: the run will stop early",
            p.estimated_requests_count
        ));
    }
    if let Some(m) = b.max_bytes
        && p.estimated_bytes > m
    {
        w.push(format!(
            "~{} planned, budget {}: the run will stop early",
            crate::util::fmt_bytes(p.estimated_bytes),
            crate::util::fmt_bytes(m)
        ));
    }
    if let (Some(m), Some(c)) = (b.max_cost_usd, p.estimated_cost_usd)
        && c > m
    {
        w.push(format!(
            "~${c:.4} planned, budget ${m:.4}: the run will stop early"
        ));
    }
    if let Some(m) = b.max_duration_s
        && p.eta_s > m
    {
        w.push(format!(
            "ETA {:.0}s, budget {m:.0}s: the run will stop early",
            p.eta_s
        ));
    }
    w
}

struct Prepared {
    profile: Profile,
    target_name: Option<String>,
    run_id: RunId,
    run_prefix: String,
    budgets: Budgets,
    plan: Plan,
    refusals: Vec<anyhow::Error>,
    writes: bool,
}

fn prepare(st: &State, a: &PlanIn) -> anyhow::Result<Prepared> {
    let (profile, target_name) = profile_of(st, &a.target, &a.workload)?;
    let budgets = budgets_of(&st.cfg.budgets, &a.budgets)?;
    let run_id = RunId::new();
    let run_prefix = run_id.run_prefix();
    let existing = profile
        .workload
        .effective_dataset()
        .existing_prefix
        .is_some();
    let read_only = a.read_only || existing;
    let writes = crate::guard::check_read_only(&profile).is_err();
    let plan = crate::guard::plan(&profile, &run_prefix, read_only, &budgets);
    let mut refusals = Vec::new();
    if let Err(e) = check_target(st, &a.target, &profile) {
        refusals.push(e);
    }
    if let Some(e) = write_refusal(&st.cfg, &profile, read_only) {
        refusals.push(e);
    }
    Ok(Prepared {
        profile,
        target_name,
        run_id,
        run_prefix,
        budgets,
        plan,
        refusals,
        writes,
    })
}

/// Run `oprobe ARGS -o json` (same config and data directory) and parse its result.
async fn child_json(st: &State, args: Vec<String>, timeout: Duration) -> anyhow::Result<Value> {
    let exe = std::env::current_exe()?;
    let mut c = tokio::process::Command::new(exe);
    c.args(&args)
        .args(["-o", "json", "--color", "never", "--non-interactive"])
        .env("OPROBE_DATA_DIR", &st.cfg.paths.data_dir)
        .env("OPROBE_CONFIG", &st.cfg.paths.config_file)
        .env_remove("OPROBE_OUTPUT")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, c.output()).await {
        Ok(o) => o?,
        Err(_) => {
            return Err(OprobeError::new(
                ErrorCode::NetTimeout,
                format!("`oprobe {}` took longer than {timeout:?}", args[0]),
            )
            .into());
        }
    };
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        OprobeError::new(
            ErrorCode::Internal,
            format!(
                "`oprobe {}` printed no JSON result ({e}): {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )
    })?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        let body: ErrorBody = serde_json::from_value(v["error"].clone())?;
        let mut e = OprobeError::new(body.code, body.message);
        if let Some(h) = body.hint {
            e = e.with_hint(h);
        }
        return Err(e.into());
    }
    Ok(v)
}

// --- tools ----------------------------------------------------------------------------

async fn doctor(st: &Arc<State>, a: DoctorIn) -> anyhow::Result<Reply> {
    let (p, _) = profile_of(st, &a.target, &WorkloadIn::default())?;
    check_target(st, &a.target, &p)?;
    let read_only = !st.cfg.allow_write || a.read_only.unwrap_or(false);
    let mut args = vec!["doctor".to_owned()];
    args.extend(target_cli(&a.target));
    if read_only {
        args.push("--read-only".into());
    }
    let v = child_json(st, args, Duration::from_secs(120)).await?;
    let failed = v["failed_count"].as_u64().unwrap_or(0);
    let text = if v["ready"].as_bool() == Some(true) {
        format!(
            "{} is ready to benchmark ({} warnings){}",
            v["target"].as_str().unwrap_or(""),
            v["warned_count"].as_u64().unwrap_or(0),
            if read_only {
                "; write checks skipped (read-only)"
            } else {
                ""
            }
        )
    } else {
        format!(
            "{failed} check(s) failed for {}",
            v["target"].as_str().unwrap_or("")
        )
    };
    Ok(Reply {
        structured: v,
        text,
    })
}

async fn probe(st: &Arc<State>, a: ProbeIn) -> anyhow::Result<Reply> {
    check_probe(&st.cfg, &a)?;
    let mut args = vec![
        "probe".to_owned(),
        a.endpoint.clone(),
        "-n".into(),
        a.samples.unwrap_or(3).clamp(1, 50).to_string(),
    ];
    if let Some(h) = http_arg(a.http.as_deref())? {
        args.extend([
            "--http".into(),
            match h {
                HttpArg::Auto => "auto",
                HttpArg::H1 => "h1",
                HttpArg::H2 => "h2",
            }
            .into(),
        ]);
    }
    if a.get {
        args.push("--get".into());
    }
    if a.insecure {
        args.push("--insecure".into());
    }
    if let Some(p) = &a.proxy {
        args.extend(["--proxy".into(), p.clone()]);
    }
    let v = child_json(st, args, Duration::from_secs(120)).await?;
    let ms = |k: &str| {
        v[k]["p50"]
            .as_f64()
            .or_else(|| v[k]["p50_ms"].as_f64())
            .unwrap_or(0.0)
    };
    let text = format!(
        "{}: cold TTFB {} · warm TTFB {}",
        a.endpoint,
        fmt_ms(ms("cold_ttfb")),
        fmt_ms(ms("warm_ttfb"))
    );
    Ok(Reply {
        structured: v,
        text,
    })
}

fn plan_run(st: &Arc<State>, a: PlanIn) -> anyhow::Result<Reply> {
    let p = prepare(st, &a)?;
    let refusals: Vec<ErrorBody> = p.refusals.iter().map(err_ref).collect();
    let out = PlanOut {
        budget_warnings: budget_warnings(&p.plan, &p.budgets),
        plan: p.plan,
        budgets: p.budgets,
        writes: p.writes,
        allowed: refusals.is_empty(),
        refusals,
    };
    let text = format!(
        "plan: ~{} ops · ~{} requests · ~{} · cost {} · ETA {:.0}s · {}",
        out.plan.estimated_ops_count,
        out.plan.estimated_requests_count,
        crate::util::fmt_bytes(out.plan.estimated_bytes),
        out.plan
            .estimated_cost_usd
            .map(crate::util::fmt_usd)
            .unwrap_or_else(|| "n/a".into()),
        out.plan.eta_s,
        if out.allowed {
            "allowed".to_owned()
        } else {
            format!("refused: {}", out.refusals[0].message)
        }
    );
    reply(&out, text)
}

fn err_ref(e: &anyhow::Error) -> ErrorBody {
    crate::errors::classify(e, "s3")
}

fn status_of(st: &State, job: &Job, include_report: bool) -> RunStatusOut {
    let v = job.view();
    let (summary, report) = match (&v.path, v.status.is_final()) {
        (Some(path), true) => match crate::store::runs::load_path(std::path::Path::new(path)) {
            Ok(run) => (
                Some(RunSummary::from(&run.report)),
                include_report.then(|| Box::new(run.report)),
            ),
            Err(_) => (None, None),
        },
        _ => (None, None),
    };
    let _ = st;
    use crate::output::NextStep;
    let id = job.id.as_str();
    let next = match v.status {
        JobStatus::Queued | JobStatus::Running => vec![
            NextStep::new("poll every few seconds", format!("oprobe runs show {id}"))
                .with_mcp("get_run", serde_json::json!({ "run": id })),
        ],
        _ if v.path.is_some() => finished_steps(id),
        _ => vec![
            NextStep::new("list stored runs", "oprobe runs list")
                .with_mcp("list_runs", serde_json::json!({})),
        ],
    };
    RunStatusOut {
        run_id: job.id.clone(),
        kind: job.kind,
        status: v.status,
        progress: Some(v.progress),
        wall_s: Some(v.wall_s),
        plan: Some(job.plan.clone()),
        budgets: Some(job.budgets.clone()),
        error: v.error,
        path: v.path,
        summary,
        report,
        next,
    }
}

/// Next steps of a finished, stored run.
fn finished_steps(id: &str) -> Vec<crate::output::NextStep> {
    use crate::output::NextStep;
    vec![
        NextStep::new("explain the run", format!("oprobe explain {id}"))
            .with_mcp("explain_run", serde_json::json!({ "run": id })),
        NextStep::new(
            "attribute the slowest requests",
            format!("oprobe analyze {id}"),
        )
        .with_mcp("analyze_run", serde_json::json!({ "run": id })),
        NextStep::new(
            "compare with the previous run",
            format!("oprobe compare last~1 {id}"),
        )
        .with_mcp(
            "compare_runs",
            serde_json::json!({ "baseline": "last~1", "candidate": id }),
        ),
    ]
}

fn status_text(s: &RunStatusOut) -> String {
    let mut t = format!("{} {} {:?}", s.kind_label(), s.run_id, s.status).to_lowercase();
    if let Some(p) = &s.progress
        && matches!(s.status, JobStatus::Running)
    {
        t += &format!(
            " · {} {:.1}s · {} ops · {} requests",
            p.phase.map(|x| x.label()).unwrap_or("starting"),
            p.elapsed_s,
            p.ops_count,
            p.requests_count
        );
    }
    if let Some(sm) = &s.summary {
        t += &format!(
            " · {}: {} · {:.1} ops/s · p99 {}",
            sm.verdict_level.as_str(),
            sm.headline,
            sm.ops_per_s,
            fmt_ms(sm.p99_ms)
        );
    }
    if let Some(e) = &s.error {
        t += &format!(" · error [{}] {}", e.code, e.message);
    }
    t
}

impl RunStatusOut {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            JobKind::Run => "run",
            JobKind::Tune => "tune",
        }
    }
}

/// Forward a job's progress to the caller as MCP progress notifications.
fn forward_progress(job: Arc<Job>, sink: ProgressSink) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    job.bus.subscribe(Arc::new(move |e| {
        let _ = tx.send(e.clone());
    }));
    let mut status = job.watch();
    let total = Some(job.plan.eta_s.max(0.001));
    tokio::spawn(async move {
        let start = Instant::now();
        let mut last_sent = 0.0f64;
        let mut last_at: Option<Instant> = None;
        let mut send = |msg: String, force: bool| {
            let p = start.elapsed().as_secs_f64().max(last_sent + 1e-3);
            let due = last_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
            if force || due {
                sink(p, total, Some(msg));
                last_sent = p;
                last_at = Some(Instant::now());
            }
        };
        send("queued".into(), true);
        loop {
            tokio::select! {
                e = rx.recv() => match e {
                    Some(e) => match e.kind {
                        EventKind::Phase { phase } => send(format!("phase {}", phase.label()), true),
                        EventKind::Progress { elapsed_s, total_s, ops, requests } => send(
                            format!(
                                "{:.1}s{} · {ops} ops · {requests} requests",
                                elapsed_s,
                                total_s.map(|t| format!("/{t:.0}s")).unwrap_or_default()
                            ),
                            false,
                        ),
                        EventKind::Trial { trial } => send(
                            format!("trial {} · {:.1} ops/s · p99 {}", trial.id, trial.ops_per_s, fmt_ms(trial.p99_ms)),
                            true,
                        ),
                        _ => {}
                    },
                    None => break,
                },
                s = status.wait_for(|s| s.is_final()) => {
                    let label = s.map(|s| format!("{:?}", *s).to_lowercase()).unwrap_or_else(|_| "ended".into());
                    send(label, true);
                    break;
                }
            }
        }
    });
}

async fn wait_or_return(st: &Arc<State>, job: Arc<Job>, wait: bool, cx: CallCx) -> RunStatusOut {
    if let Some(sink) = cx.progress.clone() {
        forward_progress(job.clone(), sink);
    }
    if wait {
        tokio::select! {
            _ = job.wait() => {}
            _ = cx.ct.cancelled() => {
                job.request_cancel();
                job.wait().await;
            }
        }
    } else {
        // Let the job leave `queued` when a slot is free, so the first status is current.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    status_of(st, &job, wait)
}

async fn start_run(st: &Arc<State>, a: StartRunIn, cx: CallCx) -> anyhow::Result<Reply> {
    let p = prepare(st, &a.plan)?;
    if let Some(e) = p.refusals.into_iter().next() {
        return Err(e);
    }
    let job = st.jobs.spawn(JobSpec {
        kind: JobKind::Run,
        run_id: p.run_id,
        run_prefix: p.run_prefix,
        profile: p.profile,
        target_name: p.target_name,
        budgets: p.budgets,
        plan: p.plan,
        trace_max_requests: crate::cli::trace_max_requests(&st.ctx()),
        store: st.store(),
        label: a.plan.workload.label.clone(),
        tune: None,
    });
    let out = wait_or_return(st, job, a.wait, cx).await;
    let text = status_text(&out);
    reply(&out, text)
}

async fn start_tune(st: &Arc<State>, a: StartTuneIn, cx: CallCx) -> anyhow::Result<Reply> {
    use crate::cli::args::tune_defaults as d;
    let p = prepare(st, &a.plan)?;
    if let Some(e) = p.refusals.into_iter().next() {
        return Err(e);
    }
    let objective = match a.objective.as_deref().unwrap_or("throughput") {
        "throughput" => crate::tune::Objective::Throughput,
        "latency" => crate::tune::Objective::Latency,
        "cost" => crate::tune::Objective::Cost,
        o => {
            return Err(OprobeError::new(
                ErrorCode::UsageInvalidArgument,
                format!("invalid objective `{o}` (throughput, latency, cost)"),
            )
            .into());
        }
    };
    let opts = crate::cli::tune_options(
        objective,
        &crate::cli::TuneSettings {
            trial_duration: a.trial_duration.as_deref().unwrap_or(d::TRIAL_DURATION),
            trial_warmup: a.trial_warmup.as_deref().unwrap_or(d::TRIAL_WARMUP),
            max_trials: a.max_trials.unwrap_or(d::MAX_TRIALS),
            max_concurrency: a.max_concurrency.unwrap_or(d::MAX_CONCURRENCY),
            cooldown: d::COOLDOWN,
        },
        p.profile.workload.rate,
    )?;
    let mut plan_profile = p.profile.clone();
    plan_profile.workload.duration =
        crate::util::HumanDuration(opts.trial.duration * opts.max_trials as u32);
    plan_profile.workload.warmup =
        crate::util::HumanDuration(opts.trial.warmup * opts.max_trials as u32);
    let plan = crate::guard::plan(&plan_profile, &p.run_prefix, false, &p.budgets);
    let job = st.jobs.spawn(JobSpec {
        kind: JobKind::Tune,
        run_id: p.run_id,
        run_prefix: p.run_prefix,
        profile: p.profile,
        target_name: p.target_name,
        budgets: p.budgets,
        plan,
        trace_max_requests: crate::cli::trace_max_requests(&st.ctx()),
        store: st.store(),
        label: a.plan.workload.label.clone(),
        tune: Some(opts),
    });
    let out = wait_or_return(st, job, a.wait, cx).await;
    let text = status_text(&out);
    reply(&out, text)
}

fn get_run(st: &Arc<State>, a: GetRunIn) -> anyhow::Result<Reply> {
    let include = a.include_report.unwrap_or(true);
    if let Some(job) = st.jobs.get(&a.run) {
        let out = status_of(st, &job, include);
        let text = status_text(&out);
        return reply(&out, text);
    }
    let path = st.store().resolve(&a.run)?;
    let run = crate::store::runs::load_path(&path)?;
    let id = run.report.run.id.clone();
    let out = RunStatusOut {
        run_id: id.clone(),
        kind: if run.report.tune.is_some() {
            JobKind::Tune
        } else {
            JobKind::Run
        },
        status: JobStatus::Stored,
        progress: None,
        wall_s: None,
        plan: None,
        budgets: None,
        error: None,
        path: Some(path.display().to_string()),
        summary: Some(RunSummary::from(&run.report)),
        report: include.then(|| Box::new(run.report)),
        next: finished_steps(&id),
    };
    let text = status_text(&out);
    reply(&out, text)
}

fn cancel_run(st: &Arc<State>, a: CancelIn) -> anyhow::Result<Reply> {
    let Some(job) = st.jobs.get(&a.run_id) else {
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownRun,
            format!("no run `{}` was started by this server", a.run_id),
        )
        .with_hint("list_runs shows the runs this server started")
        .into());
    };
    if !job.status().is_final() {
        job.request_cancel();
    }
    let out = status_of(st, &job, false);
    let text = format!("cancel requested · {}", status_text(&out));
    reply(&out, text)
}

fn list_runs(st: &Arc<State>, a: ListRunsIn) -> anyhow::Result<Reply> {
    let mut runs = st.store().list()?;
    let total = runs.len() as u64;
    runs.truncate(a.limit.unwrap_or(20));
    let started = st
        .jobs
        .all()
        .iter()
        .rev()
        .map(|j| ActiveRun {
            run_id: j.id.clone(),
            kind: j.kind,
            status: j.status(),
        })
        .collect();
    let out = ListRunsOut {
        runs,
        total_count: total,
        started,
    };
    let text = if out.runs.is_empty() {
        "no stored runs".to_owned()
    } else {
        out.runs
            .iter()
            .map(|r| {
                format!(
                    "{} {} {:.1} ops/s p99 {} — {}",
                    r.id,
                    r.verdict_level.as_str(),
                    r.ops_per_s,
                    fmt_ms(r.p99_ms),
                    r.headline
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    reply(&out, text)
}

fn load_run(st: &State, r: &str) -> anyhow::Result<crate::model::Run> {
    if let Some(job) = st.jobs.get(r)
        && !job.status().is_final()
    {
        return Err(OprobeError::new(
            ErrorCode::UsageUnknownRun,
            format!("run `{r}` is still {:?}", job.status()).to_lowercase(),
        )
        .with_hint("wait until get_run reports it finished")
        .into());
    }
    st.store().load(r)
}

fn explain_run(st: &Arc<State>, a: ExplainIn) -> anyhow::Result<Reply> {
    let run = load_run(st, &a.run)?;
    let e = explain::explain_run(&run, a.top.unwrap_or(3));
    let text = e.narrative.join("\n");
    Ok(Reply {
        structured: crate::output::envelope_value(&e, Vec::new()),
        text,
    })
}

fn analyze_run(st: &Arc<State>, a: AnalyzeIn) -> anyhow::Result<Reply> {
    let run = load_run(st, &a.run)?;
    if run.reqs.is_empty() {
        return Err(OprobeError::new(
            ErrorCode::UsageInvalidArgument,
            format!("run `{}` has no request rows to analyze", run.id()),
        )
        .with_hint("tune runs and report-only .json files keep no request rows")
        .into());
    }
    let selection = a
        .selection
        .unwrap_or_else(|| Selection::slowest(&run.reqs, a.slowest_pct.unwrap_or(1.0)));
    let attribution = outliers::attribute(&run.reqs, &selection);
    let factors_text: Vec<String> = attribution
        .factors
        .iter()
        .take(5)
        .map(explain::factor_sentence)
        .collect();
    let text = format!(
        "{} selected vs {} other requests{}",
        attribution.selected_count,
        attribution.rest_count,
        if factors_text.is_empty() {
            ": no distinctive attribute".to_owned()
        } else {
            format!(":\n- {}", factors_text.join("\n- "))
        }
    );
    let out = AnalyzeOut {
        run_id: run.id().to_owned(),
        selection,
        attribution,
        factors_text,
    };
    reply(&out, text)
}

fn compare_runs(st: &Arc<State>, a: CompareIn) -> anyhow::Result<Reply> {
    let base = load_run(st, &a.baseline)?;
    let cand = load_run(st, a.candidate.as_deref().unwrap_or("last"))?;
    let mut t = Thresholds::default();
    if let Some(x) = &a.thresholds {
        let set = |dst: &mut f64, v: Option<f64>| {
            if let Some(v) = v {
                *dst = v;
            }
        };
        set(&mut t.p99_pct, x.max_p99_increase_pct);
        set(&mut t.p50_pct, x.max_p50_increase_pct);
        set(&mut t.min_latency_delta_ms, x.min_latency_delta_ms);
        set(&mut t.throughput_drop_pct, x.max_throughput_drop_pct);
        set(&mut t.error_rate_pp, x.max_error_increase_pp);
        set(&mut t.cost_pct, x.max_cost_increase_pct);
        if let Some(v) = x.min_samples {
            t.min_samples = v;
        }
        if let Some(v) = x.allow_throttling {
            t.fail_on_new_throttling = !v;
        }
        if let Some(v) = x.allow_workload_change {
            t.allow_workload_change = v;
        }
        if let Some(v) = x.allow_target_change {
            t.allow_target_change = v;
        }
    }
    // The CLI's `compare`: the gate with significance applied (same verdict as
    // `oprobe compare` and the app).
    let d = crate::analysis::compare::compare(
        std::slice::from_ref(&base),
        std::slice::from_ref(&cand),
        base.id(),
        cand.id(),
        &t,
    );
    let text = format!(
        "{} → {}: {}",
        base.id(),
        cand.id(),
        if d.passed {
            "passed (no regression)"
        } else {
            "REGRESSION (gate failed)"
        }
    );
    reply(&d, text)
}

fn estimate_cost(st: &Arc<State>, a: PlanIn) -> anyhow::Result<Reply> {
    let p = prepare(st, &a)?;
    let price = crate::guard::usd_per_request(&p.profile);
    let provider = p.profile.cost.provider.clone().or_else(|| {
        crate::analysis::cost::detect_provider(
            &p.profile.target.scheme,
            p.profile.target.endpoint().as_deref(),
        )
        .map(str::to_owned)
    });
    let out = EstimateOut {
        applicable: price.is_some(),
        provider: price.and(provider),
        usd_per_request: price,
        estimated_cost_usd: p.plan.estimated_cost_usd,
        estimated_requests_count: p.plan.estimated_requests_count,
        estimated_bytes: p.plan.estimated_bytes,
        eta_s: p.plan.eta_s,
        max_cost_usd: p.budgets.max_cost_usd,
        within_budget: match (p.budgets.max_cost_usd, p.plan.estimated_cost_usd) {
            (Some(m), Some(c)) => Some(c <= m),
            _ => None,
        },
        assumptions: p.plan.assumptions.clone(),
    };
    let text = match out.estimated_cost_usd {
        Some(c) => format!(
            "~${c:.4} for ~{} requests ({})",
            out.estimated_requests_count,
            out.provider.as_deref().unwrap_or("list prices")
        ),
        None => "cost not applicable: no price table for this target (pass `provider`)".into(),
    };
    reply(&out, text)
}

async fn cleanup(st: &Arc<State>, a: CleanupIn) -> anyhow::Result<Reply> {
    let (p, _) = profile_of(st, &a.target, &WorkloadIn::default())?;
    check_target(st, &a.target, &p)?;
    let dry_run = !(a.confirm && st.cfg.allow_write);
    let mut args = vec!["cleanup".to_owned()];
    args.extend(target_cli(&a.target));
    args.extend([
        "--older-than".into(),
        a.older_than.clone().unwrap_or_else(|| "1h".into()),
    ]);
    if a.datasets {
        args.push("--datasets".into());
    }
    if dry_run {
        args.push("--dry-run".into());
    }
    let v = child_json(st, args, Duration::from_secs(600)).await?;
    let n = v["entries"].as_array().map(|e| e.len()).unwrap_or(0);
    let text = if dry_run {
        format!(
            "dry run: {n} leftover prefix(es) would be deleted{}",
            if a.confirm {
                " (this server is read-only: start it with --allow-write to delete)"
            } else {
                " (pass confirm=true to delete)"
            }
        )
    } else {
        format!("deleted {n} leftover prefix(es)")
    };
    Ok(Reply {
        structured: v,
        text,
    })
}

fn list_targets(st: &Arc<State>) -> anyhow::Result<Reply> {
    let ctx = st.ctx();
    let cfg = ctx.config()?;
    let targets: Vec<TargetOut> = cfg
        .targets
        .iter()
        .map(|(n, t)| TargetOut {
            name: n.clone(),
            url: t.url.clone(),
            region: t.region.clone(),
            endpoint: t.endpoint.clone(),
            credentials: t.credentials.clone(),
            options: crate::util::redact_options(&t.options)
                .into_iter()
                .collect(),
            allowed: allowed(&st.cfg, Some(cfg), n, entry_id(t).as_ref()),
        })
        .collect();
    let out = TargetsOut {
        targets,
        default_target: cfg.defaults.target.clone(),
        allow_targets: st.cfg.allow_targets.clone(),
        allow_write: st.cfg.allow_write,
        server_budgets: st.cfg.budgets.clone(),
    };
    let text = if out.targets.is_empty() {
        "no configured targets (pass URLs such as memory:// or s3://bucket/prefix)".to_owned()
    } else {
        out.targets
            .iter()
            .map(|t| {
                format!(
                    "{} {}{}",
                    t.name,
                    t.url,
                    if t.allowed { "" } else { " (not allowed)" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    reply(&out, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_unique_and_schemas_are_objects() {
        let mut seen = std::collections::BTreeSet::new();
        for s in SPECS {
            assert!(seen.insert(s.name), "duplicate {}", s.name);
            assert_eq!((s.input)()["type"], "object", "{}", s.name);
            assert_eq!((s.output)()["type"], "object", "{}", s.name);
        }
        assert_eq!(SPECS.len(), 14);
        let start = (spec("start_run").unwrap().input)();
        let props = start["properties"].as_object().unwrap();
        for k in ["target", "duration", "max_requests", "wait"] {
            assert!(props.contains_key(k), "{k}: {start:?}");
        }
        assert_eq!(start["required"], serde_json::json!(["target"]));
    }

    #[test]
    fn allowlist_matches_names_and_url_prefixes() {
        let mut cfg = McpConfig {
            allow_write: false,
            budgets: Budgets::default(),
            allow_targets: vec![],
            paths: crate::store::Paths {
                config_file: "c.toml".into(),
                data_dir: "d".into(),
            },
            max_concurrent_runs: 1,
        };
        let id = |u: &str| TargetId::parse(u).unwrap();
        let ok = |cfg: &McpConfig, given: &str| allowed(cfg, None, given, Some(&id(given)));
        assert!(allowed(&cfg, None, "anything", None));
        cfg.allow_targets = vec!["prod".into(), "s3://bench".into()];
        assert!(allowed(&cfg, None, "prod", None));
        assert!(ok(&cfg, "s3://bench/x"));
        assert!(ok(&cfg, "s3://bench"));
        assert!(!ok(&cfg, "s3://bench-evil/x"));
        assert!(!ok(&cfg, "memory://"));
        // Normalised on both sides (MCP-1): case, trailing slash, default port, `//`, `./`.
        assert!(ok(&cfg, "S3://BENCH/x/"));
        assert!(ok(&cfg, "s3://bench//x/./y"));
        assert!(!ok(&cfg, "s3://bench2/x"));
        cfg.allow_targets = vec!["S3://Bench/Data/".into()];
        assert!(ok(&cfg, "s3://bench/Data/x"));
        assert!(!ok(&cfg, "s3://bench/data/x"), "paths are case-sensitive");
        assert!(!ok(&cfg, "s3://bench/Database"));
    }

    fn allow_state(dir: &std::path::Path, allow: &[&str]) -> State {
        std::fs::write(
            dir.join("config.toml"),
            "[targets.prod]\nurl = \"s3://bench/data\"\nregion = \"eu-west-1\"\n",
        )
        .unwrap();
        State::new(McpConfig {
            allow_write: false,
            budgets: Budgets::default(),
            allow_targets: allow.iter().map(|s| s.to_string()).collect(),
            paths: crate::store::Paths {
                config_file: dir.join("config.toml"),
                data_dir: dir.join("data"),
            },
            max_concurrent_runs: 1,
        })
    }

    fn check(st: &State, t: TargetIn) -> Result<(), ErrorCode> {
        let (p, _) = profile_of(st, &t, &WorkloadIn::default()).unwrap();
        check_target(st, &t, &p).map_err(|e| crate::errors::classify(&e, "s3").code)
    }

    #[test]
    fn allowlisted_targets_cannot_be_retargeted_by_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let st = allow_state(dir.path(), &["prod", "s3://bench/"]);
        let t = |target: &str| TargetIn {
            target: target.into(),
            ..Default::default()
        };
        assert_eq!(check(&st, t("prod")), Ok(()));
        assert_eq!(check(&st, t("s3://bench/x")), Ok(()));
        // Region and anonymous never move the target.
        let mut r = t("prod");
        r.region = Some("us-east-1".into());
        r.anonymous = true;
        assert_eq!(check(&st, r), Ok(()));
        let refused = Err(ErrorCode::SafetyRefused);
        let mut e = t("prod");
        e.endpoint = Some("https://evil.example.com".into());
        assert_eq!(check(&st, e), refused);
        let mut o = t("s3://bench/x");
        o.options.insert("bucket".into(), "other".into());
        assert_eq!(check(&st, o), refused);
        let mut o = t("prod");
        o.options.insert("root".into(), "/elsewhere".into());
        assert_eq!(check(&st, o), refused);
        assert_eq!(
            check(&st, t("s3://bench/x?endpoint=https://evil.example.com")),
            refused
        );
        let mut px = t("prod");
        px.proxy = Some("http://evil.example.com:3128".into());
        assert_eq!(check(&st, px), refused);
        let mut i = t("prod");
        i.insecure = true;
        assert_eq!(check(&st, i), refused);
        assert_eq!(check(&st, t("s3://other/x")), refused);
        // Without an allowlist every override is the caller's business.
        let open = allow_state(dir.path(), &[]);
        let mut e = t("prod");
        e.endpoint = Some("https://elsewhere.example.com".into());
        assert_eq!(check(&open, e), Ok(()));
        // probe_endpoint: no proxy/insecure overrides under an allowlist.
        let probe = ProbeIn {
            endpoint: "s3://bench/x".into(),
            proxy: Some("http://p:1".into()),
            ..Default::default()
        };
        assert!(check_probe(&st.cfg, &probe).is_err());
    }

    /// SEC-1 / MCP-1: `..`, `//`, `./`, case, ports and `existing: ../` never escape an
    /// allowed location.
    #[test]
    fn allowlist_bypasses_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string().replace('\\', "/");
        let allowed_url = format!("fs://{root}/allowed/");
        let st = allow_state(dir.path(), &[allowed_url.as_str(), "s3://bench/oprobe"]);
        let t = |target: &str| TargetIn {
            target: target.into(),
            ..Default::default()
        };
        let refused = Err(ErrorCode::SafetyRefused);
        assert_eq!(check(&st, t(&format!("fs://{root}/allowed/run"))), Ok(()));
        assert_eq!(check(&st, t(&format!("fs://{root}/allowed"))), Ok(()));
        // The 3.0 bypass: a raw prefix test let this write into `other/`.
        assert_eq!(
            check(&st, t(&format!("fs://{root}/allowed/../other"))),
            refused
        );
        assert_eq!(
            check(&st, t(&format!("fs://{root}//allowed/./../other"))),
            refused
        );
        assert_eq!(check(&st, t(&format!("fs://{root}/allowed-evil"))), refused);
        assert_eq!(check(&st, t("s3://bench/oprobe/x")), Ok(()));
        assert_eq!(check(&st, t("S3://BENCH/oprobe/x/")), Ok(()));
        assert_eq!(check(&st, t("s3://bench//oprobe/./x")), Ok(()));
        assert_eq!(check(&st, t("s3://bench/oprobe/../prod")), refused);
        assert_eq!(check(&st, t("s3://bench/prod")), refused);
        // `existing` with `..` is refused; a prefix inside the root is fine.
        let with_existing = |target: &str, existing: &str| {
            let tin = t(target);
            let w = WorkloadIn {
                existing: Some(existing.into()),
                ..Default::default()
            };
            let (p, _) = profile_of(&st, &tin, &w).unwrap();
            check_target(&st, &tin, &p).map_err(|e| crate::errors::classify(&e, "s3").code)
        };
        assert_eq!(with_existing("s3://bench/oprobe", "tables/events/"), Ok(()));
        assert_eq!(with_existing("s3://bench/oprobe", "../../prod/"), refused);
        assert_eq!(with_existing("s3://bench/oprobe", "x/../../prod"), refused);
        assert_eq!(
            with_existing(&format!("fs://{root}/allowed"), "../other"),
            refused
        );
    }

    #[test]
    fn budgets_take_the_stricter_value() {
        let server = Budgets {
            max_requests_count: Some(100),
            max_duration_s: Some(10.0),
            ..Default::default()
        };
        let b = budgets_of(
            &server,
            &BudgetIn {
                max_requests: Some(1000),
                max_duration: Some("2s".into()),
                max_bytes: Some("1MiB".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(b.max_requests_count, Some(100));
        assert_eq!(b.max_duration_s, Some(2.0));
        assert_eq!(b.max_bytes, Some(1 << 20));
        assert!(b.max_cost_usd.is_none());
    }
}
