# Reading the profile

A profile comes in three shapes:

| | Command | For |
|---|---|---|
| The brief | `oprobe run …` (default output) | the answer: the verdict, at most three findings, the next command |
| The profile | `oprobe show [RUN\|FILE] [--detail] [--by stage\|api\|conn]` | the terminal, logs, issues |
| JSON | `-o json` on any command | scripts and agents ([`schemas/report.json`](../schemas/report.json)) |

The app's Profile view shows the same sections with charts ([app](app.md)).

## The brief

```text
ATTENTION  throttling-bound: 3.6 % of requests were answered 429/503
  throttling       626 throttled responses above 24 requests in flight (high confidence)
                   remedy: layers.concurrent_limit.http, plus layers.retry.jitter
  tail_stragglers  1 in 40 GetObject responses waits > 200 ms for its first byte
                   remedy: layers.tail_cut
  2114 ops/s · 119 MiB/s · p50 5.0 ms · p99 257 ms
next: oprobe prove last throttling
```

(Illustrative: the exact wording follows the version you run.)

- **The badge** is the verdict level: **OK**, **Review** (worth a look: informational
  findings, a partial run), **Attention** (warning findings), **Critical** (critical findings).
  JSON carries the same levels as `ok`, `review`, `attention`, `critical`.
- **The headline** says what bounds the operator on this workload: throttling, the server's
  latency, the network (connect, upload, download), queueing in the client, or the client itself.
- **Findings**: at most three, most severe first. Each has its code, the evidence in one line,
  its confidence, and the remedy as an `op.toml` path, with no number: only `prove` sizes a
  change.
- **Key numbers**: goodput (successful operations per second), throughput, latency percentiles.
- **Next**: the command that proves the top remedy.

## The profile

`oprobe show` prints these sections (`--detail` adds every column, the per-second timeline,
the configuration and the five slowest operations):

| Section | What it tells you |
|---|---|
| header | run id, operator, workload, window, concurrency, start time |
| verdict | the badge and where the time went, in one sentence |
| key numbers | goodput and throughput, latency p50/p99/p99.9/max, failed operations, throttled responses |
| findings | every finding: code, cause, evidence, confidence, remedy |
| where the time goes | each operation's wall time split into stages (below) |
| operations | per workload verb: count, ops/s, p50, p99, max, requests per operation, errors |
| backend requests | per storage API (`GetObject`, `PutObject`, …): count, share on warm connections, mean TTFB and transfer, p99, status codes |
| connections | HTTP version, TLS, new connections, reuse, peak in-flight requests, dial time per phase |
| bytes | logical vs backend bytes, read amplification, over-read, requests per operation, cache hits |
| slowest operation | the worst operation request by request (a waterfall), with its async call stack |
| self | the profiler's own health: scheduling lag, CPU, whether the client was saturated |
| cleanup | test data removed (verified), or what is left |

`--by stage` sums the requests' time per stage, `--by api` per storage API, `--by conn` per
connection (cold and warm, per remote address).

`--since 3s --until 8s` shows one time range of the window (a burst, the seconds a rate limit
bit): its numbers, where the time went, the verbs, the requests by API and its slowest
operations, recomputed from the trace rows. When the rows were sampled it says so, and each row
counts for the operations it stands for. In the app, drag across the timeline or the heatmap.

### Where the time goes

Each operation's wall time is split exactly; an instant counts once even when requests overlap:

| Stage | Time when … | If it dominates |
|---|---|---|
| queue | requests wait for a connection or an in-flight permit | a concurrency limit or a small pool; see `cold_connections` and `throttling` |
| connect | DNS, TCP, proxy and TLS of a new connection | connections are not reused, or the proxy or TLS is slow (`http.keep_alive`, `http.version`) |
| send | the request body goes out | upload bandwidth; `write.chunk`, `write.concurrent` |
| TTFB | waiting for the first byte (the server's work plus one round trip) | the store or the distance to it; stragglers show as a long tail (`layers.tail_cut`) |
| body | the response body arrives | download bandwidth per stream × streams; `read.chunk`, `read.concurrent` |
| stall | the body waits for your code to read it | the consumer, not storage |
| retry | the retry layer sleeps before the next attempt | throttling or errors; see the findings |
| client | everything else: OpenDAL, signing, checksums | CPU (`client_bound`), tiny objects |

### How it is measured

- **Requests** are observed inside OpenDAL's own HTTP client (reqwest with rustls, exactly as
  OpenDAL builds it): a timed resolver, a connector layer that times each dial, the TLS
  ClientHello timestamp (which splits TCP from TLS), the response's connection identity (cold or
  warm), and the body timed as it arrives and as your code consumes it. Waiting inside reqwest's
  pool for an idle connection is part of TTFB. Kernel TCP statistics (RTT, retransmits) are read
  at the end of a request on Linux and macOS.
- **Operations** are the workload's logical calls (`read`, `write`, `stat`, `list`, a parquet
  scan); every request is attributed to the operation that caused it, including requests in
  OpenDAL's background tasks.
- **Open loop** (`--rate`): latency counts from the intended start, and arrivals that never
  started count in every percentile as censored values, so a slow server cannot hide behind fewer
  requests.
- **Percentiles** are exact when every operation was kept, otherwise from histograms with three
  significant digits. Counts always agree with the totals. The report keeps the run's whole
  latency distribution (`latency_histogram`, 20 bins per decade), and the latency CDF is drawn
  from it whenever the trace rows are sampled.
- **The timeline** has one row per interval (1 s for the first ten minutes, then 10 s).
  Operations, goodput (`goodput_ops_per_s`: successes only; `ops_per_s` counts every attempt)
  and the latency quantiles belong to the interval an operation *started* in; requests
  (`requests_count`, retries included), throttled responses (`throttled_count`), retries and
  failures (`errors_count`) to the interval they *happened* in, so a 503 at +7 s is drawn at
  +7 s. `ops_count` says how many operations stand behind an interval's quantiles: a p99 needs
  about 1 000. Each interval also carries the same counters per verb (`verbs`).
- **The profiler's cost** is measured: `cargo bench -p oprobe-trace --bench overhead` compares
  the same operations with and without the layer (budget 2 µs per operation), and the profile's
  `self` section reports scheduling lag and CPU during the run. A run where the client was
  saturated says so, and its verdict is `client_bound` rather than a server limit.

## JSON

`-o json` prints one document on stdout. `run` and `show` print the report
(`"schema": "oprobe.report/6"`), `prove` the prove result, `show --list` the run list. Numeric
fields carry their unit in the name (`_ms`, `_bytes`, `_ratio`, `_count`), enums are
`snake_case`, and fields are only ever added within 6.x. Errors are
`{"type":"error","error":{"code","message","hint","retryable","docs"}}`.

## Files

A stored run lives in `<data dir>/oprobe/runs/` as one `.oprobe` file (format `oprobe.run/6`: a
zstd stream of the header with the operator spec and workload, columnar segments of operation and
request rows, the per-second timeline, and the report). `oprobe-trace` writes `oprobe.trace/6`
files (one JSON document: the aggregates and the sampled rows). `oprobe show FILE` and the app
open both. 5.x run files are not read.
