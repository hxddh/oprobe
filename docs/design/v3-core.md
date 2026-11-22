# oprobe 3.0 — shared core APIs (stage A2)

How streams build on the core. The contract is [`v3.md`](v3.md) as amended by [`v4.md`](v4.md)
and [`v5.md`](v5.md); this page names the Rust entry points that implement it, with the 5.0
module paths.

## Module map

Paths as of 5.0 (design [v5 §1](v5.md#1-workspace-and-crates)): three crates in one workspace.
`oprobe-core` re-exports `oprobe_trace` as `trace` and `oprobe_trace::net` as `net`, and the
`oprobe` binary crate re-exports everything of `oprobe-core` at its root, so `crate::model`,
`crate::trace` etc. resolve the same way in every crate.

| Module (crate) | What it holds |
|---|---|
| `oprobe_trace` (`crates/oprobe-trace/src/lib.rs`) | `layer(Config) -> (TraceLayer, Handle)`, `Handle::snapshot()`, the `Exporter` and `Budget` hooks. |
| `trace::{recorder, scope, log, sample}` (`crates/oprobe-trace/src/`) | The always-on `Recorder`, op scopes, `TraceLog` (row cap `DEFAULT_MAX_REQUESTS` = 1 000 000), `TraceRows`, the whole-operation sampler `OpSampler`. The recorder feeds them from `Recorder::complete_op`. |
| `trace::{rows, record, snapshot}` | `ReqRow` / `OpRow` and their constructors, `RequestRecord`, the aggregates (`Summary`, `VerbStats`, `ApiStats`, `ConnStats`, `Quantiles`, …). |
| `net` (`crates/oprobe-trace/src/net/`) | TLS, proxies, dialing, the fidelity and microscope transports, tcpinfo, the handshake probe. |
| `errors` (`crates/oprobe-trace/src/error.rs`, re-exported with `classify` by `crates/oprobe-core/src/errors.rs`) | `ErrorCode` (stable dotted codes, `exit_code()`, `retryable()`), `ErrorBody`, `OprobeError`, `fail!`, `Classify::code`, `classify(&anyhow::Error, scheme)`, `exit::*`. |
| `app` (`crates/oprobe-core/src/app/`) | The service layer every front end calls: `App`, `Ctx`, typed requests (`app::input`, `RunRequest`), one module per command family (`run`, `tune`, `verify`, `fix`, `compare`, `runs`, `settings`, `doctor`, `watch`, …), profile assembly (`profile.rs`: target names + `[defaults]`), next steps (`next.rs`). |
| `model` (`crates/oprobe-core/src/model/`) | `RunId` (`ids.rs`), Report v5 types (`report.rs`, `sections.rs`), `Run` (report + rows), the run spec (`spec.rs`), the builder (`build.rs`: `build_run`, `build_report`, `ReportInput`, `workload_info`). |
| `store` (`crates/oprobe-core/src/store/`) | `Paths` (`OPROBE_CONFIG`, `OPROBE_DATA_DIR`, `dirs`), `ConfigFile` (`[defaults]`, `[targets.NAME]`, `effective()` with sources), `RunStore` (save / list / resolve / load / delete / gc), the run file (`runfile.rs`: `oprobe.run/5` segments, `RunFileWriter` / `RunFileReader`), `Baselines`, `atomic_write`, `resolve_run`. |
| `events` (`crates/oprobe-core/src/events.rs`) | `Event` / `EventKind` (`oprobe.events/1`), `EventBus` (in-process broadcast), `EventBus::progress_fn` (engine progress → events), `NdjsonWriter`. |
| `output` (`crates/oprobe-core/src/output/`) | `OutputFormat`, `CommandOutput` (typed result trait), `Output` (per command: `finish`, `error`, `info`, `warn`, `bus`; `Output::silent()` for MCP and the UI). |
| `guard` (`crates/oprobe-core/src/guard.rs`) | `Budgets`, `Guard` (live budgets), `BudgetBreach`, `check_read_only`, `plan` / `Plan` (`--dry-run`), `preflight`, `usd_per_request`. |
| `engine` (`crates/oprobe-core/src/engine/`) | `Session` (`runner.rs`), `measure::open`, operators, datasets, workloads, cleanup. |
| `analysis` (`crates/oprobe-core/src/analysis/`) | Findings, capacity and hypotheses, the compare gate (`gate.rs`), cost, hints, outliers, the finding catalog (`catalog.rs`). |
| `report` (`crates/oprobe-core/src/report/`) | `view.rs` (one presentation model per section), the terminal, brief, Markdown, HTML and compare renderers, `style` (colors, `term_width`, `wrap_text`). |
| `i18n` (`crates/oprobe-core/src/i18n.rs`) | The message catalog (English, 简体中文): `t`, `tf`, `detect`, `detect_os`, `finding`, `error_code`. Machine output never goes through it. |
| `export` (`crates/oprobe-core/src/export/`) | CSV, NDJSON, HAR, OTLP (JSON traces and metrics, the protobuf metrics encoder, `WatchMetrics`) and Prometheus (`Registry`). |
| `cli` (`crates/oprobe/src/cli/`) | The clap tree (`args.rs`), conversions to `app::input` requests (`convert.rs`), presentation (`commands.rs`), help tiers (`help.rs`), dynamic completions (`complete.rs`), `self_update.rs`, dispatch (`mod.rs`). |
| `mcp`, `ui` (`crates/oprobe/src/`) | The MCP server and the terminal and native workbench; both call `app` in-process. |

## A command returns typed output

```rust
#[derive(Serialize, JsonSchema)]
pub struct MyOutput { pub things_count: u64 }

impl CommandOutput for MyOutput {
    fn text(&self) -> String { format!("{} things\n", self.things_count) }
    // optional: markdown(), html(), exit_code(), result_event()
}

pub async fn cmd_mine(a: MyArgs, ctx: &Ctx) -> anyhow::Result<u8> {
    ctx.out.info("working…");                 // stderr, or a `log` event in ndjson mode
    ctx.out.finish(&MyOutput { things_count: 3 })  // prints per -o, returns the exit code
}
```

Put the work in an `app` module (`crates/oprobe-core/src/app/`) behind a typed request from
`app::input` (no clap types in `oprobe-core`), then wire the CLI: the arguments in
`crates/oprobe/src/cli/args.rs`, the conversion to the request in `cli/convert.rs` and the arm in
`cli::dispatch`. MCP tools and the UI call the same `App` method. stdout carries only the result; use
`ctx.out.info/warn` for anything else. `-o html` is a `usage.invalid_argument` error unless a command
implements `html()`.

## Raise a classified error

```rust
return Err(fail!(ErrorCode::UsageUnknownRun, "no run `{r}`"));
return Err(OprobeError::new(ErrorCode::SafetyRefused, "…").with_hint("…").into());
let op = something().code(ErrorCode::ConfigInvalid)?;   // errors::Classify
```

Unclassified `anyhow` errors are classified at the CLI boundary (`errors::classify`: an
`OprobeError` in the chain wins, then OpenDAL `ErrorKind`, dial/TLS/timeout sources, then known
messages) and get a hint from `analysis::hints`. Exit codes follow `ErrorCode::exit_code()`.
In json/ndjson modes the error is printed on stdout as `{"type":"error","error":{…}}`.

## Emit events

`ctx.out.bus()` is the command's `EventBus`; in `-o ndjson` mode an `NdjsonWriter` on stdout
is already subscribed and the `version` line written.

```rust
let progress = ctx.out.bus().progress_fn(Some(total_s)); // pass to Session::open_with / run
ctx.out.bus().emit(EventKind::Finding { finding });
ctx.out.bus().log(LogLevel::Warn, "…");
let rx = bus.channel();                                   // in-process consumer (TUI/GUI/MCP)
```

The final `result` event comes from `CommandOutput::result_event` via `Output::finish`.

## Run a measurement and store it

```rust
let opts = SessionOptions { run_prefix: Some(run_id.run_prefix()), guard, trace_max_requests, preflight: true };
let session = Session::open_with(profile, cancel, progress, opts).await?;   // preflight before any write
let snap = session.run(&knobs, &RunOptions::from_profile(&session.profile), Some(progress)).await?;
let cleanup = session.close().await;
let run = model::build_run(ReportInput { run_id: &run_id, snapshot: snap,
    trace: session.take_trace().unwrap_or_default(), /* profile, knobs, cleanup, … */ });
let path = ctx.store().save(&run)?;          // runs/<id>.oprobe + runs/<id>.json
```

Load: `ctx.store().load("last~1")?` or `store::read_run_file(path)`; resolve only:
`store.resolve(runref)` / `store::resolve_run(runref)` (default store). RUNREF = id, unique
prefix, `last`, `last~N`, label (newest), or a path (`.oprobe`, or a report `.json` without
rows). `Run::op(id)` / `Run::requests_of(id)` join rows; `report.slowest` holds op ids.

## Safety rails

`app::run` shows the order: `guard::check_read_only` (when `--read-only` / `--existing`),
`guard::plan` for `--dry-run`, `Guard::with_pricing(budgets, usd_per_request(&profile))`
passed in `SessionOptions` (the recorder feeds it every operation, the sampler ticks it; a
breach cancels the run, the command saves the partial run and returns
`BudgetBreach::to_error()` → exit 4), `model::ids::idempotent_run_prefix(key)` for
`--idempotency-key`.

## Report v3 notes

- Shared aggregates (`trace::{Summary, VerbStats, ApiStats, ConnStats, Quantiles, …}`) keep
  short Rust field names; their JSON names carry units via `#[serde(rename)]` (e.g.
  `Quantiles::p99` → `p99_ms`, `Summary::error_rate` → `error_ratio`). New types use unit
  names in Rust too. Dimensionless fractions use `_ratio`.
- `api` is a string everywhere (`ReqRow::api: Cow<'static, str>`, `trace::Api` serializes as
  its label). Stream B2 can feed OpenDAL `ServiceOperation` names into `RequestRecord` and
  `req_row` without changing the row format.
- `OpRow` adds `error` (detail) and `await_tree` (slow operations); `ReqRow` adds `error`.
- The tuner's report carries no trace rows (only its slowest operations).
- JSON Schema: `schemars::schema_for!(oprobe_core::model::Report)` (and `Run`, `events::Event`,
  `errors::ErrorBody`, every `CommandOutput`), registered in `crates/oprobe/src/mcp/schemas.rs`
  and published via `oprobe schema` and `schemas/`.

## Outlier attribution (shared by B1, B3, B4)

`analysis::outliers::{Selection, Attribution, Factor, attribute, dominant_stage}` is the stable
interface: `attribute(&run.reqs, &Selection::slowest(&run.reqs, 1.0))` ranks the attribute
values (api, status, protocol, connection cold/warm, remote IP, attempt, error, dominant stage)
over-represented among the selected requests. Stream B3 owns and refines the ranking; B1 (MCP
`analyze_run`/`explain_run`) and B4 (GUI brush on the timeline/heatmap) call it and must not
change its signatures.
