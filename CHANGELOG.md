# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## Unreleased (v3)

### Trust fixes (breaking)

Places where oprobe could lead to a wrong conclusion. Reports use schema `oprobe.report/v3`;
v2 reports are rejected by `oprobe diff`.

- **Retry backoff** is its own `retry` share of operation time (terminal, Markdown, app, JSON
  `breakdown.retry_ms` / `shares.retry`) instead of being counted as client time; throttling
  headlines state how much time went to waiting for retries.
- **SLA gate**: `oprobe run` / `tune` exit 3 when the run breaches its SLA — by default more
  than 1% failed operations or any corrupted data (`sla_verdict` in the report). `--sla none`
  disables it; `--max-error-rate`, `--max-throttle-rate`, `--p99-ms` override it. `--fail-on`
  still adds a gate on diagnosis findings.
- **Write chunks**: the tuner no longer recommends chunk sizes OpenDAL raises to the service's
  minimum part size (5 MiB on S3); reports carry `effective_knobs` (requested vs effective).
- **Generated code** reproduces exactly the measured layers and parameters (one `ClientStack`
  feeds both the measured operator and `--emit-rust` / `--emit-python`).
- **`r2://` / `minio://`** require `account_id` / an endpoint instead of silently using AWS.
- **Cost** is projected only for recognised cloud endpoints or an explicit price table;
  otherwise `cost.applicable = false` and the section is hidden.
- **Tuner** narrative states the throttling ceiling once, explains the safety margin with
  numbers, no longer writes `scheme = ""`, and prints its report (trial table included) on stdout.
- **`oprobe diff`** lists every configuration change; a different workload or target fails the
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
