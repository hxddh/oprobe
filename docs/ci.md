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
      - uses: hxddh/oprobe@v3
        with:
          target: s3://ci-bucket/oprobe
          preset: small-files
          duration: 20s
          args: -c 16 --max-cost-usd 0.25
          baseline: main            # a name: kept in the Actions cache, refreshed by pushes to main
```

What it does:

1. installs the pinned oprobe release (`version`, default the release that matches the action tag) after
   checking its SHA-256 against the release's `SHA256SUMS`;
2. `oprobe run TARGET` with the preset, duration and extra `args`, labelled with the commit SHA;
3. compares the run with the baseline (`oprobe compare`), writing Markdown and JUnit;
4. writes the comparison and the report to the job summary;
5. uploads an artifact with the `.oprobe` run file, `oprobe-report.html`, `oprobe-report.md` and the gate
   files — open the run later with `oprobe runs show oprobe-report/<id>.oprobe` or `oprobe runs open …`;
6. posts one pull-request comment and updates it on every push (it finds its own comment by a marker);
7. fails the step on a regression (exit 3) or when the run breaches its SLA (`fail-on-regression: false`
   only reports).

### Inputs

| Input | Default | |
|---|---|---|
| `target` | — | URL or configured name; `memory://` and `lab://demo` need no credentials |
| `version` | the action's release | `latest` follows new releases |
| `preset`, `duration` | preset's default, `20s` | |
| `args` | | extra `oprobe run` flags (`-c 32`, `--region`, budgets, `--existing PREFIX`, …) |
| `baseline` | | a `.oprobe` / report `.json` path in the workspace, or a **name** stored in the Actions cache |
| `update-baseline` | `auto` | replace a named baseline with this run: `auto` = pushes to the default branch that passed; `true` / `false` |
| `compare-args` | | e.g. `--max-p99-increase 30 --allow-throttling` |
| `fail-on-regression` | `true` | |
| `comment` | `true` | sticky PR comment (needs `pull-requests: write`) |
| `artifact-name` | `oprobe-report` | also distinguishes comments when the action runs more than once |

Outputs: `run-id`, `run-file`, `html-report`, `run-exit-code`, `regression`.

### Baselines

- **A name** (`baseline: main`): the first passing push to the default branch stores its run in the Actions
  cache; later runs compare against it, and every passing push to the default branch replaces it. Pull
  requests never replace it.
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
`--max-error-rate 0.1%`, `--max-throttle-rate 0%`, `--p99-ms 250`; `--sla none` reports without failing;
`--fail-on warning|critical` also fails on findings of that severity.

## What the gate checks

| Check | Default limit | Flag |
|---|---|---|
| same workload (preset, mix, dataset, access, rate, verify, seed) | must be identical | `--allow-workload-change` |
| same target (scheme, bucket/prefix, endpoint, options) | must be identical | `--allow-target-change` |
| throughput (ops/s, MiB/s) | −10 % | `--max-throughput-drop` |
| p50 per operation | +20 % and > 1 ms | `--max-p50-increase`, `--min-latency-delta-ms` |
| p99 per operation | +20 % and > 1 ms | `--max-p99-increase` |
| failed operations | +0.5 percentage points | `--max-error-increase` |
| new 429/503 throttling | not allowed | `--allow-throttling` |
| CRC32C corruption | must be 0 | always |
| requests per operation | +10 % | `--max-request-amp-increase` |
| read amplification | +10 % | `--max-read-amp-increase` |
| cost per 1M operations (when a price table applies to both) | +10 % | `--max-cost-increase` |

Operations with fewer than `--min-samples` (30) samples are skipped for latency checks. Every setting that
differs is listed first: a different workload or target fails (not comparable), different client knobs are
an A/B comparison, a different host, proxy or oprobe version is a warning.

## Keeping it stable

* Use `--max-ops` with an open-loop `--rate` (see `profiles/ci-gate.toml`): every run issues the same
  operation sequence (fixed seed) at the same arrival rate.
* Run baseline and candidate on the same runner class; shared runners are noisy, so keep latency limits wider
  than throughput limits.
* `memory://` and `fs://` gate client-side regressions (OpenDAL usage, request amplification); gate network
  behaviour against a dedicated bucket or `lab://`.
* Budgets keep a misconfigured job cheap: `--max-cost-usd`, `--max-requests`, `--max-duration`.
