# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
