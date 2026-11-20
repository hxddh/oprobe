# Reading the report

The same report comes in four shapes:

| | Command | For |
|---|---|---|
| Brief | `oprobe run …` (default output) | the answer in ≤ 20 lines, 80 columns |
| Full text | `oprobe runs show [RUNREF] [--detail]` | the terminal, logs |
| HTML | `oprobe runs show RUNREF -o html > report.html` (also `oprobe run … -o html`) | charts, every request, sharing |
| Markdown | `-o md` | PR comments, wikis, job summaries |
| JSON | `-o json` | scripts and agents (`oprobe schema report`) |

## The brief

```text
[CRITICAL] Throttling-bound: the service rejected 3.6% of requests with 429/503;
  ✖ 626 throttled responses (429/503)                   ← cause
    3.57% of requests throttled; 609 retries, 63.02s …  ← evidence
    → Lower `concurrency` / `max_inflight_requests` …   ← advice
    next: oprobe tune prod                              ← a command that checks it (when there is one)
  2114.0 ops/s · 119.2 MiB/s · p50 5.04ms · p99 257ms · errors 0.10%
  626 throttled · $12.40 /month · SLA met
full report: oprobe runs show 20261120-084637-d873
```

The badge is the verdict level: **HEALTHY** (ok), **REVIEW** (worth a look: informational findings, warnings,
a partial run), **ATTENTION** (warning findings), **CRITICAL** (critical findings or a breached SLA). At most
three findings are shown, most severe first; `oprobe explain RUNREF` narrates the top three with their
evidence and documentation (`--top N` for more; the report lists every finding).

## The full text report

| Section | What it tells you |
|---|---|
| header | oprobe version, run id and label, target, preset, window, workers, client, start time |
| verdict | the badge and one sentence of where the time went |
| KEY NUMBERS | throughput (ops/s, MiB/s read/write), latency p50/p99/p99.9/max, failed operations, throttled responses, the SLA, projected cost |
| FINDINGS | every finding: cause `[code]`, evidence, → advice, `try:` command |
| WHERE THE TIME GOES | the share of total operation wall time per layer (below) |
| OPERATIONS | per workload verb: count, ops/s, p50, p99, max, requests per operation, errors |
| BACKEND REQUESTS | per storage API (`GetObject`, `PutObject`, …): count, share on warm connections, mean TTFB and transfer, p99, status codes |
| CONNECTIONS | protocol, TLS, new connections, reuse, peak in-flight requests, dial p50/p99 per phase |
| BYTES & AMPLIFICATION | logical vs backend bytes, read amplification, over-read, requests per operation, cache, integrity |
| COST | the selected price table, per 1M operations and per month, waste, the same workload elsewhere |
| SLOWEST OPERATION | the worst operation request by request, with its async call stack |
| CLEANUP | test data removed (verified), or what is left and how to remove it |

`--detail` adds every column of the tables, the p99 of each request stage, the mean breakdown per verb, every
remote address, the per-second timeline, the configuration (knobs, workload, options, host, trace sampling) and
the five slowest operations.

### Where the time goes

Each operation's wall time is split exactly (an instant counts once even when requests overlap):

| Layer | Time when … | If it dominates |
|---|---|---|
| queue | requests wait for an in-flight permit or a pooled connection | raise `--max-inflight`, or fewer workers per connection pool |
| cache | the local read cache is looked up | — |
| connect | DNS, TCP, proxy CONNECT and TLS of new connections | connections are not reused, or the proxy/TLS is slow (`oprobe probe`) |
| upload | request bodies are sent | bandwidth or a proxy; bigger `--chunk-size`, more `--io-concurrency` |
| server | waiting for the first byte (server work + one round trip) | the store or the distance to it; more parallelism helps until throttling |
| download | response bodies arrive | bandwidth per stream × streams |
| retry | the retry layer sleeps before the next attempt | throttling or errors: see the findings |
| client | everything else: OpenDAL, signing, CRC, your code | CPU (`client cpu`), tiny objects, verification |

## The HTML report

One file, no network: it can be attached to a ticket, opened offline, printed or saved as PDF.

- **Verdict** — the badge, the headline, tiles for throughput, data rate, p50, p99, failed operations,
  throttled responses and projected cost; SLA, partial-run and sampling notes; warnings.
- **Findings** — cards with severity, code, cause, evidence values, the operations and time windows that show
  it (operation links filter the request table), advice and the verification command.
- **Where the time goes** — the layer shares as one 100% bar (hover for values) and a table; request-stage p99
  and the mean breakdown per operation.
- **Timeline** — operations per second, p50 and p99 latency (log axis) and errors/throttled operations per
  second, one stepped sample per second; hover a column for every value.
- **Latency heatmap** — operations by start time and latency bucket (log), darker = more operations.
  Horizontal bands above the bulk are stragglers; vertical stripes are stalls.

Time axes count seconds from the start of the measurement window, and every bucket is stamped with its
**start**: `timeline[].t_s = 3` is the interval 3–4 s, like `heatmap.t_buckets_s`, `oprobe analyze --by
second` and the `interval` events.
- **Operations** — the full verb and API tables, bytes and amplification.
- **Slowest operations** — up to five waterfalls: each request of the operation on a shared millisecond
  axis, coloured by stage (queue, connect, send, time to first byte, body; grey for retry waits and other
  time); the async call stack below.
- **Requests** — every request row (above 5 000 rows: the slowest operations' requests, failed and slowest
  requests, and an even sample, as the page says). Filter with words or `field:text`, `field=value`,
  `total_ms>100`; pick failed / throttled / cold / retried requests; sort by any column.
- **Network**, **Cost**, **Environment** — connections, TLS/TCP per phase, remote addresses, retries and
  error classes; the cost breakdown and comparison; run, host, target options, workload, requested vs
  effective knobs, cleanup.

The ◐ button switches light/dark (the default follows the system).

## Common verdicts

* **Server latency (TTFB)-bound** — the store, or the round trip to it, dominates. More parallelism helps until
  throttling; compare the TCP connect time to see how much of TTFB is network distance.
* **Throttling-bound** — 429/503 and retry backoff. Lower concurrency or run `oprobe tune` to find the ceiling;
  spread keys over more prefixes.
* **Download/upload-bound** — bandwidth per stream × number of streams; raise `io_concurrency` or
  `chunk_size`, check the NIC and any proxy.
* **Connection setup** — connections are not reused (keep-alive off, bodies not drained, tiny idle pool) or the
  proxy/TLS handshake is slow.
* **Client queue** — requests wait for `max_inflight_requests` permits.
* **Client processing** — CPU, OpenDAL overhead, signing, CRC or your consumer.

Every number is defined in [metrics.md](metrics.md).
