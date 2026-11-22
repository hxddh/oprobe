# AGENTS.md — working on the oprobe repository

Guidance for coding agents (and humans) changing oprobe itself. Agents *using* oprobe should
read [`docs/agents.md`](docs/agents.md); the binding architecture contract is
[`docs/design/v5.md`](docs/design/v5.md) (amending [`v4.md`](docs/design/v4.md) and [`v3.md`](docs/design/v3.md)), the shared core APIs
are in [`docs/design/v3-core.md`](docs/design/v3-core.md).

## Build and test

```sh
cargo build                                   # default features: CLI + terminal UI (tui)
cargo build --features gui                    # native windows (Linux needs libgtk-4-dev)
cargo test                                    # every crate: unit + integration tests
cargo test --test mcp                         # MCP server end to end (spawns the binary)
cargo test --test agent                       # JSON-only CLI workflow + schema drift check
OPROBE_BLESS=1 cargo test --test agent        # regenerate schemas/*.json and docs/agents.md codes
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --features gui --all-targets -- -D warnings
cargo package --workspace                     # crates.io readiness of all three crates
```

Tests never touch the user's files: integration tests set `OPROBE_DATA_DIR` and `OPROBE_CONFIG`
to a temporary directory. Network tests use `memory://` or the in-process S3 lab
(`oprobe::lab::LabServer`, `oprobe lab`); no cloud credentials are needed. Real-backend tests
(`crates/oprobe/tests/backends.rs`) run only when `OPROBE_IT_*` endpoints are set.

## Architecture map

A cargo workspace (design v5 §1): `crates/oprobe-trace` (embeddable instrumentation),
`crates/oprobe-core` (engine, analysis, store, service layer) and `crates/oprobe` (the binary).
Front ends call only `oprobe_core::app::App`; they never build clap structs or reach into
another front end.

| Path | What |
|---|---|
| `crates/oprobe-trace/src/lib.rs` | public API: `layer(Config) -> (TraceLayer, Handle)`, `Handle::snapshot()`, the `Exporter` hook |
| `crates/oprobe-trace/src/{recorder,scope,log,sample,snapshot,record,rows}.rs` | always-on recorder, op scopes, whole-op sampler, aggregates, `RequestRecord`, `ReqRow`/`OpRow` |
| `crates/oprobe-trace/src/net/`, `layer.rs`, `otel.rs` | fidelity and microscope transports, OpenDAL layers, OpenTelemetry metrics (feature `otel`) |
| `crates/oprobe-trace/src/error.rs` | stable dotted error codes and exit codes (re-exported by `oprobe_core::errors`) |
| `crates/oprobe-core/src/app/` | the service layer: `App`, typed requests (`input.rs`, `RunRequest`), one module per command family, profile assembly (`profile.rs`), next steps, explanations |
| `crates/oprobe-core/src/{clients,distributed}/` | `ClientChoice` and `WorkerSet` (stream SCALE) |
| `crates/oprobe-core/src/output/` | `-o text|json|ndjson|md|html`, the `CommandOutput` trait every result implements |
| `crates/oprobe-core/src/errors.rs` | classification of any error (`classify`), `fail!` |
| `crates/oprobe-core/src/events.rs` | the `oprobe.events/1` event stream (`EventBus`, NDJSON writer) |
| `crates/oprobe-core/src/guard.rs` | safety rails: budgets, `--dry-run` plan, read-only, preflight |
| `crates/oprobe-core/src/model/` | run ids, Report v5 (`report.rs`, `sections.rs`), the run spec, the report builder |
| `crates/oprobe-core/src/store/` | config file, targets, run store (`runfile.rs`: `oprobe.run/5` segments + sidecars), baselines |
| `crates/oprobe-core/src/engine/` | sessions, operators, datasets, operations, cleanup |
| `crates/oprobe-core/src/analysis/` | findings, capacity and hypotheses, the compare gate (`gate.rs`), cost, hints, outliers, finding catalog (`catalog.rs`) |
| `crates/oprobe-core/src/report/` | `view.rs` (one presentation model per section), text, Markdown, HTML and compare renderers |
| `crates/oprobe-core/src/tune/` | the throttle-aware tuner and code generation |
| `crates/oprobe-core/src/lab/` | in-process S3-compatible fault-injection server |
| `crates/oprobe/src/main.rs`, `src/cli/` | clap tree (`args.rs`), conversions to requests (`convert.rs`), presentation (`commands.rs`), dispatch; `cli/agent.rs` = `mcp`, `schema`, `capabilities` |
| `crates/oprobe/src/mcp/` | MCP server (`server.rs`, `tools.rs`, `jobs.rs`, `resources.rs`, `prompts.rs`), schema registry (`schemas.rs`), capabilities |
| `crates/oprobe/src/ui/` | terminal and native workbench (Native Shell) |
| `crates/oprobe/tests/` | integration tests (they spawn the binary) |
| `schemas/` | generated JSON Schemas (do not edit by hand) |
| `skills/oprobe/SKILL.md` | agent skill describing safe usage |

## Conventions

- **Machine output is a contract.** Every command returns a typed result (`Serialize +
  JsonSchema` + `CommandOutput`); stdout carries only the result, logs go through
  `ctx.out.info/warn`. Numeric fields carry their unit in the name (`_ms`, `_bytes`, `_ratio`,
  `_count`, …); enums are `snake_case`.
- **New machine output → register its schema** in `crates/oprobe/src/mcp/schemas.rs` and bless
  `schemas/` (`OPROBE_BLESS=1 cargo test --test agent`). The test fails when they drift.
- **New finding code → document it** in `crates/oprobe-core/src/analysis/catalog.rs` (a unit
  test scans `analysis/` for codes passed to `finding(` and fails on undocumented ones), then bless.
- **Errors are classified**: raise `OprobeError::new(ErrorCode::…, msg).with_hint(…)` or
  `fail!`; never invent ad-hoc exit codes.
- **Safety first**: anything that writes to a target goes through the guard (budgets,
  read-only, run prefix `oprobe-run-<id>/`, cleanup). The MCP server is read-only unless
  `--allow-write`.
- **MCP stdout is the protocol channel** — nothing in `mcp/` may print to stdout; tools call
  `App` in-process with a silent sink (`Output::silent()`), never child processes.
- Errors in prose: lowercase, no trailing period; hints are imperative.
- Commit messages: conventional style (`feat(mcp): …`, `fix(cli): …`, `test: …`, `docs: …`).
