# oprobe

**See every request your object-storage client makes. Know where the time and the money go, and what to change.**

oprobe drives realistic workloads through [Apache OpenDAL](https://opendal.apache.org/) against S3, GCS,
Azure Blob, R2, OSS, COS, B2, MinIO or local storage and traces **every backend request**: time queued,
DNS/TCP/proxy/TLS on new connections, sending, waiting for the first byte, transferring, and waiting for your
code. It names the cause of what it finds, backs it with evidence and says what to change — and then checks
that the change works. Every run is saved as one `.oprobe` file with its full spec: you can reopen it,
replay it exactly, compare it, export it or hand it to an agent.

It is built for people (a terminal, a native app, a single-file HTML report), for CI (a GitHub Action, exit
codes, Markdown/JUnit) and for agents (JSON everywhere, an event stream, an MCP server).

[中文](README.zh-CN.md) · [Getting started](docs/getting-started.md) · [Concepts](docs/concepts.md) ·
[CLI reference](docs/cli.md) · [Reading the report](docs/reading-the-report.md) · [Metrics](docs/metrics.md) ·
[Workloads](docs/workloads.md) · [CI](docs/ci.md) · [Install](docs/install.md) · [Troubleshooting](docs/troubleshooting.md)

## Start in 60 seconds

```bash
# 1. install (macOS / Linux; Windows and packages: docs/install.md)
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh

# 2. see what oprobe finds, with no account and no flags
oprobe demo

# 3. your bucket
oprobe target add prod s3://my-bucket/oprobe --region eu-west-1
oprobe run prod
```

`oprobe demo` is `oprobe run lab://demo`: a local S3 lab that throttles above 24 requests in flight and turns
1 in 40 requests into stragglers, measured with its default workload (small files, 32 workers, 8 seconds).
It explains, step by step, what it found, and suggests replays of the run (`oprobe run --from RUN`) so every
follow-up compares like for like. `oprobe run` without a target asks what to measure: the demo, the AWS
profiles, Google credentials or Azure variables it finds on your machine (`oprobe target discover`), or a
URL — and saves your choice as a named target.

```text
$ oprobe run prod
[CRITICAL] Throttling-bound: the service rejected 3.6% of requests with 429/503;
  ✖ 626 throttled responses (429/503)
    3.57% of requests throttled; 609 retries, 63.02s total backoff (25% of operat…
    → The service is rate-limiting this request rate. Lower `concurrency` / `max…
  ▲ `read.full` p99 is 50.9× its median
    p50 5.04ms · p99 257ms · max 663ms
    → See the slowest-operation timelines for the responsible stage. Server stra…
  2114.0 ops/s · 119.2 MiB/s · p50 5.04ms · p99 257ms · errors 0.10%
  626 throttled · SLA met
full report: oprobe runs show 20261120-084637-d873
```

The default output is at most 20 lines that fit 80 columns: the verdict, the top findings (cause · evidence ·
advice · next command), the key numbers, and where the full report is.

## Look closer

| | |
|---|---|
| `oprobe runs show last` | The full report in the terminal (80 columns; `--detail` for everything). |
| `oprobe runs show last -o html > report.html` | One self-contained HTML file: charts with real axes, a latency heatmap, the slowest operations as request waterfalls, every request in a filterable table. Works offline, light and dark, prints cleanly. |
| `oprobe runs open last` | The run in the native app. |
| `oprobe run --from last --set concurrency=16` | Replay a run from its stored spec — same workload, dataset, warmup, seed, endpoint, region — with one change. |
| `oprobe verify last throttling` | Test a finding's advice: replays baseline and change interleaved (A B A B…) and answers *confirmed*, *refuted* or *inconclusive* against the predicted numbers. |
| `oprobe fix prod` | Diagnose → tune → verify in one go: the recommended settings as a profile patch and code. |
| `oprobe compare base last` | A regression gate between two runs of the same target and workload (exit 3 on regression). |
| `oprobe tune prod` | The best goodput within a throttle budget, as a profile and Rust/Python code. |
| `oprobe runs list` | Every saved run: when, verdict, ops/s, p99, cost. |

## One model, one spec, one loop

- **One capacity model.** oprobe reconstructs the requests in flight over time and finds the level where the
  service starts to throttle (a ceiling with an interval, never a guess from mean latency × rate). Every
  recommendation that mentions concurrency — the findings, `tune`, `verify`, the app — comes from that one
  model and keeps a 15 % headroom below the ceiling, taking into account how many requests one operation
  keeps in flight.
- **One spec.** Every report carries the run's resolved spec. `oprobe run --from RUN` replays it, `--set
  key=value` changes one thing, and every suggested command is such a replay, so nothing (warmup, seed, mix,
  dataset, endpoint, region, HTTP version, rate) is silently dropped.
- **One loop.** A recommendation is a hypothesis with an expected result. `oprobe verify` measures it and
  says whether it held; `compare` refuses to judge runs of different targets or workloads.

![The HTML report of the demo run](docs/images/report-html.png)

## The app

Run `oprobe` with no arguments: a native window (AppKit on macOS, Win32/Direct2D on Windows, GTK 4 on Linux)
when there is a display, the same workbench in your terminal otherwise (`oprobe top`, also over SSH). Targets,
run history and baselines on the left; the run inspector with Overview, Requests, Timeline, Connections,
Findings and Cost; the selected request or operation on the right. [More →](docs/app.md)

![oprobe workbench](docs/images/app-workbench.png)

## What it answers

| Question | How |
|---|---|
| Network, server or my client? | Each operation's wall time split into queue / connect / upload / server / download / retry / client from the real request timeline. |
| Why is p99 so much worse than p50? | The slowest operations keep every request with its stages, connection (cold/warm), status and request ID, plus an async call stack captured while they were stuck. |
| Is it throttling? | 429/503 counts, Retry-After, retry backoff as its own share of time, the in-flight ceiling where it starts and the safe concurrency below it (the capacity model). |
| Are connections reused? Is TLS slow? | Per-dial DNS/TCP/proxy/TLS times, reuse ratio, peak in-flight requests, per remote address. |
| Am I paying for bytes I do not use? | Read amplification, over-read from range coalescing, requests per operation. |
| What will it cost? | The measured request mix × provider price tables, waste from failed requests and over-reads. |
| Did my change make storage I/O worse? | `oprobe compare` or the GitHub Action: noise floors, configuration differences, Markdown + JUnit. |

## In CI

```yaml
- uses: hxddh/oprobe@v4
  with:
    target: s3://ci-bucket/oprobe
    preset: small-files
    baseline: main
```

The action installs the newest oprobe 4.x (or the `version` you pin), runs, compares against the baseline,
uploads the `.oprobe` file and the HTML report, keeps one sticky PR comment up to date and fails on
regression. [CI guide →](docs/ci.md)

## For agents

Every command prints one JSON document with `-o json` and an event stream with `-o ndjson`; errors carry stable
codes and exit codes; `oprobe schema` prints the JSON Schemas; `oprobe mcp` is an MCP server (read-only unless
`--allow-write`). See [docs/agents.md](docs/agents.md) and [llms.txt](llms.txt).

## Install

| Platform | |
|---|---|
| macOS / Linux | `curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh \| sh` |
| Homebrew | `brew install hxddh/tap/oprobe` (the app: `brew install --cask hxddh/tap/oprobe-app`) |
| Windows | `irm https://github.com/hxddh/oprobe/releases/latest/download/install.ps1 \| iex`, the `.msi`, or Scoop (`scoop bucket add hxddh https://github.com/hxddh/scoop-bucket`, `scoop install oprobe`) |
| winget | `winget install hxddh.oprobe` once the package is accepted into winget-pkgs — [status](docs/install.md#windows) |
| Debian / Ubuntu, Fedora / RHEL | `.deb` / `.rpm` from the release page |
| Rust | `cargo install oprobe --locked` (`--features gui` for the window on Linux) |

The macOS and Windows packages are **not code-signed** (no certificates): the first open needs one
confirmation — [how](docs/install.md#first-open-of-unsigned-packages). Every asset has a SHA-256 in
`SHA256SUMS` and a signed SLSA build provenance attestation (`gh attestation verify`). Update with
`oprobe self-update` (or your package manager).

oprobe sends no telemetry. It talks only to the storage you point it at, and to GitHub when you run
`oprobe self-update`. [Privacy →](docs/privacy.md)

## License

Apache-2.0
