# CI: the storage regression gate

Measure storage on every change, compare it with a baseline and fail on regression — with the GitHub Action,
or with two commands anywhere else.

## GitHub Action

```yaml
# .github/workflows/storage.yml
name: storage
on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read
  pull-requests: write      # the sticky comment
  id-token: write           # only for cloud credentials via OIDC (below)

jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: aws-actions/configure-aws-credentials@v4     # or google-github-actions/auth, azure/login
        with:
          role-to-assume: arn:aws:iam::123456789012:role/oprobe-ci
          aws-region: eu-west-1
      - uses: hxddh/oprobe@v5       # pin a commit SHA for supply-chain hygiene: hxddh/oprobe@<sha> # v5.0.0
        with:
          target: s3://ci-bucket/oprobe
          preset: small-files
          duration: 20s
          repeat: 3                 # three measurements: a surer verdict
          args: -c 16 --max-cost-usd 0.25 --option "user_agent=oprobe ci"
          baseline: main            # a name: kept in the Actions cache, refreshed by pushes to main
```

What it does:

1. installs oprobe (`version`: by default the newest release of the action's major version, so `@v5`
   installs the newest 5.x) after checking its SHA-256 against the release's `SHA256SUMS`;
2. restores oprobe's data directory for this target from the Actions cache (`cache-data`): the noise floor
   and the capacity history survive from job to job (the runs themselves are not cached);
3. `oprobe run TARGET --repeat N` with the preset, duration and extra `args`, labelled with the commit SHA.
   `args` and `compare-args` are split like a shell would split them — quotes keep a value with spaces
   together — but never evaluated (no `$VAR`, no `$(…)`);
4. learns the target's noise floor (`oprobe baseline calibrate --from <run>`) when the data directory has
   none yet (`calibrate: auto`), so compare tells noise from regressions;
5. compares the run with the baseline (`oprobe compare --strict`: runs of different targets or workloads
   fail with exit 2 instead of passing with a warning), writing Markdown and JUnit;
6. saves the data directory back to the cache;
7. writes the comparison and the report to the job summary;
8. uploads an artifact with the `.oprobe` run file, `oprobe-report.html`, `oprobe-report.md` and the gate
   files — open the run later with `oprobe runs show oprobe-report/<id>.oprobe` or `oprobe runs open …`;
9. posts one pull-request comment and updates it on every push (it finds its own comment by a marker). The
   comment step may fail without failing the job: pull requests from forks get a read-only token;
10. fails the step on a regression (exit 3) or when the run breaches its SLA (`fail-on-regression: false`
   only reports).

### Inputs

| Input | Default | |
|---|---|---|
| `target` | — | URL or configured name; `memory://` and `lab://demo` need no credentials |
| `version` | `latest-major` | the newest release of the action's major (`@v5` → newest 5.x; resolved from `latest.json`, then the release list); `latest` = the newest release of any major; or an exact version such as `5.0.0` |
| `preset`, `duration` | preset's default, `20s` | |
| `args` | | extra `oprobe run` flags (`-c 32`, `--region`, budgets, `--existing PREFIX`, …) |
| `repeat` | `1` | measure N times (`oprobe run --repeat N`) and report the spread |
| `calibrate` | `auto` | learn the noise floor: `auto` (while the cached data directory has none), `true` (every job), `false` |
| `calibrate-repeat` | `4` | identical runs of the calibration (N − 1 A/A pairs) |
| `cache-data` | `true` | keep the data directory (noise floor, capacity history) in the Actions cache, per target and runner OS |
| `strict` | `true` | `oprobe compare --strict`: a different target or workload fails (exit 2) |
| `baseline` | | a `.oprobe` / report `.json` path in the workspace, or a **name** stored in the Actions cache |
| `update-baseline` | `auto` | replace a named baseline with this run: `auto` = pushes to the default branch that passed; `true` / `false` |
| `compare-args` | | e.g. `--max-p99-increase-pct 30 --allow-throttling` |
| `fail-on-regression` | `true` | |
| `comment` | `true` | sticky PR comment (needs `pull-requests: write`) |
| `artifact-name` | `oprobe-report` | also distinguishes comments when the action runs more than once |

Outputs: `run-id`, `run-file`, `html-report`, `run-exit-code`, `regression`.

### Baselines

- **A name** (`baseline: main`): the first passing push to the default branch stores its run in the Actions
  cache; later runs compare against it, and every passing push to the default branch replaces it. Pull
  requests never replace it. A cache entry is written only when the baseline is replaced, never for a run
  that merely restored it.
- **A file** (`baseline: benchmarks/baseline.oprobe`): commit a run file (or download one from an earlier
  artifact) and compare against it. Refresh it on purpose: `oprobe run … && cp <run file> benchmarks/`.

## Any CI

```bash
oprobe run "$TARGET" --preset small-files -d 20s --label "$COMMIT" --non-interactive
oprobe compare baseline.oprobe last --markdown gate.md --junit gate.xml   # exit 3 on regression
oprobe runs show last -o html > report.html
```

The run is saved under `$OPROBE_DATA_DIR` (set it to a cached directory to keep history and baselines between
jobs). In scripts, prefer `-o json` (one document) or `-o ndjson` (events, then the result).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | runtime failure (credentials, network, storage) |
| 2 | usage or configuration error |
| 3 | SLA breached (`run`), regression (`compare`), or `--fail-on` matched |
| 4 | a budget stopped the run |
| 5 | safety refusal (`--read-only`) |
| 130 | interrupted |

The default SLA of `oprobe run`: at most 1% failed operations and no corrupted data. Tighten it with
`--max-error-ratio 0.1%`, `--max-throttle-ratio 0%`, `--p99-ms 250`; `--sla none` reports without failing;
`--fail-on warning|critical` also fails on findings of that severity.

## What the gate checks

| Check | Default limit | Flag |
|---|---|---|
| same workload (`workload_hash`: dataset, mix, access, rate, warmup, duration) | must be identical | `--allow-workload-change` |
| same target (normalised target identity: scheme, bucket, prefix) | must be identical | `--allow-target-change` |
| throughput (ops/s, MiB/s) | −10 % | `--max-throughput-drop-pct` |
| p50 per operation | +20 % and > 1 ms | `--max-p50-increase-pct`, `--min-latency-delta-ms` |
| p99 per operation | +20 % and > 1 ms | `--max-p99-increase-pct` |
| failed operations | +0.5 percentage points | `--max-error-increase-pp` |
| new 429/503 throttling | not allowed | `--allow-throttling` |
| CRC32C corruption | must be 0 | always |
| requests per operation | +10 % | `--max-request-amp-increase-pct` |
| read amplification | +10 % | `--max-read-amp-increase-pct` |
| cost per 1M operations (when a price table applies to both) | +10 % | `--max-cost-increase-pct` |

Units are in the flag names (`_pct`: percent, `_pp`: percentage points, `_ms`); the 4.x names
(`--max-p99-increase`, …) still work as aliases. Operations with fewer than `--min-samples` (30) samples are
skipped for latency checks. Every setting that
differs is listed first. A different workload or target gives no regression verdict: the result is
`comparable: false` with the differing spec keys (exit 0 with a warning, exit 2 under `--strict`).
Different client knobs are an A/B comparison; a different host, proxy or oprobe version is a warning. Runs
replayed from one spec (`oprobe run --from RUN`) are always comparable.

## Keeping it stable

* Use `--max-ops` with an open-loop `--rate` (see `profiles/ci-gate.toml`): every run issues the same
  operation sequence (fixed seed) at the same arrival rate.
* Run baseline and candidate on the same runner class; shared runners are noisy, so keep latency limits wider
  than throughput limits.
* `memory://` and `fs://` gate client-side regressions (OpenDAL usage, request amplification); gate network
  behaviour against a dedicated bucket or `lab://`.
* Budgets keep a misconfigured job cheap: `--max-cost-usd`, `--max-requests`, `--max-duration`.

## Watching production

`oprobe watch TARGET --slo 'p99<200ms' --slo 'errors<1%'` probes at a low open-loop rate every `--interval`
and checks the SLOs over a rolling `--window` (exit 3 when one is breached at the end).

* `--listen 127.0.0.1:9464` serves Prometheus text on `/metrics` — every series labelled `target`, HELP text
  on every metric, `oprobe_build_info{version}` and `oprobe_last_success_timestamp_seconds` (the end of the
  last window without failed operations) — and `/healthz` (200, or 503 when no window succeeded within three
  intervals).
* `--otlp-endpoint http://collector:4318` pushes OTLP metrics after every window as one cumulative stream per
  session: a stable `service.instance.id` (from the host and the target) and `oprobe.target` on the resource,
  never a run id. The body is protobuf (`OTEL_EXPORTER_OTLP_PROTOCOL=http/json` for JSON) and
  `OTEL_EXPORTER_OTLP_HEADERS` (e.g. `api-key=…`) adds headers, as for any OTel SDK.
