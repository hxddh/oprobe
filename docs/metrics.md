# Metrics & methodology

oprobe measures; it does not estimate. This page explains exactly what every number means, where
it is captured, and the known limits.

## Where measurements come from

```
 your workload (oprobe workers)
   │  OpScope (task-local, follows the op into OpenDAL background tasks)
   ▼
 OpenDAL operator
   ├─ cache layer          → cache lookups/hits, lookup time
   ├─ retry / timeout / tail-cut layers → retry count, backoff (RetryLayer notify)
   ├─ service (s3, gcs, azblob, …)
   └─ HTTP transport  ◀── oprobe's own: connection pool + dialer + instrumented sockets
         queue → [dns → tcp → proxy CONNECT → tls] → send → ttfb → transfer (+ consumer stall)
```

* **Transport.** oprobe replaces OpenDAL's HTTP client with its own transport built on hyper's
  low-level HTTP/1.1 and HTTP/2 connections, rustls and its own connection pool. Because oprobe owns
  the pool, it knows *exactly* whether a request reused a connection or dialed a new one, and the
  dial phases belong to the request that waited for them.
* **Scope propagation.** A logical operation (e.g. one Parquet query) runs inside an `OpScope`.
  OpenDAL's concurrent readers/writers spawn background tasks; oprobe wraps OpenDAL's executor so
  those tasks inherit the scope. Every request is therefore attributed to the operation that caused
  it, even with `io_concurrency > 1`.
* **In-process backends** (`memory://`, `fs://`) have no HTTP; a bottom layer records each backend
  call's service time instead (shown as `service` / "backend").

## Request stages

For every backend request (columns of the *BACKEND REQUESTS* table):

| Stage | Starts | Ends |
|---|---|---|
| `queue` | `fetch()` called | in-flight permit acquired and a usable connection checked out (excluding dial time) |
| `connect` | — | sum of `dns + tcp + proxy + tls`, only for requests that **dialed** a new connection |
| `dns` | resolver call | addresses returned (to the proxy host when a proxy is used — the proxy resolves the target) |
| `tcp` | `connect()` | SYN/ACK completed |
| `proxy` | `CONNECT host:port` written | `200` received from the egress proxy |
| `tls` | ClientHello | handshake complete (ALPN negotiated) |
| `send` | connection ready | last request byte written to the socket (HTTP/1.1) / handed to the stream (HTTP/2) |
| `ttfb` | request sent | response headers received — server think time + one network round trip |
| `transfer` | headers received | body EOF, **minus** consumer stall |
| `stall` | — | time the body sat idle because the consumer had not polled it (application back-pressure) |

`total = queue + connect + send + ttfb + transfer + stall` for each request.

**HTTP/2 caveat.** Requests share one socket, so socket write timestamps cannot be attributed to a
single stream. On HTTP/2, `send` ends when the request body has been handed to the stream;
flow-controlled transmission of large uploads therefore appears in `ttfb`. Use `--http h1` when you
need to separate upload bandwidth from server latency.

## Operation breakdown ("where the time goes")

Each operation's wall time is split exactly into four parts by sweeping its request intervals:

* **backend** — at least one request is on the network (connect, send, ttfb or transfer);
* **queue** — no request is on the network, but at least one is waiting for a permit/connection;
* **cache** — local cache lookups (carved out of the remainder);
* **client** — everything else: OpenDAL processing, request signing, buffering, CRC, your code.

Parallel requests are merged (an instant counts once), so `queue + cache + backend + client` equals
the measured latency of every single operation. The report then splits *backend* into
connect / upload / server / download in proportion to the measured request-stage totals.

## Latency

* Per-operation latencies go into HDR histograms (3 significant digits, µs resolution).
* Closed loop (`concurrency` workers): latency = actual start → completion.
* Open loop (`rate`): latency = **intended** start → completion, so queueing caused by a slow
  service is included (no coordinated omission). Arrivals that cannot start because `concurrency`
  operations are already in flight are counted as `missed`.
* Warmup operations, and operations that started before the measurement window, are excluded.

## Bytes and amplification

| Metric | Definition |
|---|---|
| logical read / write | bytes the workload asked to read / wrote |
| backend down / up | HTTP response / request body bytes |
| wire rx / tx | raw socket bytes incl. TLS records, HTTP framing, proxy handshake |
| read amplification | backend down ÷ logical read (cache hits lower it, over-fetch raises it) |
| over-read | backend down ÷ (logical read − cache hit bytes): bytes fetched but never requested |
| write amplification | backend up ÷ logical write (retried uploads raise it) |
| requests per op | backend requests ÷ logical operations |

## Slowest operations and call stacks

The eight slowest operations of the window are kept with their complete request lists. While
operations run, oprobe samples [await-tree](https://github.com/risingwavelabs/await-tree) call
stacks every 20 ms for those slower than the current p90 (≥ 2 ms), including OpenDAL background
sub-tasks — so you can see e.g. `opendal::read → http::checkout → proxy CONNECT` for a stuck
operation. Request IDs (`x-amz-request-id`, `x-ms-request-id`, …) are kept for support tickets.

## Client resources

Process CPU (percent of one core) and RSS are sampled every second. When the load generator uses
more than 80 % of the machine's cores, the diagnosis warns that results may understate what the
storage can deliver.

## Integrity

Generated dataset objects are deterministic and their CRC32C is known; whole-object reads are
verified during the run. A sample of written objects is read back after the run with an untraced
client. Any mismatch is a critical finding and fails `oprobe diff`.
