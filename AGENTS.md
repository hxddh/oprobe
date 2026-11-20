# AGENTS.md — working on the oprobe repository

Guidance for coding agents (and humans) changing oprobe itself. Agents *using* oprobe should
read [`docs/agents.md`](docs/agents.md); the binding architecture contract is
[`docs/design/v3.md`](docs/design/v3.md), the shared core APIs are in
[`docs/design/v3-core.md`](docs/design/v3-core.md).

## Build and test

```sh
cargo build                                   # default features: CLI + terminal UI (tui)
cargo build --features gui                    # native windows (Linux needs libgtk-4-dev)
cargo test                                    # unit + integration tests (tests/*.rs)
cargo test --test mcp                         # MCP server end to end (spawns the binary)
cargo test --test agent                       # JSON-only CLI workflow + schema drift check
OPROBE_BLESS=1 cargo test --test agent        # regenerate schemas/*.json and docs/agents.md codes
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo clippy --features gui --all-targets -- -D warnings
```

Tests never touch the user's files: integration tests set `OPROBE_DATA_DIR` and `OPROBE_CONFIG`
to a temporary directory. Network tests use `memory://` or the in-process S3 lab
(`oprobe::lab::LabServer`, `oprobe lab`); no cloud credentials are needed. Real-backend tests
(`tests/backends.rs`) run only when `OPROBE_IT_*` endpoints are set.

## Architecture map

| Path | What |
|---|---|
| `src/main.rs`, `src/cli/` | the command tree (`args.rs`), `Ctx`, dispatch, one module per command family; `cli/agent.rs` = `mcp`, `schema`, `capabilities`, `explain` |
| `src/output/` | `-o text|json|ndjson|md|html`, the `CommandOutput` trait every result implements |
| `src/errors.rs` | stable dotted error codes, exit codes, classification of any error |
| `src/events.rs` | the `oprobe.events/1` event stream (`EventBus`, NDJSON writer) |
| `src/guard.rs` | safety rails: budgets, `--dry-run` plan, read-only, preflight |
| `src/model/` | run ids, trace rows (`ReqRow`, `OpRow`), Report v3, the report builder |
| `src/store/` | config file, targets, run store (`.oprobe` files + sidecars), baselines |
| `src/engine/` | sessions, operators, datasets, operations, cleanup |
| `src/net/`, `src/layers/`, `src/trace/` | instrumented transport, OpenDAL layers, recorder and aggregates |
| `src/analysis/` | diagnosis (findings), cost model, hints, outlier attribution |
| `src/report/` | text, Markdown and compare renderers |
| `src/tune/` | the throttle-aware tuner and code generation |
| `src/mcp/` | MCP server (`server.rs`, `tools.rs`, `jobs.rs`, `resources.rs`, `prompts.rs`), schema registry (`schemas.rs`), capabilities, finding catalog (`catalog.rs`), explanations (`explain.rs`) |
| `src/lab.rs` | in-process S3-compatible fault-injection server |
| `src/ui/` | terminal and native workbench (Native Shell) |
| `schemas/` | generated JSON Schemas (do not edit by hand) |
| `skills/oprobe/SKILL.md` | agent skill describing safe usage |

## Conventions

- **Machine output is a contract.** Every command returns a typed result (`Serialize +
  JsonSchema` + `CommandOutput`); stdout carries only the result, logs go through
  `ctx.out.info/warn`. Numeric fields carry their unit in the name (`_ms`, `_bytes`, `_ratio`,
  `_count`, …); enums are `snake_case`.
- **New machine output → register its schema** in `src/mcp/schemas.rs` and bless
  `schemas/` (`OPROBE_BLESS=1 cargo test --test agent`). The test fails when they drift.
- **New finding code → document it** in `src/mcp/catalog.rs` (a unit test scans
  `src/analysis/` for codes passed to `finding(` and fails on undocumented ones), then bless.
- **Errors are classified**: raise `OprobeError::new(ErrorCode::…, msg).with_hint(…)` or
  `fail!`; never invent ad-hoc exit codes.
- **Safety first**: anything that writes to a target goes through the guard (budgets,
  read-only, run prefix `oprobe-run-<id>/`, cleanup). The MCP server is read-only unless
  `--allow-write`.
- **MCP stdout is the protocol channel** — nothing in `src/mcp/` may print to stdout; tools that
  reuse CLI commands run them as child processes with `-o json`.
- Errors in prose: lowercase, no trailing period; hints are imperative.
- Commit messages: conventional style (`feat(mcp): …`, `fix(cli): …`, `test: …`, `docs: …`).
