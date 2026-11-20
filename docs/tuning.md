# Tuning

`oprobe tune` searches for the best client configuration **by measuring**, not by modelling. It
prepares the dataset once and then runs short trials (default 5 s + 1 s warmup) with different
knobs against the real target.

```bash
oprobe tune s3://bucket/oprobe --preset parquet --p99-ms 250 \
  --emit-profile tuned.toml --emit-rust storage.rs --emit-python storage.py --json tune.json
```

## Search strategy

1. **Baseline** — your current knobs.
2. **Concurrency ladder** — 2, 4, 8, … up to `--max-concurrency`. The ladder stops at the first
   trial that receives a 429/503 (the *throttling ceiling*), violates the SLA, or stops improving
   (two consecutive steps below +5 %).
3. **Ceiling refinement** — binary search between the last clean level and the ceiling.
   After every throttled trial the tuner pauses (`--cooldown`, or longer if that trial received
   `Retry-After`, capped at 60 s; `Retry-After` is honored even with `--cooldown 0`) so the next
   trial is not measured against a depleted rate limiter. Ctrl-C interrupts the pause.
4. **Safety margin** — the recommended concurrency is capped at 80 % of the ceiling to leave
   headroom for production variance.
5. **Workload-specific knobs** — coordinate descent over only the knobs that matter for the mix:
   `chunk_size` and `io_concurrency` for multipart writes and large reads, `range_gap` for vectored
   (Parquet) reads, the read cache for skewed access (or uniform access over your real objects with
   `existing_prefix` when they fit), and HTTP/1.1 vs HTTP/2 for HTTPS. A cache win that relies on
   the whole generated test dataset fitting in the cache is flagged as dataset-size-dependent.
   A change is accepted only if it improves the objective by more than 3 % (noise guard).
6. **Confirmation** — the winner runs again for twice the trial duration; the report you get is
   from that run.

A trial is *feasible* when its error rate, throttle rate and worst per-operation p99 satisfy
`[sla]`. Infeasible trials are never chosen if a feasible one exists. If none is feasible, the
tuner reports "no trial met the SLA" and picks the least-bad trial (not throttled if possible,
then the lowest error rate). The objective throughput counts successful operations only.

## Objectives

| `--objective` | Picks |
|---|---|
| `throughput` (default) | highest MiB/s (or ops/s for small-object workloads) |
| `latency` | lowest worst-case p99 among configs within 80 % of the best throughput |
| `cost` | lowest cost per million operations among configs within 80 % of the best throughput |

## Outputs

* `--emit-profile` — your profile with the winning `[knobs]`; run it with `oprobe run --profile`.
* `--emit-rust` — an OpenDAL 0.59 `Operator` builder with `RetryLayer`, `TimeoutLayer`,
  `ConcurrentLimitLayer` (+ HTTP limit) and reader/writer helpers using the tuned chunk/gap/concurrency.
* `--emit-python` — the same for the `opendal` Python package.
* `--json` — the full report of the confirmation run plus every trial (`tune.trials`).

## Budget

`--max-trials` (default 24) bounds the number of trials (the final confirmation run comes on
top); with 5 s trials the typical run takes
1–3 minutes. Use `--trial-duration 15s` for noisy networks. Every trial uses the same dataset;
the test data is removed at the end.
