# oprobe for agents

oprobe is built to be driven by programs and AI agents as well as people. Everything a person
sees has a machine form with a published JSON Schema, every failure has a stable error code and
exit code, every run is stored and addressable, and the whole tool is available as an MCP server.

This page is for agents (and their authors) **using** oprobe. Agents working **on** the oprobe
repository should read [`AGENTS.md`](../AGENTS.md).

## Contracts

| What | Where | Schema |
|---|---|---|
| Command results | `-o json` (one document on stdout) | `oprobe schema <command>` |
| Live events | `-o ndjson` (one event per line, ends with `result` or `error`) | `oprobe schema events` |
| Errors | `{"type":"error","error":{code,message,hint?,retryable,docs?}}` on stdout in json/ndjson modes | `oprobe schema error` |
| Report v3 | `report` of `run`, `runs show`, MCP `get_run` | `oprobe schema report` |
| Run files | `runs/<id>.oprobe` (zstd NDJSON: header, report, op rows, req rows) | `oprobe schema run-file` |
| Config | `config.toml` (`[defaults]`, `[targets.NAME]`) | `oprobe schema config` |
| Self-description | `oprobe capabilities -o json` | `oprobe schema capabilities` |

Schemas are JSON Schema 2020-12 with `$id`
`https://github.com/hxddh/oprobe/schemas/4/<name>.json`; the checked-in copies live in
[`schemas/`](../schemas). `oprobe schema` lists every name.

Rules that hold everywhere:

- stdout carries only the result; logs and progress go to stderr (or become `log` events).
- Numeric field names carry their unit (`_us`, `_ms`, `_s`, `_bytes`, `_mib`, `_gib`, `_bps`,
  `_per_s`, `_pct`, `_ratio`, `_usd`, `_count`; `_mib`/`_gib` only where the value is naturally
  fractional, e.g. `client.rss_mib`, `cost.stored_gib`); identifiers (`*_id`, `op_ids`, `slowest`),
  `exit_code`, `seed`, mix `weight` and `heatmap.counts` are unitless. Finding evidence values follow
  the same rule (`current_p99_ms`, `predicted_p99_ms`, …). Enums are lowercase `snake_case`; there are
  no human-formatted numbers in machine fields. A test walks the report schema to enforce this.
- Time buckets (`timeline[].t_s`, `heatmap.t_buckets_s`, `interval` events) are stamped with the start
  of the bucket, in seconds from the measurement window start.
- `_mean` marks fractional means of counts (`capacity.observed.inflight_mean`); values whose unit
  is a sibling `unit` field (`predicted`, `lo`, `hi`, `baseline`, `candidate`) are unitless.
- Every JSON command result is an envelope: its own fields plus `next` (suggested steps, each
  `{ title, cli, mcp: { tool, arguments } }`) and `warnings` (including what the command printed as
  warnings on stderr).
- Every report carries `spec`, the run's replayable definition: `oprobe run --from RUNREF` repeats a
  run exactly (credentials are resolved again), `--set key=value` changes one setting. A finding's
  `verification` names its `overrides` and gives `command` (`oprobe verify RUN CODE`), `rerun`
  (`oprobe run --from RUN --set …`) and `replay` (a self-contained `oprobe run TARGET …`).
- Bad input is never `internal`: it is `usage.*` or `config.*` (exit 2) with a hint, and the CLI and
  the MCP server give the same code for the same input.
- Finding codes are `snake_case`; `oprobe explain` also accepts `-` spellings.
- Consumers must ignore unknown fields and unknown event types.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | runtime failure (auth, network, storage, internal) |
| 2 | usage or configuration error |
| 3 | SLA failed or regression (`compare`) |
| 4 | a budget stopped the run (the partial run is saved) |
| 5 | a safety rail refused the command |
| 130 | interrupted |

`oprobe capabilities -o json` lists every error code with its exit code and whether retrying can
help (`retryable`).

### Run references

Commands and tools that take a run accept a RUNREF: the run id (`20261120-143005-a1b2`), a unique
id prefix, `last`, `last~N`, a label, or a path to a `.oprobe` file.

## Safe workflow

1. **Know the tool**: `oprobe capabilities -o json` (services, presets, providers, error and
   finding codes, MCP tools, defaults).
2. **Preflight**: `oprobe doctor TARGET -o json` — `ready` must be true. Add `--read-only` to
   skip the write/read/delete probe.
3. **Plan**: `oprobe run TARGET … --dry-run -o json` — requests, bytes, objects written and
   deleted, cost and ETA. Nothing is sent to the target.
4. **Run with budgets**: `oprobe run TARGET … --max-requests N --max-duration 2m
   --max-cost-usd 0.10 -o ndjson`. A breached budget stops the run (exit 4, `safety.budget_exceeded`)
   and still saves the partial run. `--read-only` refuses anything that writes (exit 5);
   `--existing PREFIX` benchmarks existing objects and never writes.
5. **Explain**: `oprobe explain last -o json` — verdict, findings with evidence and documentation,
   outlier attribution of the slowest 1 % of requests, suggested next commands.
6. **Change one knob, run again, compare**: `oprobe compare last~1 last -o json` (exit 3 on a
   regression).

Writes are always confined to `<prefix>/oprobe-run-<id>/` and removed when the run ends;
`oprobe cleanup TARGET --dry-run` finds leftovers of killed runs. `memory://` needs no
credentials and never leaves the process — use it to learn the outputs.

## MCP server

`oprobe mcp` serves the Model Context Protocol on stdio; `oprobe mcp --http 127.0.0.1:8765`
serves Streamable HTTP at `http://127.0.0.1:8765/mcp`.

| Flag | Effect |
|---|---|
| (default) | read-only: runs that would upload a dataset or write are refused with `safety.read_only` (`memory://` and `existing` runs are allowed); `doctor` skips its write probe; `cleanup` is a dry run |
| `--allow-write` | enable runs that write test data, `start_tune`, `cleanup` with `confirm` |
| `--max-cost-usd`, `--max-requests`, `--max-bytes`, `--max-duration` | budgets applied to every run the server starts (a call can only lower them) |
| `--allow-target NAME\|URL-PREFIX` | only these targets (repeatable; prefixes match at `/` boundaries). A call may set `region` and `anonymous`, but `endpoint`, `options`, `proxy`, `insecure` or URL query parameters that change the scheme, host, bucket, root, TLS trust or proxy of the allowed target are refused with `safety.refused` |
| `--http-allow-remote` | allow `--http` on a non-loopback address (refused otherwise: the HTTP transport has no authentication) |
| `--max-concurrent-runs N` | runs executing at once (others queue; default 1) |

### Tools

`tools/list` order is fixed. Every tool has an `inputSchema` and an `outputSchema`, returns
`structuredContent` plus a one-paragraph text summary, and reports failures as tool errors
(`isError: true`) whose second text block is the JSON error document.

| Tool | Purpose |
|---|---|
| `doctor` | preflight a target |
| `probe_endpoint` | DNS/TCP/TLS handshakes and cold/warm TTFB of an endpoint |
| `plan_run` | dry-run plan, effective budgets, `allowed` + `refusals` |
| `start_run` | start a run in the background; returns the run id at once (`wait: true` blocks); progress notifications when the call has a `progressToken` |
| `get_run` | status, progress and (when done) summary and report; also any stored RUNREF |
| `cancel_run` | stop a run (cleanup runs, the partial run is saved) |
| `list_runs` | stored runs and runs this server started |
| `explain_run` | narrated explanation (same as `oprobe explain RUNREF`) |
| `analyze_run` | outlier attribution for a `Selection` (time window, latency floor, API) or the slowest N % |
| `compare_runs` | regression gate between two runs |
| `start_tune` | throttle-aware tuning in the background |
| `estimate_cost` | request cost at list prices vs the cost budget |
| `cleanup` | leftover test data (dry run unless `confirm` on a write-enabled server) |
| `list_targets` | configured targets, what is allowed, server budgets |

Resources: `oprobe://runs/{id}/report`, `oprobe://runs/{id}/events` (NDJSON), `oprobe://runs/{id}/slowest`,
`oprobe://runs/{id}/html` (the single-file HTML report), `oprobe://schema/{name}`,
`oprobe://docs/{getting-started|safety|findings|finding/CODE}`, `oprobe://presets`.

Prompts: `diagnose-slow-reads`, `tune-for-p99`, `ci-gate-setup`.

### Setup

Claude Code:

```sh
claude mcp add oprobe -- oprobe mcp --max-requests 50000 --max-duration 2m
# allow writes to a scratch bucket only:
claude mcp add oprobe-rw -- oprobe mcp --allow-write --allow-target s3://scratch-bucket/oprobe/ --max-cost-usd 0.50
```

Claude Desktop (`claude_desktop_config.json`), Cursor (`.cursor/mcp.json`) and other clients that
use the `mcpServers` format:

```json
{
  "mcpServers": {
    "oprobe": {
      "command": "oprobe",
      "args": ["mcp", "--max-requests", "50000", "--max-duration", "2m"],
      "env": { "AWS_PROFILE": "bench" }
    }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "oprobe": {
      "type": "stdio",
      "command": "oprobe",
      "args": ["mcp", "--max-requests", "50000"]
    }
  }
}
```

Streamable HTTP (any client): start `oprobe mcp --http 127.0.0.1:8765` and point the client at
`http://127.0.0.1:8765/mcp`. Only loopback `Host` headers are accepted unless you bind another
address. The HTTP transport has no authentication, so `--http` refuses non-loopback addresses
(`0.0.0.0:8765`, a LAN IP) unless you also pass `--http-allow-remote`, which prints a warning;
prefer a loopback bind behind an authenticating tunnel or proxy.

Credentials come from the server's environment (the usual provider chains, `AWS_PROFILE`, …) or
from configured targets (`oprobe target add`); never pass secrets as tool arguments.

## Explaining findings

`oprobe explain CODE` documents a finding code; `oprobe explain --list` lists them. The catalog
below is generated from the same source (`src/mcp/catalog.rs`).

<!-- BEGIN finding-codes (generated: OPROBE_BLESS=1 cargo test --test agent) -->

<a id="finding-integrity"></a>
### `integrity` — Data read back differs from data written

CRC32C verification of objects oprobe wrote and read back failed: the storage path returned different bytes. *(correctness, usually critical)*

- **Causes:** a proxy or gateway that rewrites or truncates bodies; multipart parts assembled in the wrong order by an S3-compatible service; a storage service bug or degraded node
- **Verify:** report.integrity (corrupted objects > 0); repeat with `oprobe run TARGET --preset checkpoint` and compare integrity counts; run `oprobe compat TARGET` for multipart and checksum edge cases
- **Knobs:** --no-verify (disables the check; never to hide this); --chunk-size; --proxy

<a id="finding-errors"></a>
### `errors` — Operations failed

Some logical operations failed after retries; critical when the failure rate exceeds the SLA's max error rate. *(availability, usually warning)*

- **Causes:** missing permissions (403) or wrong credentials; objects or buckets that do not exist (404); timeouts or connection resets on an overloaded path; throttling that outlasts the retry budget
- **Verify:** report.resilience.error_classes and report.summary.error_ratio; `oprobe doctor TARGET` for credentials, permissions and the endpoint; `oprobe analyze RUNREF --where error` / MCP analyze_run to see which requests failed
- **Knobs:** --retries; --max-error-rate; --concurrency

<a id="finding-throttling"></a>
### `throttling` — The service rate-limited requests (429/503)

The storage service answered with 429 or 503 SlowDown: the request rate is above what the bucket, prefix or account allows right now. The evidence names the request rate the service accepted while throttling (the ceiling) and a concurrency to try. *(availability, usually critical)*

- **Causes:** too many concurrent requests against one prefix (S3 partitions scale per prefix); a request-rate quota on the account or bucket; a burst right after the bucket was created (not yet scaled out)
- **Verify:** report.resilience.throttled_responses and the retry share in report.stages.shares.retry_pct; the finding's verification command re-runs at the suggested concurrency; `oprobe tune TARGET` finds the throttling ceiling
- **Knobs:** --concurrency; --max-inflight; --retries; --max-throttle-rate

<a id="finding-retries"></a>
### `retries` — Operations spend their time waiting to retry

Requests fail with temporary errors (not throttling) and operations sleep between retry attempts: at least 5% of operation time is retry backoff. *(availability, usually warning)*

- **Causes:** timeouts or connection resets; 5xx errors from an unhealthy service; a too aggressive timeout
- **Verify:** report.stages.shares.retry_pct and report.resilience; `oprobe analyze RUNREF --where attempt=retry --by status`
- **Knobs:** --retries; --tail-cut

<a id="finding-tail_stragglers"></a>
### `tail_stragglers` — A few requests wait far longer for the first byte

The slowest requests spend most of their time waiting for the server's first byte while the rest answer quickly: server-side stragglers. The evidence includes a replayed estimate of what a tail-cut (re-issuing slow requests) would do to p99. *(bottleneck, usually warning)*

- **Causes:** server stragglers (slow disks, garbage collection, hot partitions); one slow front-end behind the endpoint; background load on a shared service
- **Verify:** the finding's evidence values (current and predicted p99 in ms); the verification command re-runs with `--tail-cut`; compare p99 with `oprobe compare`; `oprobe analyze RUNREF --slowest 2 --by remote`
- **Knobs:** --tail-cut; --retries; --concurrency

<a id="finding-tail_latency"></a>
### `tail_latency` — p99 latency is far above the median

Operation p99 is more than 5× the median; the finding names the request stage the slowest requests spend most of their time in. *(bottleneck, usually warning)*

- **Causes:** cold connections or retries on a few requests; throttling bursts; a slow remote IP behind the endpoint; large transfers on a few operations
- **Verify:** report.slowest op ids and their request rows; `oprobe analyze RUNREF --slowest 2 --by stage`
- **Knobs:** --tail-cut; --retries; --concurrency

<a id="finding-client_queue"></a>
### `client_queue` — Operations wait for in-flight request permits

Operations spend a large share of their time (at least 10%) queued in the client before a request is even sent. *(bottleneck, usually warning)*

- **Causes:** max_inflight_requests caps requests below concurrency × io_concurrency; a connection pool that is too small
- **Verify:** report.stages.shares.queue_pct; report.stages.request_p99.queue_ms; re-run with `--max-inflight 0` and compare
- **Knobs:** --max-inflight; --concurrency; --io-concurrency

<a id="finding-cold_connections"></a>
### `cold_connections` — Connection setup costs a large share of request time

Requests that open a new connection pay DNS, TCP, proxy and TLS handshakes; together they take at least 10% of request time, or connections are rarely reused. The evidence names the slowest handshake step. *(network, usually warning)*

- **Causes:** keep-alive disabled; response bodies not fully drained, so connections are not returned to the pool; an idle pool too small for the concurrency; a proxy that closes connections; slow TLS handshakes (long round trip, TLS-inspecting proxy, no resumption)
- **Verify:** report.connections (reuse_pct, dialed connections) and report.network.tcp / tls; `oprobe analyze RUNREF --by conn`; `oprobe probe ENDPOINT` for handshake times
- **Knobs:** --no-keep-alive (remove it); --http h2; --microscope

<a id="finding-http1_connections"></a>
### `http1_connections` — HTTP/1.1 needs one connection per in-flight request

Nearly all requests use HTTP/1.1, so every in-flight request needs its own connection; HTTP/2 would multiplex them over a few connections. *(network, usually info)*

- **Causes:** the HTTP version left on auto against a service that negotiates HTTP/1.1; a proxy that only speaks HTTP/1.1
- **Verify:** report.connections (distinct connections vs requests); re-run with `--http h2` and `oprobe compare` (some services are faster on HTTP/1.1)
- **Knobs:** --http h1|h2; --max-inflight

<a id="finding-remote_skew"></a>
### `remote_skew` — One remote address is much slower than the others

Requests to one front-end IP take at least 3× longer (server + transfer time) than requests to the other remotes behind the same endpoint. *(network, usually warning)*

- **Causes:** a degraded or overloaded front-end server; a remote in another zone or region; long DNS caching pinning connections to one address
- **Verify:** report.network.remotes; `oprobe analyze RUNREF --by remote` and `--where remote=IP`
- **Knobs:** --concurrency; --tail-cut

<a id="finding-tcp_retransmits"></a>
### `tcp_retransmits` — Packets were lost while requests were in flight

The kernel's TCP statistics show retransmissions on a connection between consecutive samples, i.e. while a request was running on it. Loopback connections are not counted. *(network, usually warning)*

- **Causes:** a congested NIC, proxy or VPN; a lossy cross-region or internet link; a receive window or buffer too small for the bandwidth
- **Verify:** the finding's evidence: requests with retransmissions and the retransmitted segments; `oprobe analyze RUNREF --by conn`; compare from another host or region
- **Knobs:** --region; --proxy; --concurrency

<a id="finding-high_rtt"></a>
### `high_rtt` — The network round trip is long

The kernel-measured TCP round-trip time to the endpoint is at least 50 ms at the median; every request pays at least one round trip. *(network, usually info)*

- **Causes:** the client runs far from the bucket's region; a proxy or VPN detour
- **Verify:** report.network.tcp (round-trip time); `oprobe probe ENDPOINT`
- **Knobs:** --region; --concurrency; --proxy

<a id="finding-consumer_stall"></a>
### `consumer_stall` — Response bodies wait for the consumer

Data arrives faster than the application side consumes it; storage is not the limit. *(client, usually warning)*

- **Causes:** slow decoding or copying of downloaded data; a CPU-bound consumer
- **Verify:** report.stages.request_p99.stall_ms larger than body_ms; `oprobe analyze RUNREF --slowest 2 --by stage` (stage=stall)
- **Knobs:** --io-concurrency; --chunk-size

<a id="finding-overread"></a>
### `overread` — Downloaded bytes were never requested

Range coalescing or chunking downloaded noticeably more bytes than the application asked for, costing egress and bandwidth. *(efficiency, usually warning)*

- **Causes:** a large range_gap merging distant ranges; a chunk size larger than the reads
- **Verify:** report.bytes (overread ratio, backend bytes down)
- **Knobs:** --range-gap; --chunk-size

<a id="finding-small_objects"></a>
### `small_objects` — Small requests: round trips, not bandwidth, bound throughput

The mean request moves less than 128 KiB and most request time is spent waiting for the server's answer; throughput grows with parallelism, not bandwidth. *(bottleneck, usually info)*

- **Causes:** many small objects or small range reads; too little parallelism for the per-request latency
- **Verify:** report.apis (bytes per request) and report.stages.shares; the verification command doubles the concurrency; compare throughput
- **Knobs:** --concurrency; --range-gap; --object-size

<a id="finding-suffix_read"></a>
### `suffix_read` — Use suffix reads: saves one HEAD per file

File-scan operations issue a HEAD to learn the object size before reading the footer; a suffix range read returns the footer and the size in one request. *(efficiency, usually info)*

- **Causes:** readers that stat a file before reading its tail (Parquet/ORC footers); a reader without suffix-range support enabled
- **Verify:** report.apis: HeadObject requests close to the number of file scans; `oprobe run TARGET --preset parquet-suffix` and `oprobe compare` against the parquet preset
- **Knobs:** --preset parquet-suffix

<a id="finding-cache_opportunity"></a>
### `cache_opportunity` — The same objects are read repeatedly

Replaying the run through a 256 MiB read cache would save at least 200 requests per 1 000 operations: the working set is re-read. *(efficiency, usually info)*

- **Causes:** a skewed (zipf, hot-set) access pattern; repeated reads of the same footers or metadata
- **Verify:** the finding's predicted request savings; the verification command re-runs with `--cache 256MiB`
- **Knobs:** --cache SIZE; --access

<a id="finding-cost_hotspot"></a>
### `cost_hotspot` — One API or wasted requests dominate the bill

One API is at least 60% of the projected request spend, or at least 10% of the projected monthly bill buys nothing (failed or throttled requests, over-read egress). *(cost, usually info)*

- **Causes:** many small requests of one kind (HEAD, LIST, small GETs); throttled or failed requests that are billed anyway; over-read downloads
- **Verify:** report.cost.by_api and report.cost.waste_usd; `oprobe run TARGET --provider NAME --monthly-ops N` for your own volume
- **Knobs:** --range-gap; --chunk-size; --concurrency; --provider

<a id="finding-slow_start"></a>
### `slow_start` — The first seconds are much slower than the steady state

The start segment of the run has at least twice the median latency of the steady segment; short runs are skewed by it. *(sla, usually info)*

- **Causes:** cold connections, DNS and TLS at the start; server-side scaling or caches warming up
- **Verify:** `oprobe analyze RUNREF --by second`; the verification command re-runs with a longer `--warmup`
- **Knobs:** --warmup; --duration

<a id="finding-server_latency"></a>
### `server_latency` — Latency-bound on server time-to-first-byte

Requests spend most of their time waiting for the server's first byte; throughput scales with parallelism. *(bottleneck, usually info)*

- **Causes:** network round trips to a distant region; per-request service latency of the storage (normal for object stores); many small range reads that could be coalesced
- **Verify:** report.stages.request_p99.ttfb_ms dominates; `oprobe probe ENDPOINT` cold vs warm TTFB; re-run with higher `--concurrency` and compare throughput
- **Knobs:** --concurrency; --io-concurrency; --range-gap; --region

<a id="finding-download_bound"></a>
### `download_bound` — Bandwidth-bound on downloads

Transferring response bodies dominates; the per-stream or aggregate bandwidth is the limit. *(bottleneck, usually info)*

- **Causes:** NIC, VM or proxy bandwidth caps; cross-region or internet links; too few parallel streams for large objects
- **Verify:** report.stages.shares.download_pct is the largest share; report.summary (MiB/s) against the machine's network limit
- **Knobs:** --io-concurrency; --concurrency; --chunk-size

<a id="finding-upload_bound"></a>
### `upload_bound` — Bandwidth-bound on uploads

Sending request bodies dominates; upload bandwidth is the limit. *(bottleneck, usually info)*

- **Causes:** NIC, VM or proxy bandwidth caps; single-stream uploads of large objects
- **Verify:** report.stages.shares.upload_pct is the largest share
- **Knobs:** --io-concurrency; --chunk-size; --concurrency

<a id="finding-client_cpu"></a>
### `client_cpu` — Load generator CPU is saturated

The machine running oprobe is near its CPU limit, so measurements understate what the storage can do. *(client, usually warning)*

- **Causes:** CRC32C verification of large objects; TLS on many connections from a small machine; very small operations at high concurrency
- **Verify:** report.client (CPU use vs cores); re-run with `--no-verify` or fewer workers and compare throughput
- **Knobs:** --no-verify; --concurrency; --object-size

<a id="finding-client_overhead"></a>
### `client_overhead` — Time is spent outside network requests

Most operation time is client-side processing (signing, buffering, OpenDAL, CRC), not network requests. Reported only when nothing more specific explains the run. *(client, usually info)*

- **Causes:** in-memory or local backends (expected); tiny objects where per-operation overhead dominates
- **Verify:** report.stages.shares.client_pct is the largest share
- **Knobs:** --object-size; --no-verify

<a id="finding-local_backend"></a>
### `local_backend` — Local backend service time dominates

An in-process backend (memory, fs) is measured: results reflect local disk or memory, not network storage. *(bottleneck, usually info)*

- **Causes:** benchmarking memory:// or fs:// (expected)
- **Verify:** report.target.scheme is memory or fs

<a id="finding-cache_ineffective"></a>
### `cache_ineffective` — The read cache rarely hits

The in-memory read cache is enabled but almost every lookup misses. *(efficiency, usually info)*

- **Causes:** an access pattern with little reuse; a working set larger than the cache
- **Verify:** report.cache (lookups, hits, hit ratio)
- **Knobs:** --cache SIZE; --access zipf:1.1

<a id="finding-sla_p99"></a>
### `sla_p99` — p99 latency exceeds the SLA

An operation's p99 is above the `--p99-ms` objective. *(sla, usually warning)*

- **Causes:** queueing or throttling at this concurrency; tail latency of the service
- **Verify:** report.verbs[].latency vs report.sla.p99_ms
- **Knobs:** --p99-ms; --concurrency; oprobe tune --objective latency

<a id="finding-rate_not_sustained"></a>
### `rate_not_sustained` — The target arrival rate was not sustained

With an open-loop `--rate`, arrivals queued behind busy workers; their wait is part of the measured latency. *(client, usually warning)*

- **Causes:** too few workers (`--concurrency`) for the rate; a rate above what the target can serve
- **Verify:** report.open_loop (target vs achieved rate, missed arrivals)
- **Knobs:** --rate; --concurrency

<!-- END finding-codes -->
