# CI regression gate

Store a baseline report, produce a candidate on every change, and let `oprobe diff` decide.

```bash
oprobe run --profile profiles/ci-gate.toml --json baseline.json     # once, commit or cache it
oprobe run --profile profiles/ci-gate.toml --json candidate.json    # in CI
oprobe diff baseline.json candidate.json --markdown gate.md --junit gate.xml
```

Exit codes: `0` pass · `3` regression · `1` error (bad input, unreachable target).

## Checks

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
| requests per op | +10 % | `--max-request-amp-increase` |
| read amplification | +10 % | `--max-read-amp-increase` |
| cost per 1M ops (when a price table applies to both) | +10 % | `--max-cost-increase` |

Operations with fewer than `--min-samples` (30) samples in either report are skipped for latency
checks.

Every setting that differs between the two reports is listed first under **CONFIGURATION
CHANGES** (also `config_changes` in `--json`, and in the Markdown summary): a different workload
or target fails the gate (the numbers are not comparable), different client knobs (e.g.
`knobs.concurrency 16 → 64`) are reported as an A/B comparison, and a different host, proxy or
oprobe version is a warning. The measurement window (`duration`, `warmup`, `max_ops`) is not a
workload change.

## Determinism tips

* Use `max_ops` with an open-loop `rate` (see `profiles/ci-gate.toml`): every run issues the same
  operation sequence (fixed `seed`) at the same arrival rate.
* Run baseline and candidate on the same runner class; shared CI runners are noisy, so keep the
  latency limits wider than throughput limits.
* `memory://` and `fs://` gate *client-side* regressions (OpenDAL usage, request amplification).
  Gate network behaviour against a dedicated bucket or `oprobe lab`.

## GitHub Actions

```yaml
- name: Storage benchmark
  run: |
    cargo install oprobe --locked
    oprobe run --profile profiles/ci-gate.toml --json candidate.json -q --label "$GITHUB_SHA"
- name: Storage gate
  run: |
    oprobe diff benchmarks/baseline.json candidate.json \
      --markdown gate.md --junit oprobe-gate.xml
- if: always()
  run: cat gate.md >> "$GITHUB_STEP_SUMMARY"
```

`oprobe run` itself fails the job (exit code 3) when the run breaches its SLA, even without a
baseline. The default SLA is: at most 1% failed operations and no corrupted data. Tighten or relax
it in the profile's `[sla]` or with `--max-error-rate 0.1%`, `--max-throttle-rate 0%`,
`--p99-ms 250`; `--sla none` reports the verdict without failing. `--fail-on warning|critical`
additionally fails the job when the diagnosis has findings of that severity (e.g. throttling).
