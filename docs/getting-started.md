# Getting started

Five minutes from install to a report on your own bucket.

## 1. Install

```bash
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh   # macOS, Linux
brew install hxddh/tap/oprobe                                                         # Homebrew
```

```powershell
irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 | iex       # Windows
```

The scripts offer to add the install directory to your `PATH` (Windows: the user `PATH` and the current
window). Other ways (Scoop, `.deb`/`.rpm`, `.msi`, `.dmg`, cargo; winget once accepted) are in
[install.md](install.md). Check it works:

```bash
oprobe --version
```

## 2. The demo

```bash
oprobe demo
```

No account, no flags. The demo is exactly `oprobe run lab://demo`: an S3-compatible lab inside oprobe's own
process that answers `503 SlowDown` above 24 requests in flight and delays 1 in 40 requests by 250 ms,
measured with the scenario's default workload (small files: 64 objects of 64 KiB, 32 workers, 0.5 s warmup,
8 seconds). It narrates the result:

1. **What ran** — the workload and its headline numbers.
2. **The verdict** — where the time went, in one sentence.
3. **What oprobe found** — each finding with its evidence, what to do, and a command to check it.
4. **Where the time went** — the shares of operation time, retries, stragglers.
5. **Look closer** — the commands for the full report, the HTML report and the app.

Then it suggests what to try next. Each suggestion replays the demo run from its stored spec, so the results
compare like for like:

```bash
oprobe run --from RUN                       # the same run again (same target, workload, seed)
oprobe compare RUN last                     # comparable: same target identity and workload
oprobe verify RUN throttling                # does the advice hold? A/B replays, then a verdict
oprobe run --from RUN s3://my-bucket/oprobe --region eu-west-1   # the same workload on your bucket
```

`oprobe run lab://demo` with no flags measures the same thing as the demo; other lab scenarios have their own
defaults (`lab://parquet` runs the parquet preset). The demo run is saved like any other (`oprobe runs list`).

## 3. Your bucket

Save the bucket as a named target. `target add` runs a quick read-only check (reachability, TLS, clock,
credentials, listing) right away:

```bash
oprobe target add prod s3://my-bucket/oprobe --region eu-west-1
oprobe target add prod s3://my-bucket/oprobe --credentials profile:prod     # an AWS profile
oprobe target add r2 s3://bucket/x --endpoint https://<account>.r2.cloudflarestorage.com --region auto
oprobe target add gcs gcs://my-bucket/oprobe
oprobe target add az azblob://container/oprobe --endpoint https://<account>.blob.core.windows.net
```

Or run `oprobe run` with no target in a terminal: it lists what it finds on this machine (AWS profiles in
`~/.aws/config` and `~/.aws/credentials`, Google application default credentials, `AZURE_STORAGE_*`
variables), asks for the bucket and saves the target for you. `oprobe target discover` lists the same
credential sources (names only, never secrets) without asking anything.

Then measure:

```bash
oprobe target test prod        # read-only doctor; --write-probe also writes, reads and deletes a probe object
oprobe run prod                # 10 s of the parquet preset with 16 workers
oprobe run prod --preset small-files -c 64 -d 30s
oprobe run prod --existing tables/events/        # read-only, on objects that already exist
oprobe run prod --dry-run                        # the plan: requests, bytes, cost, ETA; sends nothing
```

`oprobe run` writes its test data under `<prefix>/oprobe-run-<id>/` and deletes it at the end (verified).
`--read-only` or `--existing PREFIX` never write. Budgets stop a run early: `--max-cost-usd 0.50`,
`--max-requests 100000`, `--max-bytes 10GiB`, `--max-duration 5m`.

## 4. Read the result

The run prints at most 20 lines: verdict, top findings, key numbers and the command for the full report.

```bash
oprobe runs show                       # the last run, full report (80 columns)
oprobe runs show last --detail         # every column, the timeline, the 5 slowest operations
oprobe runs show last -o html > r.html # charts and every request, one offline file
oprobe runs open last                  # in the app
```

[Reading the report](reading-the-report.md) walks through every section.

## 5. Act on it

Every finding that suggests a change carries a hypothesis: the change (`--set key=value`), the command that
tests it and the expected result with an interval. Concurrency advice comes from the capacity model: the
in-flight level where the service starts to throttle, minus 15 % headroom, divided by the requests one
operation keeps in flight.

```bash
oprobe verify last throttling                    # replay with the finding's change, A B A B…: confirmed / refuted / inconclusive
oprobe verify last --all                         # every finding with a change
oprobe run --from last --set concurrency=24      # replay with your own change
oprobe compare last~1 last                       # same target and workload, or no verdict (exit 3 on regression)
oprobe fix prod                                  # diagnose → tune → verify: a profile patch and code
oprobe tune prod --emit-profile tuned.toml --emit-rust storage.rs    # best goodput within the throttle budget
```

`compare` judges only runs of the same target and the same workload (`workload_hash`); otherwise it reports
`comparable: false` and lists what differs. Replays (`--from`) are always comparable.

Next: [concepts](concepts.md) (targets, runs, baselines, findings), [workloads](workloads.md),
[the CI gate](ci.md).
