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
[Attention] 2.4% of GetObject requests wait far longer for the server's first byte than their peers
  (stragglers).
  lab://demo · small-files · 8.0 s · run 20261123-063611-3d82
  ▲ 1 tail_stragglers (high confidence) — 2.4% of GetObject requests wait far longer for the
      server's first byte than their peers (stragglers).
      162 stragglers wait ~261ms for the first byte against ~9.78ms for a typical GetObject;
      operation p50 10.0ms · p99 261ms
      remedy: layers.tail_cut + layers.retry.min_delay — re-issue requests far slower than their
        peers, after a short retry backoff (TailCutLayer + RetryLayer::with_min_delay)
      next: oprobe prove 20261123-063611-3d82 tail_stragglers
  ▲ 2 throttling (medium confidence) — 15% of operation time is spent in retry backoff after
      429/503: the service throttled 0.1% of requests.
      16 throttled responses (429/503) in 11416 requests (0.14%); 15% of operation time in retry
      backoff; the rejections start above a number of requests in flight (the in-flight limit
      explains 81% of the throttled requests)
      remedy: layers.concurrent_limit.http + layers.retry.jitter — cap the HTTP requests in flight
        below the service's limit and retry with jitter
        (ConcurrentLimitLayer::with_http_concurrent_limit + RetryLayer::with_jitter)
      next: oprobe prove 20261123-063611-3d82 throttling
  1425.0 ops/s · 80.1 MiB/s · p50 10.0ms · p99 261ms · errors 0.00% · 0.14% of requests throttled
  full profile: oprobe show 20261123-063611-3d82 --detail · terms: https://github.com/hxddh/oprobe/blob/v6.3.0/docs/concepts.md#glossary
```

(`oprobe demo`; your numbers differ from run to run.)

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
| key numbers | goodput and throughput, latency p50/p99/p99.9/max, failed operations, throttled responses, and one **resilience** line: retries, time spent waiting between attempts, 429/503, the 5xx/4xx/network split and the top failure classes (the app's Retries section) |
| findings | every finding: code, cause, evidence, confidence, remedy |
| where the time goes | each operation's wall time split into stages (below) |
| operations | per workload verb: count, ops/s, p50, p99, max, requests per operation, errors (`--detail`: MiB/s, retries, throttled share) |
| backend requests | per storage API (`GetObject`, `PutObject`, …): count, share on warm connections, TTFB and total p50/p99, bytes down/up, status codes (`--detail`: every stage's p50/p99); and, for a run that wrote, a **writes** line: the multipart part size OpenDAL used (and the requested `write.chunk` and the service's part-size limits when it was clamped) and the parts uploaded with their mean size — the knobs `prove` sizes for `slow_multipart` |
| connections | HTTP version, TLS, new connections, reuse, peak in-flight requests, dial time per phase (proxy CONNECT included), and a **tcp** line from the kernel: rtt p50/p99, connections that retransmitted, the share of sending time limited by the peer's receive window or the local send buffer, the delivery rate. High rtt or retransmits point at the network; a clean network with a high TTFB points at the server. `--detail` adds the TLS handshakes (resumed share, HelloRetryRequests) and the cold start (dials and first operations before the warm-up absorbed them) |
| bytes | logical vs backend bytes, read amplification, over-read, requests per operation, cache hits |
| slowest operation | the worst operation request by request (a waterfall), with what each request recorded beyond its stages (below) and, with `--detail`, its async call stack |
| self | the profiler's own health: scheduling lag, CPU, whether the client was saturated |
| cleanup | test data removed (verified), or what is left |

`--detail` also adds, per interval of the timeline, retries, new connections and the client's
CPU (percent of one core), and the cold and warm reads (first read of a key, repeat reads) to
SPREAD.

`--by stage` sums the requests' time per stage, `--by api` per storage API, `--by conn` per
connection (cold and warm, per remote address). `--by stage` also gives each stage's p50/p99
per API and, when the requests recorded them, the request signals below.

### Request signals

Besides its stage timings a request row can carry what the response and the connection said.
The waterfall of the slowest operation (terminal and app) lists them per request, and
`show --by stage` summarises them per API:

| Signal | What it says |
|---|---|
| error code, Retry-After | the provider's own error (`SlowDown`, `ServerBusy`) and how long it asked to wait |
| h2 stream wait | HTTP/2: the request waited for a stream slot (`max_concurrent_streams`); part of queue |
| h2 flow stall | HTTP/2: the request body waited for flow-control window; it is hidden inside TTFB |
| h2 streams | HTTP/2 streams open on the connection when the request started |
| upload acked, send buffered | when the kernel saw the last request byte acknowledged, and when the body reached the send buffer: the end of the upload |
| cache | a CDN's cache status (`hit`, `miss`) |
| region | the bucket's region (`x-amz-bucket-region`) |
| rtt, retransmits, Server-Timing | the connection's round trip and retransmitted segments, the server's own timing header |

The compact profile shows the error code, Retry-After, the HTTP/2 waits and the cache status;
`--detail` shows every signal.

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
| stall | the body waits for your code to read it (client time: its own segment in a waterfall, never part of body) | the consumer, not storage |
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
