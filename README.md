# oprobe

**See which layer makes your object-storage I/O slow or expensive — then fix it.**

`oprobe` drives realistic workloads through [Apache OpenDAL](https://opendal.apache.org/) against
S3, GCS, Azure Blob, R2, OSS, COS, B2, MinIO or local storage and traces **every backend request**
at the transport level: time spent queued, DNS/TCP/proxy/TLS on fresh connections, sending the
request, waiting for the first byte, transferring the body, and waiting for your code to consume
it. It then tells you where the time and the money go, finds the best client configuration
without tripping 429/503 throttling, and turns it all into a CI regression gate.

[中文文档](README.zh-CN.md) · [Desktop app](docs/app.md) · [Metrics & methodology](docs/metrics.md) · [Workloads & profiles](docs/workloads.md) · [Tuning](docs/tuning.md) · [CI gate](docs/ci.md) · [Cost model](docs/cost.md) · [Troubleshooting](docs/troubleshooting.md)

```text
$ oprobe run s3://noaa-ghcn-pds --anonymous --existing csv.gz/ --preset mixed -d 8s -c 8

[REVIEW] Server latency (TTFB)-bound: 81% of operation time is spent in server latency (TTFB) (p50 70.4ms / p99 281ms).

  throughput 89.8 ops/s · 92.2 MiB/s   latency p50 70.4ms · p99 281ms · p99.9 1.93s   errors 0 · 0 throttled

WHERE THE TIME GOES  (share of operation wall time)
  connect   ▍                              1.0%  DNS + TCP + proxy + TLS on fresh connections
  server    ████████████████████████████  81.0%  time to first byte (server processing + RTT)
  download  ██████▏                       17.6%  receiving response bodies

BACKEND REQUESTS  (mean per request; p99 of total)
  request     count  warm   queue  connect    send    ttfb  transfer   stall    p99   status
  GetObject     717   99%  0.01ms    213ms  0.15ms  86.2ms    19.9ms  0.00ms  414ms  206×717
  HeadObject     91  100%  0.01ms        -  1.03ms  44.9ms    0.00ms  0.00ms  77.2ms  200×91

SLOWEST OPERATION
  read.full 1.92s at 4.0s  queue 0.01ms · backend 1.92s · client 0.78ms
    #2  +0.05ms  GetObject  206 h1 conn#12 cold  connect 72.1ms (dns 0.00ms tcp 0.10ms proxy 9.37ms tls 62.6ms) · ttfb 94.2ms · transfer 248ms  req-id PY6T4PG8JQ00QR2G
    #3  +0.08ms  GetObject  206 h1 conn#13 cold  connect 367ms (dns 0.00ms tcp 0.08ms proxy 287ms tls 80.9ms) · ttfb 68.3ms · transfer 261ms  req-id DRM21XWEN03GHVD7
    async call stack captured while in flight:
      read.full [156.000ms]
        [sub-task 0] opendal::background_task → opendal::read → http::checkout → proxy CONNECT s3.us-east-1.amazonaws.com:443
```
*(real output, trimmed: AWS S3 through a corporate egress proxy)*

## Desktop & terminal app

`oprobe-app` puts the same engine behind a native UI built with [Native Shell (NSH)](https://github.com/longbridge/nsh):
AppKit on macOS, Win32/Direct2D on Windows, GTK 4 on Linux, and the same UI in any terminal (`oprobe-app --tui`).
Run, tune and probe from forms; watch live charts; inspect the slowest operation as a request waterfall.
`oprobe top` opens it in the terminal. [More →](docs/app.md)

The architecture has two layers and no other frameworks: **OpenDAL + oprobe's pure-Rust transport** for everything that
measures, and **NSH** for everything you look at.

![oprobe-app: slowest operation waterfall](docs/images/app-slowest-op.png)

## Why oprobe

Black-box benchmarks report "120 MiB/s, p99 280 ms" and leave you guessing. oprobe answers the
next questions directly from measurements — never from estimates:

| Question | How oprobe answers it |
|---|---|
| Is it the network, the server, or my client? | Every operation's wall time is split into **queue / connect / upload / server TTFB / download / client** using the real request timeline ([how](docs/metrics.md)). |
| Why is p99 so much worse than p50? | The slowest operations are kept with **every backend request's stage timings, connection (cold/warm), status, request ID** and an **async call stack** captured while they were stuck. |
| Are connections reused? Is TLS or the proxy slow? | Per-dial DNS / TCP / proxy CONNECT / TLS histograms, ALPN, TLS version, cipher, reuse ratio, peak in-flight requests. |
| Am I paying for bytes I don't use? | Logical vs backend vs wire bytes: **read amplification, over-read from range coalescing, requests per operation**. |
| What will this cost per month? | Measured request mix × provider list prices (S3, GCS, Azure, R2, B2, OSS), waste from failed/throttled requests and over-reads, savings from caching. |
| What settings should I ship? | `oprobe tune` runs real trials, **detects the 429/503 ceiling**, backs off, refines chunk/gap/parallelism/cache/HTTP version and emits a profile plus Rust/Python OpenDAL code. |
| Did my change make storage I/O worse? | `oprobe diff` compares JSON reports with noise floors and exits non-zero; Markdown for PR comments, JUnit for CI dashboards. |

## Install

| Platform | Command |
|---|---|
| Any (Rust ≥ 1.91) | `cargo install oprobe --locked` |
| macOS / Linux (Homebrew) | each release ships `oprobe.rb` for a tap — see [docs/install.md](docs/install.md) |
| macOS / Linux (script) | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| macOS installer | `oprobe-<version>-macos-universal.pkg` (Apple Silicon + Intel) |
| Debian / Ubuntu | `sudo apt install ./oprobe_<version>-1_amd64.deb` (or `_arm64`) |
| Fedora / RHEL / SUSE | `sudo dnf install ./oprobe-<version>-1.x86_64.rpm` (or `.aarch64`) |
| Windows (installer) | `oprobe-<version>-x64.msi` / `-arm64.msi` — adds `oprobe` to `PATH` |
| Windows (Scoop) | `scoop install https://github.com/hxddh/oprobe/releases/latest/download/oprobe.json` |
| Windows (PowerShell) | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex` |
| **Desktop app** | macOS `oprobe-app-<version>-macos-universal.dmg` · Windows `oprobe-app-<version>-x64.msi` · Linux `oprobe-app-<version>-x86_64-unknown-linux-gnu.tar.gz` |

Release archives (`oprobe-<version>-<target>.tar.gz` / `.zip`) include man pages, shell completions
(bash, zsh, fish, PowerShell, elvish) and example profiles. Every asset is listed in `SHA256SUMS`.
oprobe is pure Rust — no OpenSSL, no C toolchain — and the Linux musl builds are fully static.

## Quick start

```bash
# 1. Zero setup: in-memory backend
oprobe run memory://

# 2. Reproduce storage problems locally: an S3-compatible lab with latency and throttling
oprobe lab --latency 20ms --straggler-every 50 --throttle-above 16 &
AWS_ACCESS_KEY_ID=lab AWS_SECRET_ACCESS_KEY=lab \
  oprobe run s3://bench/demo --endpoint http://127.0.0.1:9000 --proxy none -c 32

# 3. Your bucket (credentials from the usual AWS/GCP/Azure environment)
oprobe doctor s3://my-bucket/oprobe            # connectivity, TLS, clock skew, permissions
oprobe run    s3://my-bucket/oprobe --preset parquet -d 30s --json baseline.json
oprobe run    s3://my-bucket --existing warehouse/events/   # read-only, your real objects
oprobe top    s3://my-bucket/oprobe            # live terminal workbench (needs oprobe-app)

# 4. Tune and ship
oprobe tune s3://my-bucket/oprobe --preset parquet --p99-ms 250 \
  --emit-profile tuned.toml --emit-rust src/storage.rs --emit-python storage.py

# 5. Gate regressions in CI
oprobe run --profile tuned.toml --json candidate.json
oprobe diff baseline.json candidate.json --junit oprobe.xml --markdown oprobe.md
```

All test data is written under a unique `oprobe-run-<timestamp>-<id>/` prefix and deleted (and
verified deleted) when the run ends — also on errors and Ctrl-C. `oprobe cleanup` sweeps leftovers
of killed runs.

## Commands

| Command | Purpose |
|---|---|
| `oprobe run [TARGET]` | Run a workload; print the white-box report; save `--json` / `--markdown`. `--fail-on warning\|critical` for CI. |
| `oprobe top [TARGET]` | The app's interactive workbench in the terminal (`oprobe-app --tui`): live charts, time breakdown, per-API tables, slowest-op waterfall, tuner, probe. |
| `oprobe tune [TARGET]` | Throttle-aware closed-loop tuning with `--objective throughput\|latency\|cost`. |
| `oprobe diff BASE CAND` | Regression gate (exit code 3 on regression) with JSON / Markdown / JUnit output. |
| `oprobe doctor [TARGET]` | Step-by-step checks: config, credentials, DNS/proxy/TCP/TLS, HTTP, clock skew, list/write/read/stat/delete. |
| `oprobe probe URL` | Handshake microscope: DNS, TCP, proxy CONNECT, TLS, cold vs warm TTFB, clock skew. |
| `oprobe lab` | Local S3-compatible server (HTTP/1.1, HTTP/2, TLS 1.3) with latency, stragglers, bandwidth caps, slow multipart completion and 429/503 throttling. |
| `oprobe cleanup [TARGET]` | Remove leftovers from interrupted runs (`--dry-run`, `--older-than`, `--datasets`). |
| `oprobe init` | Print a fully commented profile for a preset. |
| `oprobe report FILE` | Re-render a saved JSON report as text or Markdown. |
| `oprobe completions SHELL` | Shell completions. |

## Workloads

Five presets model real access patterns; each can be customised with an operation mix, dataset
shape, key popularity (`uniform`, `zipf:1.1`, `hotset:0.2/0.8`), closed-loop concurrency or
open-loop arrival rate (latency measured from the intended start — no coordinated omission).

| Preset | Models |
|---|---|
| `parquet` | Lakehouse scans: `stat` + footer read + vectored column-chunk range reads with gap coalescing |
| `checkpoint` | ML checkpoints: large multipart uploads plus full restores |
| `small-files` | Many small objects: whole-object GET/PUT and HEAD |
| `metadata` | Partition discovery: LIST + HEAD storms |
| `mixed` | General purpose: 70 % reads (range + full), 20 % writes, 10 % stat |

Use `--existing <prefix>` to benchmark objects you already have, read-only. See
[docs/workloads.md](docs/workloads.md) for the profile format.

## How it works

oprobe installs its own HTTP transport (hyper + rustls, pure Rust) into OpenDAL and wraps it with
a scope that follows each logical operation through OpenDAL's layers **and** its background tasks
(concurrent readers/writers). Each request records sequential, non-overlapping stages; each
operation's wall time is attributed by sweeping its request intervals. Connections are pooled by
oprobe itself, so "cold" vs "warm" is exact, as are dial phases and in-flight caps. Nothing is
estimated or simulated. Details and caveats (e.g. HTTP/2 multiplexing) are in
[docs/metrics.md](docs/metrics.md).

## License

Apache-2.0. oprobe is not affiliated with the Apache Software Foundation; it builds on Apache OpenDAL.
