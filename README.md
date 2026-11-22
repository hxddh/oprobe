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
oprobe prove last throttling     # A/B-test the remedy of the throttling finding
oprobe                           # the app: runs, profile, operator, compare
```

`oprobe demo` is `oprobe run lab://demo`: a local S3 that throttles above 24 requests in flight and
turns some requests into stragglers. The brief names at most three findings, each with its remedy
as an `op.toml` path, and the next command. `prove` ends with the diff, for example:

```text
proved on goodput: +31 % [+27, +35], p99 −42 %, 0 throttled (4 A/B pairs)
+ ConcurrentLimitLayer::new(24)
~ RetryLayer::new().with_jitter().with_max_times(4)
```

Then point it at your own storage:

```bash
oprobe run s3://my-bucket/oprobe --set options.region=eu-west-1
oprobe run op.toml --preset range-reads           # your operator, a parquet-like workload
oprobe run s3://a/x r2://b/x fs:///nvme/x         # several operators, one workload: a race
```

Writes go under a fresh `oprobe-run-<id>/` prefix and are cleaned up; `--dry-run` prints the plan,
`--read-only` refuses every write, and `--max-requests/--max-bytes/--max-duration` are hard budgets.

## Your operator: `op.toml`

The unit of everything is the OpenDAL `Operator`: a service, a layer stack, and read/write options.
oprobe reads it from a URL, from a file, or from `[operators.NAME]` in its config file.

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
| `bandwidth_cap` | a bytes-per-second ceiling | `layers.throttle.bandwidth` |
| `tail_stragglers` | a few very slow server responses | `layers.tail_cut` |
| `hung_requests` | requests that never finish | `layers.timeout.io` |
| `small_ranges` | many small range reads | `read.gap` / `read.concurrent` |
| `slow_multipart` | multipart uploads that crawl | `write.chunk` / `write.concurrent` |
| `repeated_reads` | the same bytes read again and again | `layers.cache` (`FoyerLayer`) |
| `cold_connections` | handshakes on the critical path, HTTP version | `http.keep_alive` / `http.version` |
| `client_bound` | your process is the bottleneck | none in storage; reported as such |

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
stack beside each remedy: toggle a layer, press *Prove*, watch the A/B pairs stream in, copy the
diff) and **Compare**. It speaks 简体中文 and English. [More →](docs/app.md)

## For scripts and agents

Every command prints one JSON document with `-o json` (stdout carries only the result; progress
goes to stderr), errors carry stable codes and exit codes (0 ok · 1 runtime · 2 usage · 3 prove
refuted · 4 budget · 5 safety · 130 interrupted), and the JSON Schemas are in [`schemas/`](schemas).
Agents: [`skills/oprobe/SKILL.md`](skills/oprobe/SKILL.md) and [llms.txt](llms.txt).

## Install

| Platform | |
|---|---|
| macOS / Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` (`-s -- --cli` for the CLI without the app) |
| Homebrew | `brew install hxddh/tap/oprobe` (the app bundle: `brew install --cask hxddh/tap/oprobe-app`) |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex`, the `.msi`, Scoop (`scoop bucket add hxddh https://github.com/hxddh/scoop-bucket`, `scoop install oprobe`) or `winget install hxddh.oprobe` |
| Debian / Ubuntu, Fedora / RHEL | `.deb` / `.rpm` from the release page |
| Rust | `cargo install oprobe --locked` (`--no-default-features --features cli` for the CLI only, `--features gui` for native windows on Linux) |

The macOS and Windows packages are **not code-signed**: the first open needs one confirmation
([how](docs/install.md#first-open-of-unsigned-packages)). Every asset has a SHA-256 in `SHA256SUMS`
and a SLSA build provenance attestation; the install scripts check the SHA-256 and, when the GitHub
CLI is logged in, the attestation ([verifying](docs/install.md#verifying-a-download)).

oprobe sends no telemetry and talks only to the storage you point it at. [Privacy →](docs/privacy.md)

## License

Apache-2.0
