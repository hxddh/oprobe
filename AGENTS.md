# AGENTS.md — working on the oprobe repository

This file is guidance for coding agents (and humans) who change oprobe itself.
- Agents that only *use* oprobe should read [`skills/oprobe/SKILL.md`](skills/oprobe/SKILL.md).
- The binding contract is [`docs/design/v6.md`](docs/design/v6.md), and §8 is the implementation
  contract. The older design documents (v3–v5) are history.

**oprobe 6 is a profiler for OpenDAL operators.** It has three parts:
- **a workbench**: the nsh app, and the CLI `run`/`prove`/`show`/`lab`;
- **a layer**: `oprobe-trace`;
- **a file**: `.oprobe`.

It follows one loop: **Profile → Explain → Prove**. Every finding names exactly one OpenDAL remedy,
which is an `OperatorSpec` path. Only `prove` gives a number for that remedy.

## Build and test

```sh
cargo build                                        # default features: app (nsh, terminal) + cli (cloud services)
cargo build --features gui                         # native windows (Linux needs libgtk-4-dev)
cargo build --no-default-features --features cli   # the CLI-only build
cargo test --workspace                             # unit + integration tests (or: cargo nextest run --workspace --all-targets)
cargo test -p oprobe --test convergence            # prove against every lab fault (~2 min)
OPROBE_BLESS=1 cargo test -p oprobe --test cli     # regenerate schemas/*.json
OPROBE_BLESS=1 cargo test -p oprobe --test docs    # regenerate llms-full.txt
cargo bench -p oprobe-trace --bench overhead       # oprobe-trace overhead (budget 2 µs/op)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --features gui --all-targets -- -D warnings
cargo package --workspace
```

**Isolation.** Tests never touch the user's files: integration tests point `OPROBE_DATA_DIR` and
`OPROBE_CONFIG` at a temporary directory.

**Network.** Network tests use `memory://` or the in-process S3 lab (`oprobe::lab::LabServer`,
`oprobe lab`), so no cloud credentials are needed. The real-backend tests
(`crates/oprobe/tests/backends.rs`) run only when the `OPROBE_IT_*` endpoints are set; CI sets them
for MinIO, fake-gcs-server and Azurite.

## Architecture map

The workspace has three crates. Front ends call only `oprobe_core::app::App`.

| Path | What |
|---|---|
| `crates/oprobe-trace/` | The published library: `layer(Config) -> (TraceLayer, Handle)`, `Handle::snapshot/save` (`oprobe.trace/6` files), the instrumented `HttpFetch`, the `Exporter` hook, OpenTelemetry metrics (feature `otel`). Internals are public only under the hidden `__internal` feature that oprobe-core enables. |
| `crates/oprobe-core/src/operator/` | `OperatorSpec` (`op.toml`, `oprobe.operator/6`). `build.rs` builds the layer stack; `diff.rs` renders a diff as Rust and TOML; `resolve.rs` resolves the OPERATOR argument. |
| `crates/oprobe-core/src/app/` | The service layer: `plan`, `run` (1..n operators), `prove`, `show`, `list`, `rm`, `lab`. |
| `crates/oprobe-core/src/engine/` | Sessions, datasets, the runner and its schedule, the four workload presets, cleanup. |
| `crates/oprobe-core/src/analysis/` | Findings, each with a `remedy` (`findings.rs`); limit fitting (`limits.rs`); statistics and intervals (`stats.rs`); the finding catalog (`catalog.rs`). |
| `crates/oprobe-core/src/model/` | Run ids, the report (`oprobe.report/6`), the run spec, the report builder. |
| `crates/oprobe-core/src/report/` | `view.rs` (the one presentation model used by the CLI and the app) and the terminal renderer. |
| `crates/oprobe-core/src/store/` | The config file, the run store (`runfile.rs`: `oprobe.run/6`), prove results and their child trials. |
| `crates/oprobe-core/src/{guard,errors,events,output}.rs` | Safety rails; error classification (`ErrorCode`, exit codes); the event bus; output formats (`text`, `json`). |
| `crates/oprobe-core/src/lab/` | The in-process S3-compatible fault-injection server. |
| `crates/oprobe/src/cli/` | The clap tree (`args.rs`), presentation (`commands.rs`) and the schema registry (`schema.rs`). |
| `crates/oprobe/src/launch.rs`, `src/ui/` | Bare `oprobe` opens the nsh app: a sidebar of runs, a toolbar in the title bar, menus (`ui/menus.rs`), and Profile, Operator or Compare for the selection (design: `docs/design/app-native.md`). Its zh/en catalog is `ui/i18n.rs`, and `ui/data.rs` is its one adapter onto core. |
| `crates/oprobe/tests/` | Integration tests (they spawn the binary); `ui.rs` runs nsh headless. |
| `schemas/` | Generated JSON Schemas: `report`, `prove`, `run-list`, `operator`, `error`, `events`. Do not edit them by hand. |

## Conventions

- **Keep it small.** Before you add a command, flag, finding or output format, check §1 of v6.md
  ("what oprobe is not"). The answer is usually no.
- **One finding, one remedy.**
  - A new finding code needs an `OperatorSpec` remedy path (or an explicit "not a layer").
  - It needs an entry in `analysis/catalog.rs`; a unit test fails on undocumented codes.
  - `run` never prints a size; `prove` sizes the remedy and proves it.
- **Machine output is a contract.**
  - Every result is `Serialize + JsonSchema`. stdout carries only the result; logs go to stderr.
  - Numeric fields carry their unit in the name (`_ms`, `_bytes`, `_ratio`, `_count`), and enums are
    `snake_case`.
  - A new output type is registered in `crates/oprobe/src/cli/schema.rs` and blessed.
- **Errors are classified.** Raise `OprobeError::new(ErrorCode::…, msg).with_hint(…)` or use `fail!`.
  `docs/troubleshooting.md` has one section per `ErrorCode`, and a test enforces that.
- **Safety first.** Anything that writes to a target goes through the guard: budgets, read-only,
  the run prefix `oprobe-run-<id>/`, cleanup.
- **The app has no logic of its own.** It calls `App` and reads `report/view.rs`. User-facing app
  strings live in both languages in `ui/i18n.rs`.
- **Docs.** `docs/cli.md` must name every command and flag (checked by a test). Hints never mention
  commands that do not exist.
- **Prose style.** Errors are lowercase, with no trailing period. Hints are imperative.
- **Commit messages** use the conventional style (`feat(prove): …`, `fix(app): …`, `test: …`,
  `docs: …`).
