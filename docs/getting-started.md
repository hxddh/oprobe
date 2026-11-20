# Getting started

Five minutes from install to a report on your own bucket.

## 1. Install

```bash
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh   # macOS, Linux
winget install hxddh.oprobe                                                           # Windows
```

Other ways (Homebrew, `.deb`/`.rpm`, `.msi`, `.dmg`, cargo) are in [install.md](install.md). Check it works:

```bash
oprobe --version
```

## 2. The demo

```bash
oprobe demo
```

No account, no flags. oprobe starts an S3-compatible lab inside its own process that answers `503 SlowDown`
above 24 requests in flight and delays 1 in 40 requests by 250 ms, measures it with 32 workers for 8 seconds,
and narrates the result:

1. **What ran** — the workload and its headline numbers.
2. **The verdict** — where the time went, in one sentence.
3. **What oprobe found** — each finding with its evidence, what to do, and a command to check it.
4. **Where the time went** — the shares of operation time, retries, stragglers.
5. **Look closer** — the commands for the full report, the HTML report and the app.

The demo run is saved like any other (`oprobe runs list`).

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
variables), asks for the bucket and saves the target for you.

Then measure:

```bash
oprobe target test prod        # full doctor: also writes, reads and deletes a probe object
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

```bash
oprobe tune prod --emit-profile tuned.toml --emit-rust storage.rs    # best knobs below the throttling ceiling
oprobe run prod -c 32 --label after
oprobe compare before after                                          # did it help? (exit 3 on regression)
```

Next: [concepts](concepts.md) (targets, runs, baselines, findings), [workloads](workloads.md),
[the CI gate](ci.md).
