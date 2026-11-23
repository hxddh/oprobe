---
name: oprobe
description: Profile an Apache OpenDAL operator (S3, GCS, Azure Blob, R2, MinIO, OSS, COS, B2, fs) with the oprobe CLI - what it does on the wire, which OpenDAL layer fixes it, and an A/B test that proves the fix. Use when asked why an OpenDAL-based service reads or writes storage slowly, which layer or option to change (ConcurrentLimitLayer, RetryLayer, TimeoutLayer, TailCutLayer, ThrottleLayer, FoyerLayer, read/write chunking, HTTP settings), or to open a .oprobe profile. Safe by default - plan first, run with budgets, never write without permission.
---

# oprobe

oprobe is a profiler for OpenDAL operators. Four commands: `run` (profile an operator on a
workload), `prove` (A/B-test a change and print the proven layer diff), `show` (a stored run or a
`.oprobe` file; `--list` for history) and `lab` (a local S3 with faults). `demo` is
`run lab://demo`. Every command prints one JSON document with `-o json`; schemas are in the
repository's `schemas/` (`report`, `prove`, `run-list`, `operator`, `error`, `events`).

## Rules

1. **Never write to a real bucket without the user's explicit permission.** `run` writes test
   objects under `<prefix>/oprobe-run-<id>/` and deletes them. Without permission use
   `--existing PREFIX` (read-only, existing objects), `lab://SCENARIO` or `memory://`.
2. **Plan first**: `oprobe run OPERATOR --dry-run -o json` shows requests, bytes, objects written
   and deleted, and the ETA. Tell the user before anything non-trivial.
3. **Always set budgets** on real storage: `--max-requests`, `--max-bytes`, `--max-duration`
   (exit 4, `safety.budget_exceeded`; the partial run is saved). For `prove`, `--max-duration`
   is the wall-time budget of the whole proof.
4. **Parse JSON, not text**: `-o json`. Branch on exit codes and `error.code`, never on prose.
5. Never pass credentials in arguments: they come from the environment (`AWS_*`,
   `GOOGLE_APPLICATION_CREDENTIALS`, `AZURE_STORAGE_*`) or from `[options]` in an operator file
   that references `${ENV_VARS}`.
6. `run` names a remedy but **no number**. Only `prove` measures a change; never present a run's
   remedy as a guaranteed improvement.
7. Change one thing at a time: `--set PATH=VALUE` on `run`, `--with PATH=VALUE` on `prove`.

## Workflow

```sh
oprobe demo -o json                                   # no credentials: what a profile looks like
oprobe run s3://bucket/prefix --existing data/ -c 16 -d 15s \
  --max-requests 20000 --max-duration 1m --dry-run -o json  # plan
oprobe run s3://bucket/prefix --existing data/ -c 16 -d 15s \
  --max-requests 20000 --max-duration 1m -o json            # profile (saves the run)
oprobe show last --by stage -o json                   # where the time went, per stage
oprobe show last --since 3s --until 8s -o json        # one time range (`range`), from the rows
oprobe prove last throttling --max-duration 5m -o json   # A/B-test the finding's remedy
oprobe prove last --with layers.tail_cut=on --objective p99 -o json   # a change of your own
oprobe show --list -o json                            # history (prove trials hidden)
oprobe show prod.oprobe -o json                       # a profile written by oprobe-trace
```

`OPERATOR` is a URL (`s3://bucket/prefix`, `gcs://…`, `azblob://…`, `fs:///path`, `memory://`,
`lab://demo`), an operator file (`op.toml`: `url`, `[options]`, `[layers]`, `[read]`, `[write]`,
`[http]`) or a name from the config file's `[operators.NAME]`. Several operators in one `run`
race on the same workload. Workload: `--preset small-files|large-objects|range-reads|list-stat`,
`-c`, `-d`, `--objects`, `--object-size`, `--rate`, `--seed`, `--existing PREFIX`.

`RUN` is a run id, an id prefix, `last` or `last~N`.

## Reading results

- `run` / `show` (`oprobe.report/6`): `verdict.level` (`ok`, `review`, `attention`,
  `critical`) and `verdict.headline`; `findings[]` with `code`, `severity`, `cause`, `evidence`
  (numbers, operation ids, time windows) and the remedy, an `op.toml` path.
- Finding codes and their remedy: `throttling` → `layers.concurrent_limit.http` (+
  `layers.retry.jitter`), `rate_limit` → `layers.concurrent_limit.http`, `key_limit` → key layout
  (advice), `bandwidth_cap` → `layers.throttle.bandwidth`, `tail_stragglers` → `layers.tail_cut`,
  `hung_requests` → `layers.timeout.io`, `small_ranges` → `read.gap` / `read.concurrent`,
  `slow_multipart` → `write.chunk` / `write.concurrent`, `repeated_reads` → `layers.cache`,
  `cold_connections` → `http.keep_alive` / `http.version`, `client_bound` → none (the client is
  the bottleneck); also `errors` and `invalid_run`.
- `prove` (`oprobe.prove`): `verdict` (`improved`, `worse`, `inconclusive`), `objective`,
  `objective_delta` (estimate with interval), `pairs_count`, `secondary[]` (shown, never
  decisive) and `diff` with the change as `rust` and `toml`. Hand the user the diff.

Exit codes: 0 ok · 1 runtime · 2 usage · 3 prove: the change is worse on the objective · 4 budget ·
5 safety refusal · 130 interrupted. Errors: `{"type":"error","error":{"code","message","hint",
"retryable","docs"}}`.

## Profiling production

To profile a service's real traffic, add the `oprobe-trace` layer to its operator
(`let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());`), save with
`handle.save("prod.oprobe")`, then `oprobe show prod.oprobe -o json`.
