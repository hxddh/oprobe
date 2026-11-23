# oprobe 7 review: what 6.3.0 gets wrong, and the plan for 7.0

Status: proposal. Date: 2026-11-23. Basis: 6.3.0 (`3c08614`).

This review used three independent passes:
- a hands-on run of the CLI, from a newcomer, a backend engineer and a CI/agent point of view;
- an audit of observability coverage: what OpenDAL 0.59 and `oprobe-trace` can observe, compared with what the terminal, the JSON report and the app show;
- a productization pass checked against v6.md §1 ("what oprobe is not").

Every item cites code or output that was checked. Nothing here adds a command. The plan adds no flag, except one library method.

**Why a major version.** In embedded (production) mode, `oprobe-trace` sees only HTTP requests: no operations, keys, ranges or retries (B1). Fixing that changes what a trace means. Request counts stop being labelled "ops", and the trace file gains operation rows. That is a 7.0 change. Everything else is additive.

---

## Where 6.3.0 stands

These already work well. Keep them.
- **Time to first insight.** `oprobe demo` takes 12.5 s and names two findings, each with a remedy and a `next:` line. `oprobe prove last` then takes 60 s and gives a proven diff (+38 % goodput).
- **Errors.** Every error has a stable code, an exit code and a docs URL. Typos get a suggestion: in clap flags, in `--set` paths, in service options and in finding codes.
- **Safety.** Ctrl-C cleans up and saves a partial run. Budgets stop the run and save it. `--read-only` refuses with a useful hint.
- **App first run.** It starts with an empty state, then New run, pre-filled with `lab://demo`, then Enter. That is a result in two clicks with no CLI.

## 1. Correctness and trust bugs (fix first)

| # | Bug | Evidence | Fix |
|---|---|---|---|
| A1 | Failed requests store the URL in the trace, which can include an Azure SAS `?sig=` | `oprobe-trace/src/net/fidelity.rs:953` formats `{err}`; `without_url()` is applied only to the source | Format `err.without_url()`. Add a test with a `?sig=` URL |
| A2 | `prove` reports "improved" while B throttles 43× over `--max-throttle` | `app/prove.rs:1028` checks the limit only while sizing; `throttled 0.29 % → 4.36 %` | If B exceeds `max_throttle`, the verdict is `inconclusive`, with a note |
| A3 | Hints name a flag and a config table that do not exist (`--endpoint`, `[targets.NAME]`) | `config.rs:1114-1117`, `:1135`; a test pins the wrong hint | Use `--set options.endpoint=…` and `[operators.NAME.options]`; fix the test |
| A4 | An interrupted or invalid run ranks a low-confidence finding first and suggests `prove` | Ctrl-C after 4 s: finding #1 is `throttling … next: oprobe prove` | When `invalid_run` is present, put it first, drop the `next: prove` lines, and have `prove` refuse |
| A5 | A partial run shows **Critical**; the docs say **Review** | `--max-requests 500` gives `[Critical] Run invalid` | A budget stop or a partial run is Review |
| A6 | Double-clicking a `.oprobe` file fails on Windows and Linux | `oprobe C:\x.oprobe` gives `unrecognized subcommand`; the `.desktop` file has `Exec=oprobe` with no `%F` and no `MimeType=` | When the only argument is an existing `.oprobe` or `.json` file, open the app. Fix the `.desktop` file, add the MSI association, add a test |
| A7 | `run -o json` and the `--dry-run` plans have no schema; docs/cli.md says otherwise | `RunOutput` (`app/run.rs:25`) is not in `cli/schema.rs` | Register and bless `run` and `plan`; fix the sentence in the docs |
| A8 | The budget is off by one, and the dry-run plan ignores budgets | `501 requests (limit 500)`; the dry-run plans 6 541 requests with no warning | Stop at the limit. The plan states the budgets and warns when it will exceed one |

## 2. Usability: lower the barrier without adding surface

| # | Friction | Fix (no new concept) |
|---|---|---|
| U1 | The scheme list offers schemes that are not in this build. A pasted `https://bucket.s3.amazonaws.com/x` gives a raw "scheme is not registered" error | List only the compiled schemes. Map virtual-hosted and path-style S3, GCS and Azure URLs to the right `s3://` / `gs://` / `azblob://` form, or print it as a hint |
| U2 | Network and auth errors arrive as raw OpenDAL dumps, with the text duplicated | One plain cause first ("connection refused by 127.0.0.1:9999"); the context only in `-o json` or after it |
| U3 | "credentials rejected" does not say where the credentials came from, or which endpoint | Name the endpoint and the credential source ("from AWS_ACCESS_KEY_ID", "from ~/.aws/credentials") |
| U4 | The headline understates cost: "throttled 0.6 %" next to Critical | The headline states the cost: "54 % of operation time is spent in retry backoff after 429/503" |
| U5 | Unexplained terms (goodput, TTFB, stragglers, `±2.0 %`, `[+19.5, +51.1]`) | Say "95 % interval" where the interval is printed. Add a short glossary in concepts.md, linked from the `full profile:` line and from the app's Help menu |
| U6 | A warmup warning the user cannot act on; `--set warmup=` is undocumented | Name `--set warmup=…` in the warning, list the workload paths in the `--set` docs, and don't warn about prove's own trials |
| U7 | Setting up a real bucket means editing files. Settings shows only language and paths | Settings gets one "Your storage" line linking to `op.toml` and env credentials. The New run dialog accepts any URL and shows the plan (it already uses `App::plan`). The empty-state button says "Profile the lab" |
| U8 | A refusal to prove a production trace gives a generic hint | The trace knows the service (scheme, name, root), so print a concrete `oprobe run s3://name/root` |
| U9 | Output order is not deterministic (API order in the headline; confidence flips) | Sort by a fixed key. Make confidence thresholds use hysteresis, or round before comparing |
| U10 | CI users don't know the supported patterns | SKILL.md and the README "For scripts" section get the two patterns: `jq -e '.runs[0].report.verdict.level != "critical"'`, and `prove` exiting 3 when the result is worse |
| U11 | Small doc drift | `reading-the-profile.md` example; `--help` says bare `oprobe` opens the app; `prove --help` says RUN may be a path; Help ▸ Docs opens the installed version's tag |

## 3. Observability: show what is already captured

The capture side is rich, and the JSON report has nearly all of it. The terminal and the app each show a different subset, and some request-level signals are stored but never read. The rule for 7.0: **every captured signal is in the JSON report, and in at least one place a person looks, in the view where it explains something.** No new screens.

### B1. Embedded mode sees operations (the one capture gap)
- **Today.** `TraceLayer::apply_service` returns HTTP services unwrapped (`oprobe-trace/src/lib.rs:259-263`), so production traces have no verbs, op latency, op breakdown, keys, ranges or slow operations. Each request is counted as an "op".
- **Fix: wrap `Access` for every service.** The in-process path already has the code in `layer.rs`. This gives production the same Profile the lab gives.
- **Retry events.** Expose oprobe-trace's retry interceptor (`RetryLayer::with_notify`), so embedders' retries land in the Retries section.
- **Keys and ranges.** Record a hashed key prefix and the range length per request. This makes hot keys, prefixes, range sizes and over-read work for traces; `trace_file.rs:13-16` lists them as "not derived" today.
- **Labels.** Stop labelling request counts as "ops/s" or "failed ops" in trace reports.

### B2. Resilience in the terminal
`RetryView` exists but `render_with` never uses it. Add one KEY NUMBERS line, the same text as the app's Retries section: retries, backoff time, 429/503, the 4xx/5xx/network split, and the top error classes.

### B3. Honest charts
- **Waterfall.** It merges consumer stall into "download" (`view.rs:378`), while the share bars count stall as client time. Give stall its own segment; the `stage-stall` token already exists.
- **Goodput chart.** Throttling and errors are only on/off shading, so one 429 looks the same as a thousand. Plot `throttled_rate` and `errors_rate` as step lines on the Requests chart, next to retries.
- **Inflight chart.** `new_connections_count` is in `TimelineView` but never plotted. Draw it as a step line here; it explains latency spikes caused by connection churn. Add `cpu_pct` to `TimelineView` and draw it as a faint line, so client saturation over time is visible.
- **Hot keys evidence.** `HotKeys` and `Amplification` evidence charts return `None` (`plots.rs:411`). Use a horizontal bar of each key's share of throttling.

### B4. One set of numbers in both front ends
- **API table.** The terminal shows *mean* TTFB and transfer next to *p99* total. The app shows p50 and p99. Use p50 and p99 in both, and add per-API bytes to the terminal.
- **Verbs.** The app drops ops/s, MiB/s, bytes/op, retries, throttled % and req/op, which `VerbView` already computes. Add the four most useful as text columns: ops/s, MiB/s, throttled % and req/op.
- **Slices.** The app's slice table drops the error ratio. `cold_warm` slices are missing from SPREAD in both front ends.
- **Connections.** Delete `ConnectionView`, which no front end reads; the app re-derives `Conns`. Make it the one model, in line with the "one presentation model" rule.

### B5. Connections tell you whether the network or the server is the limit
Today the JSON report has these, and no one sees them. Add them as prop lines in the Connections section of both front ends:
- TCP kernel stats: rtt p50/p99, retransmitting connections, rwnd-limited and sndbuf-limited ratios, delivery rate;
- TLS: resumption ratio and HRR count;
- cold-start latency;
- proxy CONNECT in the app's dial phases.

### B6. Write path and request signals
- **Write path.** `effective_write` (chunk size, service min/max part) and the mean multipart part size are JSON-only, yet `prove` sizes those very knobs. Add one line to the Requests section and to Setup.
- **Request signals.** HTTP/2 `stream_wait_us`, `flow_stall_us` and `streams_count`, `upload_acked_us`, `send_buffered_us`, `cache_status` and `region` are stored in request rows (`rows.rs:201-239`) with no reader. Add them to the waterfall signals and to `show --by stage`. The terminal footnote at `terminal.rs:642-650` says the h2 flow-control stall is hidden in TTFB; these signals show it.
- **Error classes.** `FailureCode::from_status` folds 409, 412 and 416 into `internal`, and per-request rows have no temporary/persistent bit. Keep OpenDAL's error kinds (ConditionNotMatch, AlreadyExists, RangeNotSatisfied, plus temporary/persistent) in the rows, so the Retries section's "classes" are accurate for traces too.

### B7. Await trees in the app
Slow operations' await trees appear only in the terminal with `--detail`. The app's waterfall shows them as a disclosure under the operation.

## 4. Productization

### Must fix (none adds a concept)
- **P1. Publish what the docs promise.** `oprobe-trace` and `oprobe` are not on crates.io, and the Homebrew tap and the Scoop bucket return 404. The 6.3.0 release notes still advertise all three. This needs the owner to set `CARGO_REGISTRY_TOKEN`, `TAP_TOKEN` and `SCOOP_TOKEN`. Until then, `release.yml` prints a channel in the notes only when its secret is set, and install.md marks such rows "when published".
- **P2. Bounded memory in long-lived processes.**
  - **Today.** The trace timeline grows forever: after the first 10 minutes it keeps 10-second intervals, 8 640 a day (`recorder.rs:139-152`, `:1279`). Aggregates are cumulative since start.
  - **Fix: merge the oldest intervals** in pairs above about 2 000, so memory stays bounded and a month-long trace opens quickly.
  - **Fix: `Handle::save` can start a fresh window** (one library method), so hourly files are hourly profiles.
- **P3. Runs killed hard leave test data behind.**
  - **Today.** `engine/cleanup.rs::sweep` has no caller.
  - **Fix: a pending marker.** Write `<data>/pending/<id>` before the first write and remove it after a verified cleanup.
  - **Fix: clean up on the next run.** The next run against the same target deletes the pending prefixes it knows are its own, within the guard.
- **P4. Embedding compatibility stated.** docs/embedding.md says which OpenDAL (0.59), runtime (Tokio) and MSRV (1.97) are required. A wrong OpenDAL version otherwise shows up as opaque trait errors.
- **P5. The macOS bundle is sealed.** Run `codesign --force -s -` on `oprobe.app` after lipo. Add a CI step that sets quarantine and runs `spctl --assess` (still unsigned, but not "damaged").

### Nice to have
- **Show in Finder/Explorer.** A run's context menu gets it (Copy Run ID means nothing to a teammate). `show` prints a `file:` line.
- **An HTTP overhead benchmark.** Add one lab-S3 line to the overhead benchmark (today it measures only `memory://`).
- **A manual Help ▸ Check for Updates…** It reads `latest.json`, only when the user asks. privacy.md stays true.

### Explicitly not doing (v6.md §1)
- a CI regression gate, PR comments, JUnit output or a `compare` command;
- HTML or Markdown export (the `.oprobe` file is the shared artifact; A6 makes it open everywhere);
- OTLP spans or Prometheus beyond the existing `otel` metrics;
- sampling knobs (the overhead is 0.38 µs/op);
- background update checks or telemetry;
- native HDFS or SFTP (`webhdfs` covers HDFS);
- more UI languages;
- automatic data-dir retention.

---

## 5. Plan for 7.0

Four streams, in this order of risk.

| Stream | Items | Notes |
|---|---|---|
| **T: trust** | A1–A8 | Small, independent. Lands first; each fix with a test that fails on 6.3.0 |
| **E: embedded parity** | B1, P2, B6 error classes | The 7.0 reason. The trace file becomes `oprobe.trace/7`; 6.x trace files still open (read-compatible). Overhead budget stays ≤ 2 µs/op, with an HTTP benchmark line |
| **V: visibility** | B2–B7 | View-model first (`report/view.rs`), then the terminal and the app. JSON changes are additive. The app reads only `view.rs` |
| **U: usability and distribution** | U1–U11, P1, P3–P5, A6 packaging | Wording, defaults, docs, packaging. No new commands or flags |

**Done means:**
- `cargo test --workspace` and the convergence test pass;
- the schemas are re-blessed;
- `docs/cli.md`, `troubleshooting.md` and `llms-full.txt` are updated;
- the CHANGELOG has a `v7.0.0` section with a short migration note for trace consumers (`oprobe.trace/7`, request counts no longer labelled as operations).

## 6. Dependencies: what is new and what 7.0 uses

Checked on 2026-11-23 against crates.io.

| Dependency | From → to | Decision |
|---|---|---|
| nsh-* | 0.6.0 → 0.6.1 | **Upgrade.** Fixes: placement and full screen requested while the window is hidden now apply when it shows (our `keep_placement`); a list row can no longer read new data at an old index. File association already works in 0.6.0 (`AppOptions.file_types`, `next_open_files`). A6 is a bug in our argument parsing and packaging, not in nsh |
| OpenDAL | 0.59.5 (latest) | **Use `opendal-layer-observe-metrics-common` as the design reference for B1.** Its `MetricsLayer` wraps every service in `apply_service`, wraps the HTTP transport in `apply_context`, and records on cancellation. **Don't depend on it.** Its labels have no path, no operation-to-request link, and no range or retry data |
| opentelemetry | 0.31 → **0.32** (not 0.33) | OpenDAL 0.59's `opendal-layer-otelmetrics` pins 0.32. Embedders who use both need one opentelemetry version, so both write to the same global `Meter`. Keep the semconv name `http.client.request.duration`; OpenDAL's metric of a similar name measures a different interval. Add OpenDAL's label names (`scheme`, `namespace`, `root`, `operation`) as attributes, so dashboards can join on them. This is a public dependency of `oprobe-trace`, so it belongs in 7.0 |
| reqsign | aws-v4 3.3 → 3.5 (transitive) | **Upgrade.** Profiles with `role_arn` / `source_profile` now resolve (role chains). No API reports which provider supplied the credentials, so U3 derives the source from what oprobe can see itself: `AWS_*` variables, `AWS_PROFILE`, `options.*` |
| rustls-platform-verifier | 0.6 → 0.7 | **Upgrade.** reqwest 0.13 already uses 0.7, so today two copies are compiled, and the claim "the same TLS stack as OpenDAL's client" is false |
| sha2 0.11, zstd 0.14, dirs 7, rcgen 0.14, clap 4.6.9 | – | **Upgrade.** This removes duplicates; rcgen needs a one-line change in the lab |
| webpki-roots, rustls-native-certs | – | **Remove.** Nothing uses them |
| rand | 0.8 → 0.10 | **Skip.** The `SmallRng` seeding changed, so the same `--seed` would produce a different workload (docs/cli.md promises reproducibility). The only gain is removing duplicates |
| toml | 0.8 → 1.x | **Skip.** It brings nothing new, and the formatting of `op.toml` and of the TOML diff would change |
| Rust | MSRV 1.97; latest is 1.100 | **Keep MSRV 1.97.** Embedders depend on it (P4), and nothing new would delete real code |
