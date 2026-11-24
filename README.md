# oprobe

> **oprobe is a profiler for OpenDAL operators.**
> It shows what your `Operator` really does on the wire, names the layer that fixes it, and proves
> the fix before you ship it.

A program has `perf` and flame graphs. An [Apache OpenDAL](https://opendal.apache.org/) operator
has oprobe. It traces every backend request under every operation, stage by stage (queue, connect,
send, time to first byte, body, retry backoff), explains what it finds as **one OpenDAL change**,
and proves that change with an interleaved A/B test on the objective you care about.

[中文](README.zh-CN.md) · [Getting started](docs/getting-started.md) · [Concepts](docs/concepts.md) ·
[op.toml](docs/operator.md) · [CLI](docs/cli.md) · [Reading the profile](docs/reading-the-profile.md) ·
[Embedding](docs/embedding.md) · [App](docs/app.md) · [Install](docs/install.md) ·
[Troubleshooting](docs/troubleshooting.md)

## The loop: Profile → Explain → Prove

1. **Profile.** Drive a workload through an operator (`oprobe run s3://bucket/prefix`), or open a
   `.oprobe` file that [`oprobe-trace`](docs/embedding.md) wrote in production.
2. **Explain.** Each finding names a cause, shows the requests that are its evidence, and names the
   one OpenDAL remedy. It gives no numbers: a single run varies no load and cannot size a change.
3. **Prove.** `oprobe prove` runs the current operator and the one with the remedy interleaved
   (A B A B …), sizes the remedy where it has a size, and decides on your objective (goodput by
   default, or p99). The result is a **proven layer diff** you can copy.

## Start in 60 seconds

```bash
# install (macOS / Linux; Windows, packages and cargo: docs/install.md)
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh

oprobe demo                      # profile a misbehaving S3 lab on your laptop: no account needed
oprobe prove last                # A/B-test the remedy of the top finding (about a minute)
oprobe                           # the app: runs, profile, operator, compare
```

`oprobe demo` is `oprobe run lab://demo`: a local S3 that makes every 40th request a straggler
that waits 250 ms (and throttles above 40 requests in flight). The brief names at most three findings,
each with its remedy as an `op.toml` path, and the next command:

```text
[Attention] 2.4% of HeadObject, 2.4% of PutObject and 2.6% of GetObject requests wait far longer for
  the server's first byte than their peers (stragglers).
  lab://demo · small-files · 8.0 s · run 20261123-135409-4b57
  ▲ 1 tail_stragglers (high confidence) — 2.4% of HeadObject, 2.4% of PutObject and 2.6% of
      GetObject requests wait far longer for the server's first byte than their peers (stragglers).
      33 HeadObject stragglers wait ~260ms for the first byte against ~9.48ms for a typical
      HeadObject; 100 PutObject stragglers wait ~260ms for the first byte against ~9.55ms for a
      typical PutObject; 214 GetObject stragglers wait ~260ms for the first byte against ~9.54ms for
      a typical GetObject; operation p50 9.65ms · p99 261ms
      remedy: layers.tail_cut + layers.retry.min_delay — re-issue requests far slower than their
        peers, after a short retry backoff (TailCutLayer + RetryLayer::with_min_delay)
      next: oprobe prove 20261123-135409-4b57 tail_stragglers
  1733.6 ops/s · 97.6 MiB/s · p50 9.65ms · p99 261ms · errors 0.00%
  full profile: oprobe show 20261123-135409-4b57 --detail · terms: https://github.com/hxddh/oprobe/blob/v7.0.0/docs/concepts.md#glossary
```

`prove last` runs trials of the current operator (A) and of the remedy (B), interleaved, and ends
with the diff (this run took 54 s):

```text
improved on goodput: +33.3 %, 95 % interval [+26.9, +40.1] (3 pairs of trials, noise ±2.0 %) · lab://demo (local lab)
secondary (not decisive; 95 % intervals): p99 -68.5 % [-70.0, -66.9] · throttled 0.00 % → 0.00 % · errors 0.00 % → 0.00 %
note: TailCutLayer re-issues through RetryLayer: its first backoff is shortened so a cut request is retried at once; trials warm up 11.5 s because the layer ignores its first 10 s
+ TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(60)).build()
~ RetryLayer::new().with_min_delay(Duration::from_millis(20))
op.toml:
[layers]
retry = { min_delay = "20ms" }
tail_cut = true
```

Then point it at your own storage:

```bash
oprobe run s3://my-bucket/oprobe --set options.region=eu-west-1
oprobe run op.toml --preset range-reads           # your operator, a parquet-like workload
oprobe run s3://a/x r2://b/x fs:///nvme/x         # several operators, one workload: a race
```

Writes go under a fresh `oprobe-run-<id>/` prefix and are cleaned up; `--dry-run` prints the plan,
`--read-only` refuses every write, and `--max-requests/--max-bytes/--max-duration` are hard budgets
(for `prove`, `--max-duration` is the wall-time budget of the whole proof).

## Your operator: `op.toml`

The unit of everything is the OpenDAL `Operator`: a service, a layer stack, and read/write options.
oprobe reads it from a URL or from a file, `op.toml` (the recommended way to describe yours).

```toml
url = "s3://bucket/prefix"
[options]                       # OpenDAL service options
region = "eu-west-1"
[layers]                        # each key is one OpenDAL layer; absent = not in the stack
concurrent_limit = { http = 24 }
retry = { max_times = 3, jitter = true }
timeout = { op = "30s", io = "10s" }
[read]
chunk = "8MiB"
concurrent = 4
[http]
version = "auto"                # auto | h1 | h2
```

`--set layers.retry.max_times=5` or `--set read.gap=1MiB` changes one value. The diff between two
operators renders as Rust and as TOML. [The reference →](docs/operator.md)

## Findings and their remedy

| Finding | Cause | Remedy (`op.toml` path) |
|---|---|---|
| `throttling` | 429/503 above an in-flight level | `layers.concurrent_limit.http`, plus `layers.retry.jitter` |
| `rate_limit` | a request-rate ceiling | `layers.concurrent_limit.http` |
| `key_limit` | a hot prefix or key | the key layout (advice, not a layer) |
| `bandwidth_cap` | a bytes-per-second ceiling | `layers.throttle.bandwidth` on writes; on reads not a layer |
| `tail_stragglers` | a few very slow server responses | `layers.tail_cut` |
| `hung_requests` | requests that never finish | `layers.timeout.io` |
| `small_ranges` | many small range reads | `read.gap` / `read.concurrent` |
| `slow_multipart` | multipart uploads that crawl | `write.chunk` / `write.concurrent` |
| `repeated_reads` | the same bytes read again and again | `layers.cache` (`FoyerLayer`) |
| `cold_connections` | handshakes on the critical path, HTTP version | `http.keep_alive` / `http.version` |
| `client_bound` | your process is the bottleneck | none in storage; reported as such |
| `errors` | 5xx or lost connections that retries recovered | `layers.retry.min_delay`, plus `layers.retry.jitter` |
| `errors` | failures that reached the application | none: fix their cause (`show RUN --by api`) |

`run` names the remedy; only `prove` puts a number on it.

## Profile production: `oprobe-trace`

Add one layer to your own operator and save a profile the workbench opens like any other run:

```rust
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = op.layer(layer);
handle.save("prod.oprobe")?;          // later: oprobe show prod.oprobe
```

`oprobe-trace` is an OpenDAL layer plus an instrumented `HttpFetch` (OpenDAL's own reqwest client,
observed). Memory stays bounded, the overhead is benchmarked (budget ≤ 2 µs per operation), and an
`Exporter` hook (with OpenTelemetry metrics behind the `otel` feature) sees every request.
[Embedding →](docs/embedding.md)

## The app

Run `oprobe` with no arguments: a native window when there is a display, the same app in your
terminal otherwise (also over SSH). Four views: **Runs**, **Profile** (verdict, where the time
went, the request waterfall of the slowest operations, timeline, heatmap), **Operator** (your layer
stack beside each remedy: toggle a layer, press *Prove*, watch the trials stream in, copy the
diff) and **Compare**. It speaks 简体中文 and English. [More →](docs/app.md)

## For scripts and agents

Every command prints one JSON document with `-o json` (stdout carries only the result; progress
goes to stderr), errors carry stable codes and exit codes (0 ok · 1 runtime · 2 usage · 3 prove: worse
· 4 budget · 5 safety · 130 interrupted), and the JSON Schemas are in [`schemas/`](schemas).
Agents: [`skills/oprobe/SKILL.md`](skills/oprobe/SKILL.md) and [llms.txt](llms.txt).

Two patterns cover CI:

```sh
# Fail the job when a profile is critical.
oprobe run s3://bucket/bench --existing data/ --max-requests 20000 -o json \
  | jq -e '.runs[0].report.verdict.level != "critical"'
# Fail the job when a change makes the operator worse: prove exits 3.
oprobe prove last --with layers.retry.max_times=5 --max-duration 5m -o json > prove.json
```

## Install

| Platform | |
|---|---|
| macOS / Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` (`-s -- --cli` for the CLI without the app) |
| Homebrew (when published) | `brew install hxddh/tap/oprobe` (the app bundle: `brew install --cask hxddh/tap/oprobe-app`) |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex`, the `.msi`, Scoop when published (`scoop bucket add hxddh https://github.com/hxddh/scoop-bucket`, `scoop install oprobe`) or `winget install hxddh.oprobe` |
| Debian / Ubuntu, Fedora / RHEL | `.deb` / `.rpm` from the release page |
| Rust (crates.io, when published) | `cargo install oprobe --locked` (`--no-default-features --features cli` for the CLI only, `--features gui` for native windows on Linux) |

The macOS and Windows packages are **not code-signed**: the first open needs one confirmation
([how](docs/install.md#first-open-of-unsigned-packages)). Every asset has a SHA-256 in `SHA256SUMS`
and a SLSA build provenance attestation; the install scripts check the SHA-256 and, when the GitHub
CLI is logged in, the attestation ([verifying](docs/install.md#verifying-a-download)).

oprobe sends no telemetry and talks only to the storage you point it at. [Privacy →](docs/privacy.md)

## License

Apache-2.0
