# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## v6.2.0 — 2026-11-23

### Changed

- **The app is a native window** ([design](docs/design/app-native.md)). The row of tabs and the
  bordered cards are gone:
  - a sidebar of runs grouped by operator, on the platform's sidebar material; select runs with a
    click, mod-click and shift-click, and two or more show Compare;
  - the title bar holds the toolbar: Profile/Operator, New Run, Run Again/Stop, and the main menu
    on Windows and Linux;
  - menus with the platform's shortcuts (the menu bar on macOS), and a context menu on runs;
  - New Run, Settings (the language) and Delete are dialogs;
  - progress shows on the Dock or taskbar; open a run file or drop it on the window;
  - control sizes follow each platform, headings are in sentence case, and Operator's settings
    are grouped.

### Added

- **Profile shows what the run measured, not only its verdict**:
  - requests by S3 API: count, status codes, TTFB p50 / p99, p99, bytes, reuse;
  - retries and failures: retries, backoff time, 429 / 503, the longest `Retry-After`, failure
    classes;
  - requests in flight and p99.9 on the timeline, and p99.9 among the key numbers;
  - **Details**: byte amplification, the hottest keys, object sizes, prefixes and server
    addresses;
  - **Run setup**: workload, load, dataset, mix, seed, client health, rows kept, and the command
    that runs it again;
  - a note up front when the numbers are less certain (a saturated client, a partial run, late
    arrivals, few operations).
- Evidence for more findings: hot keys (`key_limit`, `repeated_reads`) and byte amplification
  (`small_ranges`). Core view model: `SetupView`, `TrustView`, `ApiRow`, `RetryView`,
  `BytesView`, `SpreadView`.

### Fixed

- The request waterfall draws the wait before each retry. In a throttled run it was blank,
  though it was most of the time.
- Compare shows a change of a share (errors, throttled) in percentage points: from 0.08 % to
  0 % is −0.08 pp, not −100 %. It also notes that each column is a single run.
- Findings come before the stage split. The key numbers say what they are a share of (failed per
  operation, throttled per request) and how many operations were measured.

- `prove`: the noise floor is never below what a trial can resolve, `ln(1 + 1/ops)` for its
  fewest operations. A/A trials of a few dozen operations that happened to agree exactly let a
  one-operation difference read as a significant change (a `worse` verdict from rounding).

## v6.1.0 — 2026-11-23

### Added

- **The app draws its evidence.** Each finding carries the chart that shows it: the share throttled
  at each number of requests in flight with the fitted limit's interval (`throttling`,
  `key_limit`), throttled or failed operations per second (`rate_limit`, `errors`), bandwidth over
  time (`bandwidth_cap`), the latency distribution (`tail_stragglers`, `hung_requests`).
- **Prove is drawn as it converges**: each A/B pair's change, the running estimate with its 95 %
  interval, and the A/A noise floor around no change.
- Profile: the latency distribution (CDF) with p50 and p99 marked, the stage split of each verb,
  and the throttled and failed seconds shaded under the goodput timeline.
- Compare: goodput over time and the latency distributions of every run on one chart each.
- Core view model: `LatencyCdf`, `VerbView`, `ConcurrencyView`, `EvidenceChart`, and
  `prove::convergence` (the interval after each pair, the same computation as the verdict's).

### Fixed

- Linux: the window opens when the app starts from its desktop entry. GTK 4 unsets
  `DESKTOP_STARTUP_ID` and `XDG_ACTIVATION_TOKEN` before `main`, so the launch was taken for a
  script and the help was printed instead.

## v6.0.0 — 2026-11-22

Design [v6](docs/design/v6.md), from the review in [v6-review.md](docs/design/v6-review.md).
oprobe 6 is **a profiler for OpenDAL operators**: one workbench (the app and four CLI commands),
one layer (`oprobe-trace`) and one file (`.oprobe`). It is a deep cut, built around one loop.
Everything under **Changed** and **Removed** is breaking; 5.x run files are not read.

### The new model

- **The operator is the unit.** An OpenDAL `Operator` (a service, a layer stack, read/write and
  HTTP options) is given as a URL, an operator file (`op.toml`, `oprobe.operator/6`) or a
  configured name (`[operators.NAME]`). `OperatorSpec` replaces targets, knobs and client
  stacks; `--set PATH=VALUE` changes one value, and the difference between two operators renders
  as Rust and TOML.
- **Profile → Explain → Prove.** `run` profiles one or more operators on one workload and
  explains each finding with its evidence and exactly one OpenDAL remedy (an `op.toml` path) and
  no number. `prove` is the only source of numbers: it sizes the remedy, runs A and B
  interleaved with a sequential test against a learned noise floor, and decides
  `improved | worse | inconclusive` on your objective (goodput by default, or p99). The result is
  a proven layer diff.
- **Findings use OpenDAL's layers:** `ConcurrentLimitLayer`, `RetryLayer` with jitter,
  `ThrottleLayer`, `TailCutLayer`, `TimeoutLayer`, `FoyerLayer`, read/write chunking and the
  `HttpFetch` settings. The catalog is `throttling`, `rate_limit`, `key_limit`, `bandwidth_cap`,
  `tail_stragglers`, `hung_requests`, `small_ranges`, `slow_multipart`, `repeated_reads`,
  `cold_connections`, `client_bound`, `errors` and `invalid_run`.
- **Four workload presets** (`small-files`, `large-objects`, `range-reads`, `list-stat`) replace
  the five of 5.0.

### Changed

- **CLI: four commands** (29 in 5.0): `oprobe run OPERATOR…`, `oprobe prove RUN [FINDING]`,
  `oprobe show [RUN|FILE]` (with `--list`, `--rm`, `--detail`, `--by`) and `oprobe lab
  [SCENARIO]`; `oprobe demo` is `oprobe run lab://demo`, and a bare `oprobe` opens the app.
  Global flags: `-o text|json`, `-q`, `--color`, `--non-interactive`.
- **Exit code 3** means that `prove` refuted the change (worse on the objective); 0, 1, 2, 4, 5
  and 130 are unchanged.
- **The app** is rebuilt on nsh with four views (Runs, Profile, Operator, Compare); in the
  Operator view a toggled layer is proven live. It speaks 简体中文 and English; the CLI and JSON
  output are English only.
- **Files and schemas:** `oprobe.run/6` and `oprobe.report/6`; `oprobe-trace` writes
  `oprobe.trace/6`. Six schemas are published (`report`, `prove`, `run-list`, `operator`,
  `error`, `events`).
- **Prove trials** are saved as children of their prove run and hidden from `show --list`.
- **Trust fixes:** censored open-loop arrivals count in every percentile; tail attribution
  follows the stage split (retry backoff is not "TTFB"); a saturated client feeds the verdict;
  "high" confidence requires the fit to explain most of the effect; intervals stay in range;
  throttle-driven failures are reported as throttling, not as an invalid run; counts agree with
  totals.
- **`oprobe-trace` is slim and stable.** The API is `layer(Config) -> (TraceLayer, Handle)`,
  `Handle::snapshot()`, `Handle::save(path)` / `Snapshot::save(path)` (a `.oprobe` profile that
  `oprobe show FILE` opens) and the `Exporter` hook; `Config` is a builder and public types are
  `#[non_exhaustive]`. The CLI's error codes and exit codes moved to `oprobe-core`; trace rows
  carry `FailureCode`. `schemars` is optional; the probe code, the microscope dialer, hyper and
  the root-store builder are gone. A real overhead benchmark
  (`cargo bench -p oprobe-trace --bench overhead`) replaces the self-reported number; the budget
  is 2 µs per operation.
- **Crates:** `oprobe-trace` is the published API. `oprobe-core` is still published, only so
  that `cargo install oprobe` works; it is documented as internal with no semver promise, and
  the crates pin each other with exact versions.
- **Releases:** every platform gets the app build and a CLI-only build
  (`oprobe-cli-<ver>-<target>` archives; `--no-default-features --features cli`). `install.sh
  --cli` / `install.ps1 -Cli` install it; `install.sh --help` prints help instead of installing.
- **`prove` needs at least 3 A/B pairs to decide:** with fewer (a small `--budget`), the verdict
  is `inconclusive` with a note; `--pairs` is at least 3. The objective's interval uses Student's
  t for the number of pairs, and a noise floor from a single A/A difference is doubled.
- **A budget breach mid-prove keeps the partial result:** the prove stops, prints and saves what
  it measured (`budget_exceeded: true`) and exits 4; its trials stay removable with `show --rm`.
- **`layers.tail_cut` carries every parameter oprobe applies:** `percentile` (50..99),
  `safety_factor` (1.5), `min_samples` (32), `min_deadline` ("50ms"), `max_deadline` (the
  operation timeout) and `window` ("60s"). The measured layer and the generated Rust are built
  from one parameter list, and the Rust spells out every value that differs from OpenDAL's
  defaults, so pasted code behaves as measured.
- **Docs** are rewritten around the loop: getting started, concepts, `op.toml`, the CLI,
  reading the profile, embedding, the app, install, troubleshooting, privacy and releasing.

### Removed

- **Commands:** `tune` (and its Bayesian optimiser), `verify` and `fix` as commands (they are
  `prove` now), `compare`, `baseline`, `watch`, `matrix`, `compat`, `doctor`, `probe`, `target`,
  `analyze`, `explain`, `runs` (now `show`), `top`/`gui` (now a bare `oprobe`), `config`,
  `cleanup`, `schema`, `capabilities`, `self-update`, `completions` and `man`.
- **The MCP server** (`oprobe mcp`, 21 tools, prompts and resources). Agents use `-o json` and
  the skill in `skills/oprobe/SKILL.md`.
- **The GitHub Action** (`action.yml`), baselines, noise-floor caching for CI, JUnit and PR
  comments.
- **Distributed workers**, the SDK clients (`aws-sdk`, `exec:`) and the language adapters.
- **The cost model** and price tables.
- **Output formats:** HTML, Markdown, NDJSON (the event stream on stdout), CSV, HAR and OTLP
  export; `-o` is `text` or `json`.
- **The microscope transport** (oprobe's own HTTP pool) and the in-house cache layer
  (`FoyerLayer` replaces it).
- **Internationalisation of the CLI** (`--lang`); the app keeps its own catalog.
- **Profiles** (`profiles/*.toml`), shell completions and man pages in the packages; the MSI's
  `.oprobe` file association and `oprobe://` links.
- **The minisign claim:** the README no longer promises a signature check; the installers verify
  a minisign signature only for releases that embed a public key.
- Docs: `ci.md`, `cost.md`, `agents.md`, `metrics.md`, `tuning.md`, `workloads.md`,
  `reading-the-report.md` (what is still true moved into the new pages).

### Known limitations

- **Validated without cloud credentials.** Every finding and every `prove` verdict has been
  checked against the lab, a fault-injecting proxy, MinIO, fake-gcs-server and Azurite — not
  yet against AWS S3, R2, GCS or Azure. Results name the environment they were measured in.
- **Throttling on OpenDAL's default retry** (1 s backoff, no jitter): a concurrency cap removes
  the throttling but often leaves goodput unchanged, so `prove` reports *inconclusive* on
  goodput; use `--objective p99` to judge such changes on latency.
- **`bandwidth_cap` on reads:** OpenDAL 0.59's `ThrottleLayer` shapes writes only, so `prove`
  sizes the cap from the fitted limit but cannot show a read-side gain.
- **`key_limit`'s remedy** is a change to the key layout, not a layer; `prove` tests a client-side
  cap as a stand-in and says so.
- **Production profiles** (`oprobe.trace/6`) hold requests, not operations: each request counts
  as one operation, findings that need operations or load variation are not derived, and
  `prove` refuses them.
- **`client_bound`** fires when the scheduler lags more than 5 ms at p99; on a busy shared
  machine it can lead the brief. Latency findings whose delay dwarfs that lag are kept.

## v5.0.0 — 2026-11-22

Design [v5](docs/design/v5.md). The changes under **Changed** are breaking.

### Changed

- **Workspace split.** The repository is a cargo workspace of three crates:
  `oprobe-trace` (the embeddable instrumentation: `TraceLayer`, the fidelity and microscope
  transports, `ReqRow`/`OpRow`, the recorder and aggregates), `oprobe-core` (engine, analysis,
  model, store, tune, lab, report renderers and the service layer) and `oprobe` (the binary:
  CLI, MCP server, TUI/GUI). The `oprobe` library no longer exports the engine under its own
  modules for embedding; depend on `oprobe-trace` or `oprobe-core`.
- **Service layer.** `oprobe_core::app::App` is the only entry point of the front ends, with
  typed requests and results (plan, run, tune, verify, fix, compare, explain, analyze, doctor,
  sample, discover, targets, runs, baselines, cleanup). `RunRequest` is a `RunSpec` plus
  `Overrides`, safety (budgets, read-only, allow-write), a `ClientChoice` and an optional
  `WorkerSet`.
- **MCP runs in-process.** `doctor`, `probe_endpoint`, `cleanup`, `verify_finding` and
  `recommend_config` call `App` instead of spawning `oprobe` child processes; a read-only server
  passes `allow_write: false` instead of `OPROBE_MCP_READ_ONLY`.
- **Report `oprobe.report/5`.** New sections: `client` (what generated the load; the 4.x
  `client` CPU/RSS block moved to `self.process`), `self`, `schedule`, `workers`, `dimensions`,
  `sampling`, and `capacity.hypotheses` (today's in-flight ceiling is one `inflight_limit`
  hypothesis). `compare` and `verify` gain an optional two-sided `net` verdict.
- **Run file `oprobe.run/5`.** A zstd stream of framed blocks: `header` (spec and client),
  columnar `segment`s of op and request rows, per-second `interval`s and the `report` last,
  streamed to disk (never encoded whole in memory). 4.x run files are not read.
- **Whole-operation sampling.** Beyond the row budget an operation is kept with all its
  requests or not at all: a time-stratified reservoir plus every failed or throttled operation
  and the slowest 1 % (`report.sampling`).
- **Always-on recorder.** `oprobe_trace::layer(Config) -> (TraceLayer, Handle)` instruments any
  OpenDAL operator; requests outside an operation scope are recorded in every shard. The `otel`
  feature emits `http.client.request.duration` with `oprobe.stage` and `oprobe.api`.
- The compare gate moved from `report/diff.rs` to `analysis/gate.rs`; `report/view.rs` is the
  shared presentation model of the terminal, Markdown and HTML renderers. The lab is a module
  directory (`lab/`), the finding catalog lives in `analysis/catalog.rs`.
- **Smaller binary.** Release builds use one codegen unit, fat LTO and stripping. Rarely used
  OpenDAL services (azdls, azfile, swift, tos, webhdfs, upyun, http) are behind cargo features
  (`all-services`); the default build covers s3, gcs, azblob, oss, cos, obs, b2, webdav, fs and
  memory.

### CLI, automation and distribution

- **Brief output wraps** to the terminal width (100 columns off a terminal, `COLUMNS` when set)
  and never truncates; `next` commands are printed whole. Throttled requests show their share,
  and an SLA without a throttle objective says so instead of "SLA met" alone.
- **Help tiers.** `--help` lists the common flags of large commands and names the headings of
  the rest; `--help-all` lists every flag.
- **Flags with units.** One throttle flag, `--max-throttle-ratio` (the SLA of `run`, the budget of
  `tune` and `fix`; `tune --max-throttle-ratio` and `--max-throttle-rate` are merged into it);
  `--max-error-ratio`; compare `--max-p99-increase-pct`, `--max-p50-increase-pct`,
  `--max-throughput-drop-pct`, `--max-error-increase-pp`, `--max-request-amp-increase-pct`,
  `--max-read-amp-increase-pct`, `--max-cost-increase-pct`; `compat --min-score-pct`. The 4.x
  names remain as hidden aliases.
- **Dynamic completions.** `oprobe completions SHELL` completes target names, run ids, labels and
  finding codes live; `--static` prints the 4.x script.
- **简体中文.** `--lang zh-CN` (or `OPROBE_LANG`, `LC_ALL`, `LC_MESSAGES`, `LANG`) localizes the
  brief, the findings' cause and advice, and error labels and codes; the catalog
  (`oprobe_core::i18n`) is shared with the app. JSON output stays English.
- **watch exports.** OTLP metrics are one cumulative stream per session with a stable
  `service.instance.id` and the target (no run id per window), honour
  `OTEL_EXPORTER_OTLP_HEADERS` and are protobuf by default (`OTEL_EXPORTER_OTLP_PROTOCOL=http/json`
  for JSON). Prometheus series carry a `target` label, with `oprobe_build_info`,
  `oprobe_last_success_timestamp_seconds`, HELP text and `/healthz`.
- **GitHub Action.** `compare --strict` by default (`strict`), the data directory (noise floor,
  capacity history) cached per target across jobs (`cache-data`), and `repeat`, `calibrate`,
  `calibrate-repeat` inputs.
- **Verified updates.** `install.sh`, `install.ps1` and `oprobe self-update` check the GitHub
  build provenance (`gh attestation verify`) when the GitHub CLI is logged in, else a minisign
  signature over `SHA256SUMS` (`SHA256SUMS.minisig`, signed by the release when `MINISIGN_KEY`
  is set); `OPROBE_VERIFY` / `--verify auto|require|checksum`. The self-update result gains
  `verified_by`.
- **Supply chain and CI.** Every action is pinned by commit SHA with Dependabot updates, release
  jobs use no caches, `cargo deny` checks advisories, licenses, bans and sources (`deny.toml`;
  RUSTSEC-2023-0071 documented), pull requests test on Linux, macOS and Windows in debug with
  cargo-nextest with cancel-in-progress, and release builds and installers run on main and
  nightly only. crates.io publishes `oprobe-trace`, `oprobe-core`, `oprobe` in order.
- Text: "1 request" (not "1 requests"); watch prints SLO values in their unit.

### Added (stream SCALE: clients and distributed load, design v5 §7–§8)

- **`--client aws-sdk | exec:COMMAND`.** Drive any workload with aws-sdk-s3 (cargo feature
  `aws-sdk`, over oprobe's instrumented transport) or with an external adapter speaking the
  versioned oprobe client protocol `oprobe.client/1` (NDJSON over stdin/stdout, documented in
  [docs/clients.md](https://github.com/hxddh/oprobe/blob/v5.0.0/docs/clients.md); schemas `client-hello`, `client-hello-reply`,
  `client-op`, `client-result`). `report.client` names the client, its version and the stages
  it observes; findings that need other stages are suppressed with a warning. The client is
  never stored in a run's spec. Reference adapters in `adapters/`: `minimal` (Python standard
  library), `boto3`, `awscrt` and `go-sdk` (aws-sdk-go-v2 with `httptrace`).
- **`oprobe worker` and `--workers host:port[*WEIGHT],… | local:N`.** Token-authenticated
  workers (`oprobe.worker/1`, h2c or TLS); the coordinator checks versions and clock offsets,
  splits concurrency, rate, dataset and budgets, starts every worker at one instant, enforces
  the global budgets, fans out cancellation and merges the workers' snapshots and trace rows
  into one run with `report.workers[]` and `report.dimensions.workers[]`.

### Added (stream MEAS: measurement, design v5 §3)

- **Open-loop schedule stage.** Open-loop runs time every operation from its intended start: the
  `schedule` stage (intended → actual start) is part of its latency, and arrivals that never
  started before the window ended are counted as censored latencies in the tail ("≥" values)
  instead of being dropped. `report.schedule` gives `open_loop`, `arrivals`,
  `intended_ops_count`, `missed_arrivals_count`/`_ratio`, `schedule_stage` and
  `max_schedule_lag_ms`; a closed-loop run says `open_loop: false`.
- **Arrival shapes.** `--arrivals constant|poisson` and a time-varying rate with
  `--shape ramp:FROM..TO/D | step:R1,R2,…/D | diurnal[:MIN..MAX/PERIOD]`. Over `--workers` the
  shape is scaled by each worker's weight and every worker draws its own Poisson seed.
  A constant pace never bursts its backlog after a stall: late arrivals leave at most 4 at
  once, then at the nominal spacing, keeping their intended start (the lag shows in latency).
- **Log replay.** `--replay FILE` replays an S3 server access log, CloudTrail S3 data events or
  an oprobe run file, with `--speed N` and `--map-prefix FROM=TO` (repeatable); writes outside
  the run prefix are refused. Replays are split by key over local workers and refused for
  remote ones.
- **Self probe.** `report.self` measures the load generator: scheduling lag, Tokio runtime busy
  share, CPU after warmup, RSS, the observer's cost per operation and `client_saturated`.
- **Streamed run file.** Rows go to a writer thread that appends columnar segments while the
  run measures; past `trace.max_requests` every failed or throttled operation is kept and the
  rest thinned. A run keeps only a 100k-row sample in memory, so memory stays flat on long runs.
  The interval series adds p90, p99.9, max, in-flight, new connections, CPU, missed arrivals and
  heatmap columns, at 1 s for the first ten minutes and 10 s after.
- **Server signals.** Requests record the provider's error code (from the body, first 4 KiB),
  S3 `x-amz-id-2`, the bucket region, `Server-Timing` and the CDN cache status.
- **HTTP/2 stream waits.** The time a request waits for a stream slot (`stream_wait`, part of
  `queue`), flow-control stalls and the streams open on the connection; the microscope transport
  opens several HTTP/2 connections per host by stream budget.
- **Lab protocols and HTTP/2 preflight.** `lab://NAME?proto=h1|h2c|https|h2`; lab servers speak
  h2c by default, so `--http h2` works on `lab://` targets, and TLS variants trust the lab CA.
  With `--http h2` a target without HTTP/2 is refused up front (`net.http`, with a hint) instead
  of failing every request.
- **Kernel upload acknowledgement, min RTT, retransmits.** TCP samples keep the minimum RTT,
  unsent and unacknowledged bytes, lost segments, bytes acknowledged and pacing; every request
  samples its connection and carries the retransmits since the previous sample. On Linux,
  HTTP/1 uploads through the microscope transport poll `TCP_INFO` until the last byte is
  acknowledged: `upload` ends at the acknowledgement and `server` (TTFB) starts from it.
- **`report.dimensions`.** Where the traffic and the trouble are, from aggregates: prefixes,
  hot keys (at most 20, with their share of the throttling), size buckets and cold vs warm reads,
  each with its share, throttled and error ratios and p99.
- **Object size distributions.** `--object-size dist:lognormal(MEDIAN,SIGMA)` or
  `--object-size file:PATH` (one size per line) next to a fixed size.
- **Cold-start sample.** Warmup used to absorb every dial; `connections.cold_start` now reports
  the connections dialed at the start of warmup, their connect time and the latency of the
  operations that waited for them.
- **`schedule_backlog` finding.** More than 1 % of the open-loop arrivals never started: the
  tail holds censored values and throughput is what the client managed; capacity advice is
  suppressed.
- Distributed runs merge all of the above across workers (self, dimensions, the open-loop
  fields, timelines and cold starts).

### Added (stream ANA: analysis, design v5 §4)

- **Competing limit models.** Capacity fits in-flight, token-bucket rate, per-key,
  time-varying, queueing, bandwidth-cap and client-bound hypotheses and selects one by BIC
  (`capacity.hypotheses`); the 4.x ceiling and rate fields follow the selected hypothesis. The
  lab gains limit faults with one scenario each: `ratelimit`, `hotkey`, `bandwidth`, `queueing`
  (an M/M/4 queue), `timevarying` and `noisy` (random 5xx unrelated to the load).
  The in-flight ceiling's `lo..hi` bracket spans every plausible split plus one level of
  measurement error; `requests` is the likeliest split and the safe concurrency, tune and the
  in-flight advice apply their headroom to it (4.x used `lo`).
- **Advice per limit model.** The throttling finding's advice depends on the selected
  hypothesis (rate, per-key, time-varying, random, inconclusive); a queue, a bandwidth cap or a
  busy client no longer gets "add workers". Retried 5xx are reported even under throttling, and
  `cache_opportunity` needs real data or a stated skew.
- **Two-sided verify with an SPRT.** `verify` runs 2 to 8 interleaved pairs (`--repeat`) and
  stops a finding early once its sequential test decides, with bounded log-ratio and
  Wilson/Newcombe intervals; each finding gets a net verdict (`improved`, `regressed`,
  `traded`, `no_change`, `inconclusive`), the same one `compare` shows for those runs. A loss
  within a confirmed prediction (fewer streams under a bandwidth cap) never refutes a finding.
- **Three-tier gate.** Each compare check is `fail` (significantly worse and over the limit),
  `warn` (significantly worse within the limit) or `inconclusive` (not significant, but the
  interval reaches past the limit), next to `pass`/`improved`; the gate verdict gains `warned`
  and `inconclusive`. Intervals are log-ratio and pooled block bootstraps instead of
  t-intervals on two runs (run-level once either side has 3+ runs), `compare` reports a net
  verdict, and `compare --fail-on-warn` makes a warn fail the gate (exit 3).
- **Fix menu.** `fix` offers 2 to 4 Pareto options, each with goodput, p99, throttling, monthly
  cost and whether it was verified.
- **Tune 5.** A rate-limit hypothesis adds paced rate trials; Gaussian-process surrogates of
  goodput and the throttle ratio pick the next concurrency by constrained expected improvement;
  a measured configuration beyond the fixed headroom is accepted when the posterior risk of
  exceeding the throttle budget is at most `--risk` (default 0.05), and is then reported as
  `safe_concurrency`. Under a queue or a bandwidth cap the tuner prefers the lowest-latency
  configuration on the plateau. Trial goodput counts only the in-window operations that
  finished by the deadline, so a large closed loop no longer looks faster than it is.
- **Bill-grade cost.** Price tables gain storage classes (minimum billable size and duration,
  retrieval fees), egress tiers with free allowances, inter-region transfer and per-provider
  error billing; the cost spec takes `storage_class` (`standard`, `infrequent`, `archive`) and
  `inter_region_fraction`. The AWS egress note is corrected.
- **Deterministic lab queue.** The lab's queue runs on virtual time, so its capacity is exactly
  servers ÷ service time whatever the timer resolution; on Windows the lab asks for 1 ms timers.
  In-flight ceilings tolerate a client that counts in-flight requests one level off.

### Security and MCP (stream SEC, design v5 §5–§6)

- **Private store files.** On unix, store files are created `0600` and new directories `0700`.
- **Lab exposure.** `oprobe lab` refuses a non-loopback `--bind` unless `--allow-remote`, caps
  object bodies, the in-memory store and open connections (`--max-object-size`,
  `--max-store-size`, `--max-connections`) and times out slow request headers.
- **MCP default budgets.** Every run the MCP server starts is limited to 50 000 requests,
  2 minutes, $1 and 5 GiB unless the server's own flags say otherwise; a tool call can only
  lower them.
- **Credential-safe overrides.** A tool call may not point a configured or credentialed target
  elsewhere (endpoint, proxy, insecure, options, `set`): overrides are accepted only for
  unsigned requests and `memory://` / `lab://`, never towards link-local addresses.
  `probe_endpoint` refuses loopback, private, link-local and reserved addresses unless
  `--allow-private-endpoints`.
- **HTTP bearer token.** `oprobe mcp --http` requires a bearer token (a new one written to
  `<data>/mcp-token`, mode 0600, at every start, or `--token-file`), compared in constant time.
  A non-loopback bind needs `--http-allow-remote` and TLS (`--tls-cert`/`--tls-key`) or
  `--insecure-remote`; connections (`--max-connections`), bodies and header time are limited.
- **Untrusted text.** Object keys and service messages are escaped, bounded and kept in
  untrusted fields, out of the tools' text content.
- **No client or workers via MCP.** Tool arguments and `set` keys that name a load client or
  workers are refused (`safety.refused`), and so are replays of runs that used them.
- **Jobs.** `verify_finding` and `recommend_config` are jobs; `doctor`, probes, `sample` and
  `cleanup` continue as jobs after 5 s; `start_run` waits at most 5 s; `get_run` and
  `cancel_run` handle every job.
- **Compact responses.** `tools/list` shrinks from 442 KB to 51 KB (compact input schemas,
  output schemas that name their `oprobe://schema` resource); results are summaries by default
  (`detail=summary|full`; `include_report` is accepted as an alias of `full`), `list_runs`
  pages with a cursor and `oprobe://runs/{id}/slowest` pages five operations at a time.
- **New tools.** `ask_run` (questions about a stored run, answered from its report with cited
  fields, no model), `save_target` and `apply_profile` (both need `--allow-write`).
- **Next steps as MCP calls.** Every next step carries an MCP call, derived from its CLI command
  when it has none (`target add` → `save_target`, `fix` → `recommend_config`, `run --from` →
  `start_run`, …); capabilities report the default budgets and the bearer auth.

### App (terminal and native workbench, design v5 §9)

- **Target Health main screen.** The app opens on live tiles with sparklines of the last runs
  and the baseline band, the selected limit hypothesis, a primary Fix action with its
  prediction (opening the fix menu) and an annotated run history.
- **Per-operation waterfall.** Drill into one operation's requests (stages, retry backoff,
  critical path, compared with a typical operation) from the timeline, the heatmap brush or the
  waterfall command. The timeline opens on the p99 operations, slowest first.
- **One verdict source.** Compare and Verify show one net verdict through `App::compare`, with
  one vocabulary and colour scale for verdicts, severities and gate checks; a warn check reads
  "worse (within limit)", and a large change without significance is inconclusive, never
  "same". Test Connection runs a read-only doctor through `App`.
- **Honest terminal charts.** Gutters as wide as their labels, no grid in a terminal, x labels
  never overlapping, text swatches in legends.
- **Toasts** take the terminal's status row instead of drawing over content; terminal
  notifications use OSC 9 or the bell.
- **Provisional live findings** with a confidence while a run measures, an early-numbers flag
  and the tuner's ETA. Findings use the shared presentation with readable evidence labels, and
  CLI commands in advice become buttons.
- **`oprobe://` view links.** `oprobe://run/ID?view=…` (and `oprobe gui LINK`) opens the run and
  restores its view; the scheme and `.oprobe` files are registered by the Linux desktop entry,
  the macOS `Info.plist` and the Windows installer.
- **Markdown exports carry links** to the shown view.
- Also: keyboard heatmap brush and sort keys, a fuzzy command palette with shortcuts and the
  reason a command is disabled, per-platform shortcut labels and icons, app text size, and the
  language taken from the OS. `RunSummary` gains `preset` and `throttled_request_ratio`.
- **Live snapshots on the event bus.** `EventBus` gains an in-process live stream (engine
  snapshots, cleanup, the finished run, budget stops); it is never serialized, so CLI and MCP
  output are unchanged. The app's run job goes through `App::run`.

### Known limitations

- There are no `oprobe probe limits` or `oprobe probe semantics` commands.
- TCP info (RTT, retransmits, upload acknowledgement) is not collected on Windows; the upload
  acknowledgement is Linux only.
- `--workers` refuses `--repeat` and `--baseline`.
- `compare` does not flag runs made with different load clients (`report.client`).
- The app's tuner job still calls `engine::measure` directly instead of going through `App`.
- Plain `oprobe run` reports still count operations that started in the window and finish
  while draining after the deadline (the tuner's trials do not).
- The release binary (about 60 MB with the default features) misses the 45 MB target of design
  v5.
- Installers are still not signed with a developer certificate (see 4.0's notes).

## v4.0.2 — 2026-11-21

### Fixed

- MCP `recommend_config` with `from` and no `target` forwards the call's target overrides
  (`region`, `anonymous`, `endpoint`, `options`, …) to the `fix` it runs, as they were applied to
  the profile the server checked; they were dropped, so the measurement used the stored settings.

## v4.0.1 — 2026-11-21

A patch release for four review findings in 4.0.0; no breaking changes.

### Security

- **MCP `recommend_config` could write on a read-only server.** With `from` naming a run that used
  `--existing`, the call passed the read-only check, but the child `oprobe fix` was rebuilt from the call's own
  arguments: the `--existing` prefix (and any `set`) was lost and the child uploaded a generated dataset to the
  target, bypassing `--allow-write`. The child is now `fix --from RUN --set …` with every workload flag, so it
  measures exactly the checked workload, plus `--read-only` whenever the server would refuse the write;
  `verify_finding` passes `--read-only` the same way. As defense in depth, a read-only server sets
  `OPROBE_MCP_READ_ONLY=1` on its child commands, which then refuse any measurement that would write to a
  target outside their own process.
- **Secrets in target URLs were stored in reports and run files.** `spec.target_ref` kept the target as typed
  (`webdav://alice:hunter2@host/files`, `s3://bucket?secret_access_key=…`) and `spec.profile.target.url` kept
  secret query parameters; both reached run files, `--json` reports, replay commands, next steps and the
  dry-run plan. Passwords and query parameters with secret names (`secret`, `key`, `token`, `password`,
  `credential`, `sas`, `sig`, `signature`, `session`, …) are now removed; replay resolves credentials again from
  the environment, profile or configured target. Run files written by 4.0.0 keep what they stored.
- **`verify`, `fix` and `baseline calibrate` budgets now cover the whole command.** Each replay of `verify
  --repeat N` used a fresh guard, so the command could spend N × (findings + 1) times `--max-requests`,
  `--max-bytes`, `--max-cost-usd` and `--max-duration` (MCP `verify_finding` forwards the server's budgets into
  the same path). One guard is now shared by every replay, by the diagnosis, tuning and verification of
  `fix`, and by the runs of `baseline calibrate`, as with `run --repeat`; the first breach stops the command
  (exit 4, the partial run kept).

### Fixed

- `oprobe fix --no-save` failed at verification (it replayed the diagnosis run by id, and that run was never
  stored); the replays now start from the diagnosis in memory.
- `oprobe baseline calibrate --from RUN --dry-run` replayed the run instead of printing the plan.

### Added

- `oprobe fix --from RUNREF [--set KEY=VALUE]…`: fix the workload of a stored run (resolved like `run --from`).

## v4.0.0 — 2026-11-21

oprobe 4: *one model, one spec, one loop* ([docs/design/v4.md](docs/design/v4.md)). Nothing reads 3.x run files
or reports.

### Foundation (breaking)

- **Run spec**: every report carries `spec` (`oprobe.report/4`; run files are `oprobe.run/4`) — the resolved
  profile with secrets redacted, the target as given, the seed, `--existing` and the label. `oprobe run --from
  RUNREF [--set key=value]… [flags]` replays a stored run (credentials are resolved again from the target);
  `--set` changes any knob, workload, SLA or cost setting (`oprobe run … --set seed=7` works without
  `--from` too). `config::defaults()` is the single source of built-in defaults.
- **Findings' verifications** are `{ description, overrides, command, rerun, replay, expect }`: `command` is
  `oprobe verify RUN CODE`, `rerun` is `oprobe run --from RUN --set …`, `replay` is a self-contained
  `oprobe run TARGET …` that keeps every setting of the run (warmup, seed, mix, dataset, endpoint, region, HTTP
  version, tail-cut, rate, `--existing`, `--max-ops`) and changes only the advice. The 3.x `command: Option`
  is gone.
- **Capacity model**: `report.capacity` estimates the in-flight ceiling from the request rows (the levels at
  which throttling starts, bracketed `lo`/`hi`), the per-operation request shape (a parquet scan peaks at 4
  requests in flight, not its 6.3 requests per op), `safe_concurrency(headroom)` and `predict(concurrency)`.
  Ceilings are remembered per target and workload in `<data>/capacity.toml`.
- **Target identity**: `--allow-target` on the MCP server compares normalised identities (scheme and bucket
  case, default ports, `.`/`..`/`//`, trailing `/`, query) on whole path segments; `..` above a root and
  `existing` prefixes that leave the allowed target are refused (fixes the `fs://…/allowed/../other` bypass).
- **Typed input**: one parser per input kind (`src/input.rs`) behind clap, the profile and config loaders and
  MCP, with "did you mean" hints. Bad input is never `internal` any more, and the CLI and MCP give the same
  code; the message-matching classifier is gone. Provider aliases (`aws`, `gcp`, `google`, `azure`, `r2`, `b2`,
  `oss`, `aliyun`) are accepted. Missing credentials are `auth.no_credentials`. New code
  `usage.unknown_baseline`. `--object-size 0`, `--endpoint ftp://…`, a missing `--config` file, negative
  durations and amounts and `target add` of an existing name (now needs `--replace`) are refused; `top -o json`
  and `gui` without the feature exit 2 with a JSON error.
- **Envelope**: every JSON command result carries `next` (`{ title, cli, mcp: { tool, arguments } }`) and
  `warnings` (including the command's stderr warnings). Doctor's `next_command`, explain's `next_commands`,
  the demo's `next_commands` and MCP `get_run`'s prose `next` moved onto it. Schemas are under
  `…/schemas/4/`; new schemas: `run-spec`, `overrides`, `capacity`, `capacity-history`, `verification`,
  `recommendation`, `next-step`, `target-id`, `defaults`.
- **Lab**: new `lab://parquet` scenario (503 above 24 in flight, no stragglers) for the parquet preset.

### Analysis: one capacity model behind every recommendation

- Every finding that mentions concurrency (`throttling`, `server_latency`, `small_objects`, `client_queue`)
  and `simulate::concurrency` ask the capacity model. Following a suggestion never pushes past the measured
  ceiling, and runs under the ceiling reuse the remembered one. The 3.x ping-pong is gone: on a parquet lab
  the findings used to say 16 → 3 → 6 → 12, and on small files "try 44" after a clean run at 22. Each
  verification's `expect` holds the predicted goodput with its interval and the throttle budget.
- **Throttling is defined once**: `summary.throttled_request_ratio` (throttled responses / requests, used by
  the SLA) and `summary.throttled_op_ratio`. `throttle_ratio` is gone, and `sla.max_throttle_ratio` is now
  `max_throttled_request_ratio`.
- **Throughput counts successful operations** (`summary.ops_per_s`; `attempted_ops_per_s` counts all).
- **Invalid runs** (no ops, errors above the SLA, a window shorter than the warmup) and **interrupted runs**
  keep only correctness findings plus `run_invalid` / `run_partial`. `report.run.partial` records why, the
  warning is in `report.warnings`, and partial or invalid runs are refused as baselines.
- **Tail-cut**:
  - The simulator uses the layer's own rule (p95 × 1.5, at least 50 ms, 32 samples). A run with tail-cut on
    is never told to try it.
  - OpenDAL 0.59's tail-cut layer is blind for its first 10 s. Runs show how long it was blind, and the
    suggestion lengthens short runs.
- **Error advice per class**: credential advice only for authentication failures. Retry-exhausted 429/503
  failures fold into throttling.
- **Tail attribution** ignores the kernel RTT when it tracks server delay (loopback, correlated with TTFB).
  The straggler API is chosen stably.
- **Units**: integer `_count` fields everywhere; fractions renamed `_mean`. One percentile estimator.
- **Readable evidence**: the HTML, Markdown and text evidence uses human labels, units and percentages.
  Every bill that includes storage says "(assumed 1 TiB stored — set --stored-gib)".

### Tune, verify, fix, compare

- **`oprobe verify RUN [CODE|--all] [--repeat N]`** replays the run interleaved (A B A B…) with each
  finding's change. It compares the sides and answers `confirmed`, `refuted` or `inconclusive` per
  expectation, with numbers. Outcomes calibrate the capacity history (`capacity::calibrate`).
- **`oprobe fix TARGET`** runs diagnose → tune → verify. It returns a `Recommendation` (knobs, profile
  TOML, Rust snippet, deltas, `confirmed`) and a profile patch (`--write-profile`).
- **Tune**:
  - **Objective and budget**: optimises **goodput** by default (`p99`, `cost` and `balanced` also exist)
    within a throttle budget (`--max-throttle-ratio`, default 0.1 % of requests). Throttling within the
    budget no longer stops the ceiling search.
  - **Search**: bisection repeats trials at the boundary (`--boundary-repeats`). The margin
    (`--headroom-ratio`, default 15 %) is counted in requests in flight and re-checked after io and range
    steps.
  - **Reporting**: an infeasible baseline is reported as such, and deltas are taken against the best
    feasible trial. There is no more "−61 % vs baseline" headline. The cost of the margin and the Pareto
    front are shown.
  - **Recommendation**: in the tune JSON and in MCP `start_tune`. The emitted profile has no environment
    defaults such as `proxy`.
- **Compare**:
  - **Identity**: a verdict needs the same target identity and workload; otherwise `comparable: false`
    with the reasons (exit 2 under `--strict`).
  - **Statistics**: one run per side uses a block bootstrap over time windows, labelled low confidence.
    Several runs use a Welch interval over per-run values.
  - **Noise floor**: `<data>/noise.toml`, filled by `oprobe baseline calibrate` and verify's A/A pairs.
  - **Count floors**: 0 → 1 throttled response no longer fails the gate.
  - **Gate checks**: `{ id, unit, baseline, candidate, delta_ratio, limit_ratio, verdict }`.
  - **Text output**: compare sizes its columns to their content and summarises long values.

### CLI and agents

- **Next steps everywhere**: every command result has `next` steps with both a CLI command and an MCP call.
  MCP `start_run` and `plan_run` take `from` and `set`.
- **New MCP tools**: `discover_targets`, `sample_objects` (read-only), `verify_finding`, `recommend_config`.
  New CLI commands: `oprobe target discover` (credential sources by name only) and `oprobe target sample`.
  `oprobe` with no command and no terminal prints the discovery.
- **Formats per command**: `-o` outside a command's formats is refused before it runs, and `capabilities`
  lists them. `completions` and `man` are text only.
- **Schema rules**: the unit and enum rules are checked on every schema.
- **MCP server**: `lab://` targets are allowed on a read-only server. Unknown tool arguments are refused with
  a hint. `get_run` turns `done` atomically with its last event, and progress never overshoots.
- **Safer defaults**:
  - `doctor` is read-only (`--write-probe` opts in) and names what it sees, with an object sample.
  - `cleanup` is a dry run unless `--yes`.
  - Doctor failures show a summarised cause (raw output with `-v`).
- **Better errors**:
  - Backend-config errors name `--option KEY=VALUE` and `[targets.NAME.options]`.
  - New codes `net.rate_limited` and `net.http` for non-storage APIs.
  - A warmup at least as long as the duration warns.

### App (GUI and terminal)

- **Add Target**: discovery of credential sources, a read-only connection test, and save to the config.
- **Re-run and Verify** are built from the run's spec, so nothing is dropped. Verify opens Compare with a
  confirmed/refuted/inconclusive banner.
- **One set of defaults** with the CLI (tune, demo, cost).
- **Run list**:
  - The list is live: it watches the runs directory and refreshes on focus.
  - Partial runs get a badge; a budget stop is not a failure.
  - "Stopped during warmup" replaces 0 ops/s.
  - Search, "show more" and a truthful run count.
- **Compare**: defaults to the same target and workload. Selectors never wrap, and a non-comparable pair says
  why.
- **Commands**: one context-aware command list drives the menu bar and the ⌘K palette (Verify, Open Run,
  Switch Target, Add Target, Fix, Export…).
- **Keyboard**: letter shortcuts never fire in text fields, sheets focus their first field, Enter submits,
  Esc cancels, and the terminal shows a focus band.
- **Accessibility**: every button has an accessible label, and charts carry a text summary and a data table.
- **Overview**: a THROTTLED tile, and the cost tile labels assumed storage.
- **Layout**: works at 120×30 and in narrow windows (tiles wrap, Cost becomes one column, numbers never
  wrap).
- **Live views**:
  - live series append instead of rebuilding;
  - chart paths are cached;
  - the crosshair has its own layer;
  - live phases (preparing, warmup, measuring, cleanup) are shown;
  - the tune result offers Run, Verify, Save profile and Rust snippet.

### Onboarding and distribution

- **The demo is the lab**: `oprobe demo` runs exactly `oprobe run lab://demo`. Every lab scenario has a
  default workload, applied when no flag says otherwise: `lab://demo` is small-files (64 objects of 64 KiB),
  32 workers, 0.5 s warmup, 8 s; `lab://parquet` is the parquet preset with 8 workers. The demo used to
  measure `s3://demo/bench`, so `compare` flagged a different target, dataset and mix against
  `run lab://demo`; now both have the same target identity and `workload_hash`.
- **Demo next steps are replays**: `oprobe run --from RUN`, `oprobe compare RUN last`, `oprobe verify RUN
  CODE` and `oprobe run --from RUN s3://YOUR-BUCKET/…`, with MCP calls in the envelope's `next`.
- **Installers**: `install.sh` and `install.ps1` resolve the latest version from the release's `latest.json`,
  then the `releases/latest` redirect, then the API, so GitHub's anonymous API rate limit no longer breaks an
  install. `OPROBE_VERSION` still pins a version; `OPROBE_RELEASE_BASE` points them at a mirror. `install.sh`
  offers to add the install directory to the zsh, bash or fish rc file (asking on `/dev/tty` when piped;
  `OPROBE_MODIFY_PATH=1|0` decides without asking). `install.ps1` updates the user `PATH` and the current
  session's `$env:Path`, uses `-UseBasicParsing` and a silent progress bar, and detects ARM64 Windows from the
  OS (it used to install the x64 build under emulation).
- **`self-update`** reads `latest.json` before the API (`release_source: latest_json | api` in its output)
  and verifies against the SHA-256 listed there. A GitHub 403 or 429 is `net.unreachable` (retryable), never
  `storage.throttled`; an unknown `--to` tag is `usage.invalid_argument`.
- **Package managers are published**: on each stable release, the workflow updates the Homebrew tap
  `hxddh/homebrew-tap` (`brew install hxddh/tap/oprobe`, `brew install --cask hxddh/tap/oprobe-app`), the
  Scoop bucket `hxddh/scoop-bucket` and opens a winget-pkgs pull request, each when its secret (`TAP_TOKEN`,
  `SCOOP_TOKEN`, `WINGET_TOKEN`) exists and skipped with a warning otherwise. The formula and the cask have
  distinct names (`oprobe`, `oprobe-app`; the cask asset is now `oprobe-app.rb`). The Scoop manifest's
  `checkver` reads `latest.json` and its `autoupdate` takes hashes from `SHA256SUMS`. The docs say that
  `winget install hxddh.oprobe` works only once the package is accepted.
- **GitHub Action** (`uses: hxddh/oprobe@v4`): `version` defaults to `latest-major`, the newest release of
  the action's major version (`latest` and exact versions still work). The pull-request comment step has
  `continue-on-error` (fork PRs get a read-only token), a named baseline is saved to the cache only when it
  is replaced, and `args` / `compare-args` are split with shell quoting but never evaluated.
- **CI** runs both installers against a local fake release (`scripts/test-install.sh`,
  `scripts/test-install.ps1`), shellchecks the scripts strictly and checks the generated manifests.

## v3.0.0 — 2026-11-20

oprobe 3 turns a benchmark into a product: **see every request your object-storage client makes, know where
the time and money go and what to change — usable by people, CI and agents alike.** Every run is saved as one
`.oprobe` file; every command has typed output (`-o text|json|ndjson|md|html`), stable error codes and exit
codes. Nothing reads 2.x reports or flags.

### Highlights

- **`oprobe demo`**: zero flags. A local lab with throttling above a concurrency limit and 1-in-40 stragglers,
  measured and narrated step by step, ending with how to measure your own bucket.
- **Guided first run**: `oprobe run` with no target in a terminal offers the demo, the AWS profiles, Google
  application default credentials and Azure storage variables found on the machine, or a URL, and saves the
  choice as a named target.
- **Answers first**: the default output of `oprobe run` is at most 20 lines in 80 columns — verdict, top
  findings (cause · evidence · advice · next command), key numbers, and `oprobe runs show <id>` for the rest.
- **Findings with evidence**: every diagnosis item has a stable code, severity, one-sentence cause, evidence
  (numbers, operation ids, time windows), advice and, where possible, a command that verifies it;
  `oprobe explain` narrates a run or documents a finding code.
- **Single-file HTML report** (`-o html` on `run`, `runs show`, `compare`): SVG charts with real axes (log
  latency), a latency heatmap, stage shares, findings with evidence, waterfalls of the slowest operations,
  a filterable and sortable table of every request, network, cost and environment; light/dark, offline,
  printable.
- **Run history**: `oprobe runs list|show|open|export|rm|gc`; RUNREFs (`last`, `last~1`, an id prefix, a
  label or a file); `runs show --detail`; `runs open` opens the run in the app.
- **Named targets and configuration**: `oprobe target add|list|show|rm|test` (`add` runs a quick read-only
  check; `test` is the full doctor), `credentials = "env" | "profile:NAME" | "anonymous"`,
  `oprobe config show` (every value with its source), `config path`, `config edit`.
- **Regression gate on runs**: `oprobe compare A B` and named baselines (`oprobe baseline`, `run --baseline`),
  configuration differences listed first, Markdown and JUnit.
- **GitHub Action** (`uses: hxddh/oprobe@v3`): installs a pinned, checksum-verified release, runs, compares
  against a file or a named baseline kept in the Actions cache, uploads the run and the HTML report, keeps one
  sticky PR comment up to date, and fails on regression.
- **Agents**: `oprobe mcp` (MCP server; read-only unless `--allow-write`, budgets, progress notifications),
  `oprobe schema` (JSON Schemas of every output), `oprobe capabilities`, the `oprobe.events/1` NDJSON stream,
  `AGENTS.md`, `llms.txt` / `llms-full.txt`.
- **Deeper observability**: storage API attribution from OpenDAL's `ServiceOperation`, retry backoff as its own
  share, per-remote-address and per-connection splits, TCP_INFO and TLS handshake kind/resumption, outlier
  attribution (`oprobe analyze`), capability-aware workloads (batched delete, copy, paged list, presigned GET,
  footer reads, conditional writes), `oprobe compat` (correctness and compatibility score), `oprobe watch`
  (continuous probing, SLOs, Prometheus, OTLP), `oprobe matrix` (rank targets or knob values), exports to
  CSV, HAR, NDJSON and OTLP.
- **Safety rails**: `--dry-run` plans (requests, bytes, objects, USD, ETA), budgets (`--max-cost-usd`,
  `--max-requests`, `--max-bytes`, `--max-duration`, exit 4), `--read-only` (exit 5), writes confined to
  `<prefix>/oprobe-run-<id>/`, `--idempotency-key`, preflight before any write.
- **The app**: three panes (targets · runs · baselines / run inspector / selected request), virtualized request
  table, timeline waterfall with zoom and brush, compare, command palette; `oprobe gui --open FILE`.
- **`oprobe self-update`**: checks GitHub Releases, verifies the download against `SHA256SUMS`, checks that
  the new binary runs and replaces it atomically; refuses package-managed installs with their update command.

### Distribution

- New release assets: Homebrew cask for the `.dmg` (`oprobe.cask.rb`) next to the formula, winget manifests
  (`winget-manifests.zip`), `latest.json` (every asset's URL, size and SHA-256); `SHA256SUMS` covers every
  asset including the manifests.
- **SLSA build provenance** for every asset (`gh attestation verify FILE --repo hxddh/oprobe`).
- Packages remain unsigned (no certificates); [docs/install.md](docs/install.md#first-open-of-unsigned-packages)
  explains the first open on macOS 15+ (Privacy & Security → Open Anyway) and Windows SmartScreen.
- Release notes come from this file; CI checks the docs (links, the generated CLI reference in `docs/cli.md`).

### Documentation

Rewritten around the product: getting started, concepts (targets, runs, baselines, findings), the generated
CLI reference, workloads and presets, metrics (with `--microscope`-only values marked), reading the report,
CI, install, troubleshooting, privacy (no telemetry), releasing; README in English and Chinese.

### Shared core (breaking)

- **Run store**: every `oprobe run` / `tune` is saved as `runs/<id>.oprobe` (zstd NDJSON,
  `oprobe.run/3`: report + one row per operation and per request) with a `.json` sidecar under
  `$OPROBE_DATA_DIR`. `oprobe runs list|show|rm|gc`; runs are referenced by id, id prefix,
  `last`, `last~N`, label or path.
- **Report v3** (`oprobe.report/3`): `run`, `verdict {level, headline, exit_code}`,
  `knobs {requested, effective}`, `stages`, `network`, `heatmap`, `findings`, `slowest` (op ids);
  every numeric field carries its unit. 2.x reports are not read.
- **Global `-o/--output text|json|ndjson|md|html`**; stdout carries
  only the result; `-o ndjson` streams `oprobe.events/1`. `--format` is gone; `--option` lost
  its `-o` short flag.
- **Classified errors**: stable codes (`net.timeout`, `safety.read_only`, …) and exit codes
  (2 usage/config, 3 gate, 4 budget, 5 safety refusal, 130 interrupted); JSON errors in machine
  modes.
- **Safety rails**: `--dry-run` plan, `--read-only` (implied by `--existing`), budgets
  (`--max-requests`, `--max-bytes`, `--max-duration`, `--max-cost-usd`), `--idempotency-key`,
  and a preflight (list + tiny write/delete) before any data is uploaded.
- **Config file** with named targets (`oprobe target add|list|show|rm`, `oprobe config
  show|path`); `oprobe run prod` uses target `prod`.
- **Command tree**: `diff` is now `compare A B` (RUNREFs or files); `report` is gone (`runs show
  FILE`), `init` is hidden; `demo`, `analyze`, `explain`, `watch`, `matrix`, `compat`,
  `baseline`, `mcp`, `schema`, `capabilities`, `self-update`, `runs open|export`, `target test`
  and `config edit` are new (see Highlights).

### Trust fixes (breaking)

Places where oprobe could lead to a wrong conclusion. Reports use schema `oprobe.report/3`;
v2 reports are rejected by `oprobe compare`.

- **Retry backoff** is its own `retry` share of operation time (terminal, Markdown, app, JSON
  `stages.shares.retry_pct`, `verbs[].breakdown.retry_ms`) instead of being counted as client time; throttling
  headlines state how much time went to waiting for retries.
- **SLA gate**: `oprobe run` / `tune` exit 3 when the run breaches its SLA — by default more
  than 1% failed operations or any corrupted data (`verdict.sla` in the report). `--sla none`
  disables it; `--max-error-rate`, `--max-throttle-rate`, `--p99-ms` override it. `--fail-on`
  still adds a gate on diagnosis findings.
- **Write chunks**: the tuner no longer recommends chunk sizes OpenDAL raises to the service's
  minimum part size (5 MiB on S3); reports carry `knobs.requested` and `knobs.effective`.
- **Generated code** reproduces exactly the measured layers and parameters (one `ClientStack`
  feeds both the measured operator and `--emit-rust` / `--emit-python`).
- **`r2://` / `minio://`** require `account_id` / an endpoint instead of silently using AWS.
- **Cost** is projected only for recognised cloud endpoints or an explicit price table;
  otherwise `cost.applicable = false` and the section is hidden.
- **Tuner** narrative states the throttling ceiling once, explains the safety margin with
  numbers, no longer writes `scheme = ""`, and prints its report (trial table included) on stdout.
- **`oprobe compare`** (formerly `diff`) lists every configuration change; a different workload or target fails the
  gate (`--allow-workload-change`, `--allow-target-change`), knob changes are an A/B note.
- **Default transport** keeps OpenDAL's reqwest defaults (no connect timeout, unlimited idle
  connections per host).
- **Unknown `[target.options]` keys** are rejected with a "did you mean" suggestion.
- **`--anonymous`** sets each service's own unsigned-access options; unsupported services error.
- **Error classes** cover every OpenDAL error kind; timeouts and connect failures are detected
  from typed errors, not message text.
- Docs: the TLS cipher suite is only reported with `--microscope`.

## v2.0.1 — 2026-11-20

Fixes from a full review of 2.0.0. Some numbers change: see *Measurement changes*.

### Fixed

- **macOS: `oprobe.app` could not be opened.** Opened from Finder, the app runs `oprobe` without arguments, which
  was rejected as a usage error before the window opened. A bare `oprobe` now opens the workbench; CI and the release
  build open the app through LaunchServices and fail unless it stays up. The install docs describe the macOS 15
  Gatekeeper flow for unsigned apps (*Privacy & Security → Open Anyway*).
- **Secrets**: customer-supplied encryption keys (`server_side_encryption_customer_key`, `encryption_key`, …) and
  credentials embedded in URLs (`webdav://user:pass@host`) were written to reports, diff summaries and generated
  profiles. They are now redacted/stripped; `webdav://user:pass@…` maps to `username`/`password` options.
- **Generated profiles** (`tune --emit-profile`) rewrote `${AWS_ACCESS_KEY_ID}` as `${ACCESS_KEY_ID}` and failed to
  load; existing `${VAR}` references are kept, and defaulted or URL-derived options are no longer written.
- **Test data left behind** when the dataset upload failed or was stopped, or when the workbench was quit mid-run:
  uploads observe Stop/Ctrl-C and clean up; quitting the workbench stops the run and cleans up first (up to 30 s).
  An unreachable endpoint (connection refused, DNS) fails in seconds instead of retrying for a minute.
- **False throttling**: "429"/"503" (and "401"/"403" for hints) were matched anywhere in error text, including object
  names such as `obj-00429.bin`; classification now uses the error kind and the HTTP status. New hint for missing
  credentials.
- **Diff gate**: an operation missing from the candidate now fails; throttling is gated on its rate, not only when the
  baseline had none; new open-loop *missed %* check; warnings for closed- vs open-loop, different durations and
  interrupted runs.
- **Tuner**: never recommends a trial that failed the SLA when one passed (and says so when none did); throughput
  counts successful operations only; `Retry-After` is honoured per trial (capped at 60 s, also with `--cooldown 0`)
  and cooldowns can be stopped; `--max-trials` is respected; the cache trial no longer wins just because the test
  dataset fits in the cache.
- **Open-loop writes** could hit the same key concurrently and report false corruption.
- **Workbench**: *Apply to workbench* could apply the last run's settings instead of the tuner's winner; an engine
  panic left the UI stuck in *Running*; keyboard focus was lost when Run turned into Stop; window start-up errors
  were silently ignored (they are now reported and logged); defaults now match the CLI (preset `parquet`, tuner
  5 s trials, 24 trials); inputs have accessibility labels; terminal saves never overwrite files.
- **CLI**: an interrupted `run`/`tune` exits 130 and its report is marked `"interrupted": true`; `oprobe top`
  without a terminal and `oprobe gui` without a display exit 2 with a hint; `-c 0` and `--max-trials 1` are
  rejected; `top`/`gui` accept `--proxy`, `--ca-cert`, `--region` and honour `--color`/`NO_COLOR`.
- A cache-layer bug served the head of an object for a suffix (footer) read and could panic on out-of-bounds ranges.
- Release assets no longer include the WiX debug database.

### Measurement changes

- **Throughput** (ops/s, MiB/s, achieved rate) no longer includes the time spent draining in-flight operations
  after the window closed; values are higher than 2.0.0 reported, and correct.
- **Open loop** queues arrivals that find all workers busy and measures them from their intended start, so
  saturation shows in p50/p99 (2.0.0 dropped them); `missed` counts arrivals that never started.
- **Uploads** with the default client: `send` now ends near the last byte written (it ended before the first), so
  upload time moves from `ttfb` to `send`.
- Requests cut off by a timeout or tail-cut are recorded (with a `cancelled` error) instead of disappearing;
  bodies dropped short of their length are marked.
- Cold/warm: connections are identified by local *and* remote address; a dial that finished before the request was
  ready (hyper's pool race) is counted but not charged to that request; failed sends are not counted as warm.
- Time before your code first reads a response body is *stall*, not *transfer*.
- `Retry-After` in HTTP-date form is understood. Probe clock skew lost a +0.5 s bias.
- Cost: storage and egress are billed per GiB (2.0.0 overstated them by ~7 %); requests that never left the client
  are not billed. GCS batch requests are classified as deletes.
- Long runs use bounded memory (per-second statistics no longer keep a histogram each).

## v2.0.0 — 2026-11-19

oprobe 2.0 is a rewrite. Everything reported is measured per request; nothing is estimated.

### Highlights

- **Request-level white-box tracing of the client you deploy.** Requests go through OpenDAL's own
  HTTP client (reqwest + rustls/aws-lc-rs, configured as OpenDAL builds it), observed through
  reqwest's extension points: every backend request records DNS, TCP, proxy and TLS (when it
  dialed), request send, TTFB, transfer and consumer stall, plus status, connection, cold/warm, bytes
  and the provider's request ID. `--microscope` switches to oprobe's own instrumented connection pool
  for pool queueing, per-dial proxy timing and HTTP/2 stream detail. The observer effect is measured
  (`examples/overhead.rs`) and documented.
- **Exact attribution through OpenDAL.** A scope follows each logical operation through OpenDAL's
  layers and into its background tasks (concurrent readers/writers), and each operation's wall time
  is split exactly into queue / cache / backend / client by sweeping its request intervals.
- **Slow-operation forensics.** The slowest operations keep their full request timelines and
  async call stacks (await-tree, including OpenDAL sub-tasks) captured while in flight.
- **Realistic workloads.** Presets `parquet`, `checkpoint`, `small-files`, `metadata`, `mixed`;
  custom weighted mixes; zipf/hot-set key popularity; closed-loop concurrency or open-loop arrival
  rate without coordinated omission; warmup and time windows; `--existing <prefix>` read-only
  benchmarks of your own data; persisted datasets.
- **Throttle-aware tuner.** Concurrency ladder with 429/503 ceiling detection, binary refinement,
  Retry-After-aware cooldown and an 80 % safety margin, then coordinate descent over the knobs that
  matter for the workload, and a confirmation run. Emits a profile and Rust/Python OpenDAL code.
- **Measured cost model.** Requests are classified by real API (S3, GCS, Azure, B2 conventions)
  into billing classes; usage per operation is projected to your monthly volume for six providers,
  with waste from failed/throttled requests and over-reads and savings from caching.
- **CI gate.** `oprobe diff` checks throughput, per-operation p50/p99 (with noise floors and
  minimum samples), errors, new throttling, integrity, request/read amplification and cost; outputs
  terminal, Markdown, JSON and JUnit; exits 3 on regression.
- **New commands.** `top` / `gui` (the workbench in a terminal / window), `doctor` (step-by-step checks with fixes),
  `probe` (handshake microscope with clock-skew check), `lab` (S3-compatible fault-injection
  server), `cleanup` (sweeper with dry-run), `init`, `report`, `completions`, man pages.
- **Egress proxy support.** `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` with HTTP CONNECT
  tunnelling (incl. basic auth) and timed proxy handshakes; `--proxy` / `--proxy none`.
- **Workbench in a window and in the terminal.** `oprobe` with no arguments opens an interactive workbench built
  with Native Shell (NSH): a native window on macOS (AppKit), Windows (Win32/Direct2D) and Linux (GTK 4), or the same
  UI in any terminal (`oprobe top`, also over SSH). Live charts, time breakdown, per-API tables, cost, the slowest
  operation as a request waterfall, the tuner with apply/save, and the endpoint probe. One binary: macOS and Windows
  builds include the window (`.dmg`, `.msi` with a Start menu entry); Linux server packages stay headless and a
  `oprobe-desktop` tarball carries the window.
- **Two-layer architecture.** OpenDAL and its own HTTP client for measurement; NSH for every UI.
- **Native distribution.** Release archives named `oprobe-<version>-<target>`, static Linux musl
  builds, `.deb`, `.rpm`, macOS universal binary and `.pkg`, Windows `.msi` (x64 and ARM64),
  Homebrew formula, Scoop manifest, checksum-verifying install scripts, `SHA256SUMS`.

### Known limitations

- The macOS `.pkg`/`.dmg` and Windows `.msi` are not yet signed with a developer certificate: macOS asks to confirm
  on first open, Windows SmartScreen asks to *Run anyway* (see `docs/install.md`). Homebrew, Scoop, the install
  scripts and `cargo install` are unaffected.
- With OpenDAL's own client (the default), time spent waiting inside reqwest's pool for an idle connection is part
  of TTFB, proxy CONNECT is reported together with TCP to the proxy, and wire bytes and the TLS cipher are not
  available; `--microscope` measures all of these with oprobe's own pool.
- Observer effect: on tens of thousands of tiny requests per second over loopback, tracing costs up to ~10–18 %
  throughput; on real storage latencies it is negligible (`examples/overhead.rs`, `docs/metrics.md`).

### Breaking changes

- Report schema is now `oprobe.report/v2`; v1 reports are rejected by `oprobe diff` with a clear
  message. Re-create baselines with v2.
- Profile format changed (`[target] url`, `[workload] preset/duration/warmup/mix/dataset`,
  `[knobs]`, `[sla]`, `[cost]`). See `docs/workloads.md` and `oprobe init`.
- `serve-s3` is now `lab` (alias kept), `net-probe` is now `probe`, `tui` is now `top` (alias kept).
- Exit code for gate failures is 3 (was 2).

### Fixed (from v1)

- Secrets passed via environment variables were copied into profiles and JSON reports; option
  values with secret-like keys are now always redacted and emitted profiles use `${ENV}`.
- Stage timings were global sums divided by request counts; HTTP/2 TTFB used a connection-wide
  write timestamp; the "multipart merge" stage timed the entire upload for single PUTs; tracing was
  lost inside OpenDAL's concurrent sub-tasks. All replaced by per-request measurement.
- The auto-tuner fabricated synthetic "throttled" trials on errors; trials are now always real.
- Cost projections relied on invented scaling factors; now based on measured per-op usage.
- Release artifact names embedded the `v` tag prefix and lacked native packages.

## v1.0.0

- Pure-Rust CLI (`run`, `tune`, `diff`, `net-probe`, `tui`, `serve-s3`) on Apache OpenDAL 0.59.

## v0.1.0

- Initial prototype.
