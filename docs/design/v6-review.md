# oprobe 5.0.0 deep review: evidence for 6.0

This is the evidence behind [v6.md](v6.md). It was reviewed on 2026-11-22 against main d619eee (the same tree as
the branch head 0136a61). There were four inputs:
- a new-user walkthrough of the released musl binary;
- a read-only architecture review;
- an experiment-driven trust re-test, using `lab://` scenarios and the independent fault proxy
  `r5a/fproxy.py` in front of an external lab;
- market research.

## P: Product and adoption

- **P1.** The repository was created on 2026-11-19. Since then there have been nine releases,
  including five major versions. Status: 0 stars, 0 forks, 0 issues from users, and 1 unique visitor
  in the traffic window. The only open PRs are from Dependabot. Downloads are 25–39 per release, which
  is consistent with CI and installer smoke tests.
- **P2.** Neither the test suite nor CI has ever run against a real cloud bucket. Real-backend CI
  uses MinIO, fake-gcs-server and Azurite. Every limit model was designed against oprobe's own lab and
  validated against that same lab or the scratchpad proxy.
- **P3.** The lab has no model for how real S3 throttles: per-prefix request-rate partitions that
  scale up over minutes. The advice "spread keys over more prefixes" is prose and is never measured.

## U: Walkthrough (released binary, `install.sh`, lab only)

- **U1.** `verify last throttling` on the demo reports **confirmed**, yet throughput fell 15.6 %
  (1413 → 1193 ops/s) and p99 did not move (−0.4 %). The only gain is that throttling went to zero.
  The verdict follows the finding's narrow prediction, not what the user wants.
- **U2.** `fix lab://demo` took 3 min 25 s.
  - Its own table shows c=24 at 0 % throttling and the best goodput (1448 ops/s).
  - The options list marks `concurrency=24` as the best choice.
  - The header and profile patch say `concurrency=23`.
  - The summary line reads "1433.2 → 1359.6 ops_per_s vs baseline (baseline infeasible); −6 % vs the
    best feasible trial".
- **U3.** The output is dense with jargon and repeats itself. For example, `net traded: …` is printed
  twice. The confidence interval "[−119.1 %, −80.9 %]" for a ratio that cannot drop below −100 % is
  impossible (see T9).
- **U4.** After `demo`, `verify` and `fix` (three user actions), `runs list` holds 14 runs. Experiment
  runs crowd out the runs the user made.
- **U5.** A run with `--client exec:…` recommends `tail_cut = true` (OpenDAL's TailCutLayer), but that
  client does not use OpenDAL. Its suggested `run --from` replays with the default OpenDAL client,
  because the client is not part of the spec.
- **U6.** `--client aws-sdk` is not in the shipped binaries ("built without the `aws-sdk` client").
  The adapters live only in the repository, so a user who installed with `install.sh` does not have
  them.
- **U7.** `install.sh --help` runs the install instead of printing help.
  - The embedded minisign key is empty, so the minisign fallback the README describes never runs.
  - Without `gh`, only the SHA-256 is checked, and that file comes from the same origin as the binary.
- **U8.** The CLI is 29 commands, with 77 options under `run --help-all`. MCP is 21 tools whose
  `tools/list` is 54 KB (about 14k tokens per agent session), plus 3 prompts and 12 resources.
- **U9.** The binary is 62 MB, against the v5 target of ≤ 45 MB.

## R: Architecture (summary of the full report)

- **R1.** Several v5 contract items are hollow:
  - The ≤ 2 µs/op overhead is not benchmarked. The self-report samples one hook in N, and
    `selfobs.rs:193` holds a stubbed `Some(1.5)`.
  - `report/view.rs` (332 lines) is barely shared: `html.rs` (2.1k lines) and `terminal.rs` (1.1k)
    still format every section themselves.
  - The Fisher test, the EWMA trend gate, `probe limits`/`probe semantics`, the ORC/Lance/Zarr/HLS
    patterns and Windows TCP info are all missing.
- **R2.** "App is the only entry point" is only partly true.
  - `crates/oprobe/src/lib.rs:10` re-exports all of `oprobe_core::*`.
  - MCP run and tune jobs call `engine::measure` directly (`mcp/jobs.rs:487-548`).
  - Front ends import `analysis` 42×, `store` 45× and `guard` 24×.
- **R3.** `oprobe-core` has no layering. Its modules form one dependency cycle (model ↔ analysis,
  engine ↔ tune, `errors` → engine, `guard` → app, `store` → distributed).
- **R4.** Publishing `oprobe-core` and `oprobe-trace` on crates.io is a semver hazard.
  - oprobe-core exposes 974 pub fns, 357 pub structs and 71 pub enums.
  - There are zero `#[non_exhaustive]` attributes.
  - The CLI's `ErrorCode` and exit codes live in `oprobe-trace` (`error.rs:11-62`), so every new CLI
    error code is a breaking change to the tracing library.
- **R5.** Some functions are very large:
  - `tune()`: 696 lines;
  - `render_with`: 615;
  - `handle_s3_request`: 609;
  - `diagnose`: 383;
  - `transport::fetch`: 379.

  `mcp/tools.rs` (3.4k lines) and `findings.rs` (2.7k) were not split as planned.
- **R6.** `ui/` is 16.6k lines, 56 % of the binary crate. It pulls in 167+ crates and the nsh toolkit,
  which is pre-1.0. It accounts for 4 of the 5 advisory waivers in `deny.toml`, and nsh is 20 % of
  `.text` against 10 % for oprobe_core.
- **R7.** The `aws-sdk` client (469 lines) and the boto3, awscrt and Go adapters (1k lines) have zero
  tests and are not built in CI.
- **R8.** Distributed load (3k lines) has been tested on localhost only. It cannot be used from MCP or
  with `--repeat`/`--baseline`.
- **R9.** There is duplication:
  - two OpenTelemetry stacks (the SDK in `otel.rs` and a hand-written encoder in `export/otlp.rs`);
  - two i18n layers;
  - a second HTTP transport;
  - an in-house cache layer alongside OpenDAL's `FoyerLayer`.
- **R10.** Tests:
  - 420 unit and 167 integration tests;
  - about 125 assertions on English prose;
  - 32 sleeps and several wall-clock bounds.

  The overhead, aws-sdk, the adapters, multiple hosts and the GUI are untested.
- **R11.** oprobe uses 3 of OpenDAL's ~23 layer features (Retry, Timeout, TailCut, plus await-tree
  for stacks). `ConcurrentLimitLayer`, `ThrottleLayer`, `FoyerLayer`, `ChaosLayer`, `RouteLayer` and
  the metrics/otel layers are unused.

## T: Trust re-test (4.0.2 findings and new issues)

Fixed, now trustworthy for a single process using the OpenDAL client:
- A1: a pure rps limit is now identified as `rate_limit` 397/400 with 99 % explained;
- A5: random 5xx is not mistaken for a limit;
- A7: hot prefix or key is identified as `key_limit` 95/100;
- A8: a bandwidth cap gets "do not add streams";
- A9: client CPU is detected;
- A2: verify is two-sided, and it refuted the noisy-neighbour advice;
- M2: whole-op sampling keeps 0 broken ops out of 378k.

Still open:

- **T1.** `run` advice keeps a fixed 15 % margin and a linear prediction. For example, on parquet it
  advises "c ≤ 4, predicted 131 ops/s" when the current rate is 259 (−49 %, for 0.17 % throttling),
  and rates that critical. Only `fix`/`tune` size the change correctly.
- **T2.** It fails on limits that do not throttle (queueing, plateaus). A single closed-loop run varies
  no load, yet oprobe gives confident advice: "raise concurrency (try 32/64)" or a linear prediction
  (147 predicted, 77 measured). On lab://queueing, three identical runs gave opposite advice
  (`none` → "try 32" twice, `queueing` → "run at ≈6" once).
- **T3.** Censored open-loop arrivals are dropped from the exact percentiles (`model/build.rs:43`).
  This brings back coordinated omission on every unsampled `--rate` run.
- **T4.** Tail attribution still says "ttfb" when 97 % of the slowest operations' time is retry
  backoff (A10).
- **T5.** `--workers local:2` blurs the rate fit: 343/346 instead of 397, still at high confidence.
  `cpu_cores_count` reads 8 on a 4-core host.
- **T6.** CPU used by an `exec:` client is invisible. A pinned adapter at 75 % CPU was reported as
  "server latency-bound, raise concurrency".
- **T7.** `client_saturated` (scheduling lag) does not feed the verdict, so a slow client can be
  diagnosed as an invented server queue.
- **T8.** Confidence labels are inflated: "high" was given to models explaining 40–49 % of the
  throttles.
- **T9.** Verify intervals for ratios can be impossible, for example "−100.0 % [−509.1 %, +309.1 %]".
- **T10.** Failures caused by throttling make the run "invalid: fix the failures", which suppresses
  the throttling findings that caused them.
- **T11.** Finding counts disagree with the run totals: "40900 requests" against 62837. Warmup still
  absorbs the connection dials (M15).
- **T12.** Memory: a long memory:// run plateaus at 120 MB, but opening a run of 1.1M requests takes
  618–897 MB (the target is < 300 MB).
- **No false positives** on clean targets (60 ms WAN, clean proxy).

## M: Market

- **M1.** Raw throughput benchmarking belongs to warp and elbencho, including multi-node runs.
  Distributed load in oprobe competes with them on their own ground.
- **M2.** Stage-by-stage request tracing from the client side, across clouds, has no peer:
  - CloudWatch and Storage Lens show only the server's view;
  - APM and OpenDAL's metrics layers record one span per operation;
  - eBPF tools (OBI) are the closest emerging overlap.
- **M3.** "Name the cause, then prove the fix with A/B" has no equivalent. warp `cmp` and k6 thresholds
  only compare numbers.
- **M4.** Agents and MCP: observability MCP servers (Datadog, CloudWatch, AWS DevOps Agent) *read*
  telemetry. No storage MCP server found can safely *run an experiment*.
- **M5.** Demand is concentrated where OpenDAL already is: iceberg-rust, Databend, GreptimeDB,
  RisingWave, Vector and sccache. Concrete pains:
  - Iceberg 503 SlowDown and prefix layout;
  - DuckDB S3 parquet reads and its 2026 rework of in-flight concurrency;
  - fixed GET TTFB in ML loaders.
- **M6.** The tools that won (warp, fio, iperf3, hyperfine, k6 OSS) are a CLI plus a report, cited in
  issues and CI. None ships a native GUI.
