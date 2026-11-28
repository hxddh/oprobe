# Profiling production: `oprobe-trace`

`oprobe-trace` is oprobe's instrumentation as one
OpenDAL layer. Add it to the operator your service already uses, and every backend request is
timed by stage and aggregated in bounded memory. Save a `.oprobe` file and open it with
`oprobe show FILE` or in the app, like any run.

It is the only crate oprobe publishes, and it has a semver promise.

**Requirements.** OpenDAL 0.59 (`opendal` or `opendal-core`; another minor shows up as trait
errors about `Layer` or `Servicer`), the Tokio runtime, Rust 1.91 or later (OpenDAL 0.59's own
minimum; the oprobe workbench itself needs 1.97), and, for the `otel` feature, `opentelemetry`
0.32 (the version OpenDAL's `opendal-layer-otelmetrics` uses, so both layers share one global
meter).

## Three lines

```toml
[dependencies]
oprobe-trace = { git = "https://github.com/hxddh/oprobe", tag = "v9.4.0" }
```

`oprobe-trace` is not on crates.io yet: the git dependency above is the one that resolves today
(pin the tag of the release you read this from). Once a release publishes it, the same version
is `oprobe-trace = "9"`.

```rust
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = Operator::new(builder)?.layer(layer);
handle.save("prod.oprobe")?;          // then: oprobe show prod.oprobe
```

Apply it first, before your other layers (`ConcurrentLimitLayer`, `TailCutLayer`, `RetryLayer`): it
installs the instrumented `HttpTransport` on `OperationContext`, and the layers above it then wrap
it, so HTTP concurrency caps stay in force and every retry is recorded as its own request. The
order matters, and so does a client of your own: see [Where the layer goes](#where-the-layer-goes).
Give OpenDAL's retry layer the handle's notifier, so retries land in the profile's Retries section
(count, backoff, throttling) and the attempts of one call count as one operation:

```rust
let (trace, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = Operator::new(builder)?
    .layer(trace)
    .layer(RetryLayer::new().with_notify(handle.retry_notify()));
```

Without the notifier every attempt is an operation of its own, and retries are not counted.
When you use a local cache layer (`FoyerLayer`, `MokaLayer`), wrap it with `handle.cache_layer()`
so cache hits, hit bytes and lookup latency are recorded in `snapshot.cache` and `snapshot.bytes`:

```rust
let op = op.layer(foyer_layer).layer(handle.cache_layer());
```

## Where the layer goes

DNS, TCP, TLS and connection statistics can only be measured from inside the HTTP client. So by
default the layer brings its own client (built like OpenDAL's default one) and **replaces the
HTTP transport installed on the operator**. OpenDAL composes transports in the order of the
`.layer(...)` calls: each layer receives the transport the layers before it left, and wraps or
replaces it. Two things follow:

- **Layers that wrap the transport go after the trace layer.**
  `ConcurrentLimitLayer::with_http_concurrent_limit` added *after* the trace layer wraps the
  instrumented client and limits it. Added *before*, it wraps a transport the trace layer then
  replaces, and silently stops limiting. Add `handle.queue_layer()` last, and the time requests
  wait for the limiter is their `queue` stage:

  ```rust
  let op = Operator::new(builder)?
      .layer(trace)                                                   // first
      .layer(ConcurrentLimitLayer::new(64).with_http_concurrent_limit(16))
      .layer(RetryLayer::new().with_notify(handle.retry_notify()))
      .layer(handle.queue_layer());                                   // last
  ```

- **A client of your own is not used.** A transport you installed with
  `Operator::with_context(OperationContext::new().with_http_transport(..))`, or the process
  default of `HttpTransporter::install_default`, is replaced too, with its proxy, TLS roots,
  timeouts and pool settings. Either say the same on `Config` (`proxy`, `extra_ca_pem`,
  `http_version`, `keep_alive`, `max_inflight`; see [Configuration](#configuration)), or keep your
  client:

```rust
let cfg = oprobe_trace::Config::default().keep_http_transport(true);
let (trace, handle) = oprobe_trace::layer(cfg);
let op = Operator::new(builder)?
    .with_context(OperationContext::new().with_http_transport(my_client))
    .layer(ConcurrentLimitLayer::new(64).with_http_concurrent_limit(16)) // before or after: both work
    .layer(trace);
```

With **`keep_http_transport(true)`** the layer wraps the installed transport instead of replacing
it: your client, and every limiter beneath the trace layer, carries the requests unchanged,
wherever the layer sits. The layer then sees each request from outside:

| | the instrumented client (default) | `keep_http_transport(true)` |
|---|---|---|
| queue, time to first byte, body, consumer stall | yes | yes |
| status, bytes, request IDs, provider error codes, retries, operations | yes | yes |
| DNS, TCP, proxy and TLS time, upload send time | yes | no: part of the time to first byte |
| connections: reuse, dials, TLS details, kernel TCP statistics | yes | no |
| your proxy, TLS roots, timeouts, pool | no: set them on `Config` | yes |

`format!("{trace:?}")` says which one a layer does (`http_transport: "replaced by the
instrumented client"` or `"the installed transport, observed"`).

Each step of the integration adds signals; the smallest one changes no client:

| integration | operations, requests, stages, status, bytes | DNS/TCP/TLS, connections | retries counted, attempts linked |
|---|---|---|---|
| `.layer(trace)` with `keep_http_transport(true)` | yes | no | no |
| `.layer(trace)` (the instrumented client) | yes | yes | no |
| either, plus `RetryLayer::with_notify(handle.retry_notify())` | yes | as above | yes |

Without `retry_notify`, every attempt of a retried call is an operation of its own (a retried
failure is a failed operation) and `retries_count` stays 0. A trace says so:
`resilience.retry_linking` is `"unlinked"`, and `oprobe show FILE` prints how to link them
(9.4; before, "no retries" and "retries not linked" both read 0).

What was not observed is not reported as a number. With `keep_http_transport(true)`:
- `Snapshot::connections` counts the requests by protocol, and nothing else:
  `connections.observed()` is false, and `dialed`, `reused_requests` and `reuse_pct` say nothing
  (until 9.2 every answered request counted as reused: "100 %"). `oprobe show FILE` prints
  `dials not observed · reuse not observed`.
- The file's `service` says `"http_transport": "installed"` and leaves out `http_version` and
  `keep_alive`, which describe the instrumented client (the client is yours).
- A request row has `conn_id` 0, and its time to first byte includes connecting and sending the
  body: a slow upload shows as a long `ttfb`, not as a slow server.

## What you get

Every OpenDAL call is one **operation**, linked to the backend requests it issued, as in a run
oprobe drives: `stat`, `read.full` / `read.range` (one `open` until its stream ends, or one ranged
read), `write` / `write.multipart` (the writer's life), `list`, `delete` (a batch), `copy`,
`rename`, `create_dir`. So a trace has verbs, operation latency and where its time went (queue,
backend, retry, client), the slowest operations with their requests, hot keys and prefixes, size
buckets, cold and warm reads, and over-read. A call your code drops (a timeout, a cancelled
future) is recorded as `cancelled`. Keys are recorded hashed: see [The file](#the-file).

- **`handle.snapshot()`**: a `Snapshot` with the aggregates so far: per storage API and stage
  (queue, connect, send, TTFB, body, stall; count, mean, p50/p90/p99/p99.9/max), per verb,
  connection reuse and dial times, retries and status codes, bytes, a timeline. Aggregates are
  exact.
- **`handle.save(path)`**: a `.oprobe` profile: the snapshot plus a sample of whole operations
  with their requests (failed, throttled and slowest ones are always kept, within
  `Config::max_rows`). The rows saved are moved out of memory, so periodic saves hold the
  operations of each period; the aggregates are cumulative. `snapshot.save(path)` writes the
  aggregates alone.
- **`handle.save_and_reset(path)`**: the same, then a fresh window: the aggregates start again
  from zero (atomically with the snapshot), so hourly files are hourly profiles.
- A save that cannot be written (a missing directory, a directory, no permission, a disk
  without room for the file) fails before it takes anything: the rows and the window stay in
  memory for the next save. The file's room is reserved first, and the file appears whole or
  not at all.
- **`Exporter`**: called for every request as it completes, on the request path (keep it cheap).

```rust
struct Slow;
impl oprobe_trace::Exporter for Slow {
    fn on_request(&self, r: &oprobe_trace::RequestRecord) {
        if r.timing.ttfb_us > 500_000 {
            eprintln!("slow {} {} ttfb {} ms", r.method, r.api.label(), r.timing.ttfb_us / 1000);
        }
    }
}
let cfg = oprobe_trace::Config::default().exporter(std::sync::Arc::new(Slow));
```

- **OpenTelemetry** (feature `otel`, `opentelemetry` 0.32): `oprobe_trace::otel::config(&meter)`
  records two histograms, in seconds:
  - `http.client.request.duration`, the standard metric of the HTTP semantic conventions:
    **exactly once per HTTP request**, with standard attributes only (`http.request.method`,
    `server.address`, `server.port`, and `http.response.status_code`,
    `network.protocol.version`, `error.type` when they apply). Its count is the number of
    requests, as any HTTP dashboard assumes.
  - `oprobe.http.client.request.stage.duration`: where each request's time went, once per stage
    that took time. `oprobe.stage` is `queue`, `dns`, `tcp`, `proxy`, `tls`, `send`, `ttfb`,
    `body` or `stall` (they add up to the request's duration; until 9.3 the four parts of a
    dial were one `connect`); it also carries `oprobe.api`,
    `http.request.method`, `http.response.status_code` and OpenDAL's own metric labels
    (`scheme`, `namespace`, `root`, `operation`), so its series join
    `opendal-layer-otelmetrics`'. `scheme`, `namespace` and `root` are those of the operator
    that issued the request: a layer cloned onto several operators labels each one's requests
    with its own (until 9.2, with the first operator's). Your own `Exporter` reads the same
    from `RequestRecord::service`.
  - `http.client.request.body.size` and `http.client.response.body.size` (bytes, 9.4): the
    standard body-size metrics, with the attributes of `http.client.request.duration`.
  - `oprobe.http.client.retries` (a counter) and `oprobe.http.client.retry.backoff.duration`
    (seconds, 9.4): each retry a `RetryLayer` with `handle.retry_notify()` makes, by
    `oprobe.retry.reason`: `throttled` (429/503), `server_error`, `timeout`, `tail_cut`,
    `network` or `other`. Your own `Exporter` hears them in `on_retry`.

  Backend calls of `memory` and `fs` are not HTTP requests and are in neither. Operation
  durations and cache metrics are not repeated: use OpenDAL's own metrics layers for them.
  (Until 9.1 the stages were recorded into `http.client.request.duration` itself, under
  `oprobe.stage`, so every request counted up to seven times; query the new histogram for them.)

## Configuration

`Config` is a builder; every setting has the default OpenDAL's own client has:

| Method | Default | Meaning |
|---|---|---|
| `max_rows(n)` | 10 000 | request rows kept in memory (aggregates are exact regardless; 0 keeps aggregates only) |
| `http_version(v)` | `HttpVersion::Auto` | `Auto` (HTTP/1.1, as OpenDAL), `H1` or `H2` |
| `keep_alive(on)` | true | reuse connections |
| `max_inflight(n)` | unlimited | cap concurrent HTTP requests; waiting shows as `queue` |
| `extra_ca_pem(pem)` | none | trust an extra PEM bundle besides the system and WebPKI roots |
| `insecure_skip_verify(on)` | false | skip certificate verification (development only) |
| `proxy(Some(url) \| None)` | the environment | an explicit `http://` proxy, or none |
| `https(on)` | true | selects `HTTPS_PROXY` over `HTTP_PROXY` |
| `tcp_info(on)` | true | read kernel TCP statistics at the end of each request (Linux, macOS) |
| `keep_http_transport(on)` | false | wrap the operator's own HTTP transport instead of replacing it ([Where the layer goes](#where-the-layer-goes)); the client settings above then do not apply, `max_inflight` does |
| `exporter(e)` | none | your `Exporter` |

`handle.transport_error()` says why HTTP requests are not instrumented, if the transport could not
be built (an invalid proxy URL, a CA bundle that does not parse). The operator keeps working; only
in-process services (`memory`, `fs`) are observed then.

## How it works

For HTTP services the layer replaces the operator's `HttpTransport` with OpenDAL's own reqwest client
(rustls with aws-lc-rs, the same pool defaults: no connect timeout, unlimited idle connections per
host, 90 s idle timeout), observed through reqwest's public extension points: a timed DNS resolver,
a connector layer that times each dial, a TLS session store that timestamps the ClientHello, and a
body wrapper that times the transfer and your code's consumption. Nothing is decrypted or
proxied. For `memory` and `fs` it records one request per backend call.

A connection's dial belongs to the request that started it: that request is the connection's
cold one, whichever request is answered first on a shared HTTP/2 connection; a request that
waited for another's dial shows the wait as queue time (`detail.dial_wait_us`). A dial whose
request went elsewhere is a connection nobody waited for: counted, charged to no request.
Kernel TCP statistics are read at the end of each request, and the connection's retransmits
since the previous reading are charged to the requests in flight on it, so
`detail.retrans_count` adds up to the connection's count and no retransmit is charged twice:
- to the requests that are **sending a body**, when there are any (what a client retransmits
  is what it sends): an upload is charged its own retransmits, not the small GET that ends
  beside it on a shared HTTP/2 connection;
- else evenly to all of them (on HTTP/1.1 that is the one request).

A request counts as sending from the moment the transport can tell which connection carries
its body: with one upload in flight, the only connection alive, or, once the body is handed
to the kernel, the only one with unsent bytes in its send queue. With several uploads in
flight it cannot, and an upload joins at its response headers; a request that ends on the
same HTTP/2 connection before that is charged the upload's retransmits so far. An upload that
fails or is cancelled before any response carries the retransmits of its connection when that
connection was known.

The socket of a new connection is looked up on the runtime's blocking pool (one scan of the
descriptor table answers every connection dialed meanwhile), never on a worker thread, and no
request waits for it: a request reads the answer when its response headers arrive and again
when it ends. A request that ends before the lookup answered (the first request of a
connection, when the blocking pool is busy) has no kernel statistics; what its connection sent
and retransmitted counts with the connection's next request. Connection ids (`conn_id`) number
the client's connections in the order they were dialed, from 1.

A response OpenDAL cannot use (a `Content-Encoding` or `Content-Length` that does not parse)
fails the operation and is recorded as a failed request, not to be retried.

Above the transport, the layer wraps the service: each call runs in an operation scope that the
transport (and OpenDAL's background tasks, through a scope-propagating executor) reports its
requests to. The memory stays bounded however long the process lives: rows are sampled within
`Config::max_rows`, and past about 2 000 intervals the oldest timeline intervals are merged in
pairs (a month-long trace keeps under 2 000 intervals, the recent ones at full resolution).

`oprobe show FILE` marks the report `run.source = "trace"` and shows `production trace` instead
of a workload. Its findings are `errors`, `throttling` (and `rate_limit`, `key_limit`),
`hung_requests`, `tail_stragglers`, `cold_connections`, and, from the operations,
`small_ranges`, `slow_multipart` (when the operations issued their requests one after another)
and `repeated_reads`. The ones that need a load oprobe varied or its own client are not:
`bandwidth_cap` and `client_bound`. A trace has no workload to replay, so `oprobe prove` refuses
it. Reproduce the traffic with `oprobe run OPERATOR` and prove that run.

## Overhead

```bash
cargo bench -p oprobe-trace --bench overhead              # ~10 s
cargo bench -p oprobe-trace --bench overhead -- --quick   # ~1 s, runs in CI
```

The bench runs the same operations on a plain operator and on a traced one, interleaved, and
reports the median difference per operation: on `memory://`, and over HTTP against a local server
(OpenDAL's `http` service: OpenDAL's own reqwest client against the layer's instrumented one).
The budget is **2 µs per operation** for the layer's bookkeeping, measured on `memory://`; the
bench fails above it. On a 4-core Linux VM (busy with other builds):

```text
oprobe-trace overhead on memory:// (31 rounds × 50000 ops)
  plain OpenDAL         1.032 µs/op
  with oprobe-trace     2.363 µs/op
  overhead              1.322 µs/op  (p10 1.119, p90 1.547; budget 2 µs)
oprobe-trace overhead over HTTP on loopback (31 rounds × 4000 ops)
  plain OpenDAL        19.874 µs/op
  with oprobe-trace    25.222 µs/op
  overhead              5.167 µs/op  (p10 0.979, p90 8.327)
```

`memory://` is the worst case: its backend call costs well under a microsecond, so the difference
is all bookkeeping (an operation and its request). Over HTTP the transport's hooks add a few
microseconds to a request; against object storage, where a request takes milliseconds, the layer
is lost in the noise.

## Stability

Within 9.x the API documented on docs.rs is stable: changes are additive. A trace file is read
by the oprobe of the version that wrote it; reading the files of another version is not
promised. Public structs and enums are `#[non_exhaustive]`,
so read fields freely but build `Config` with its methods. The `__internal` feature exists for the
oprobe binary and has no semver promise; do not enable it.

## The file

`oprobe.trace/9` is one JSON document, readable without oprobe:

```text
{ "format": "oprobe.trace/9", "oprobe_trace_version": "9.3.2", "created_at": "…",
  "service": { "scheme": "s3", "name": "my-bucket", "root": "/data/", "http_version": "auto", "keep_alive": true },
  "snapshot": { … }, "rows": { "ops": [ … ], "reqs": [ … ], "info": { … }, "sampling": { … } } }
```

`service` is the OpenDAL service the layer was applied to: its scheme, name (the bucket or
container), root and client posture (`http_version`, `keep_alive`, `max_inflight_count`), with no
credentials or options. A bare `snapshot.save` has neither `service` nor `rows`.
`rows.ops` are the operations (each carrying its exact sampling `weight` when thinned); each
request row names its operation (`op_id`), its strongly typed storage `api`, the length of the
byte range it asked for (`detail.range_bytes`) and, when it failed, its class
(`storage.condition_not_match`, `storage.already_exists`, `storage.conflict`,
`storage.range_not_satisfied`, `storage.throttled`, …) and whether the failure is temporary
(`detail.temporary`).

**Keys are hashed.** An operation's `key` is its first path segment (at most 32 bytes) and a
48-bit hash of the whole key: `logs/2026/11/a.parquet` is recorded as `logs/#3f2a…`. Prefixes and
hot keys still slice the traffic, and repeated reads are still told apart, but the file never
holds a key. Request IDs and remote addresses can be in it; credentials never are: an error
text loses the credentials of every URL in it (`user:password@`, the signature of a presigned
URL, an SAS `sig`, STS and access tokens) when it is recorded, before any exporter, snapshot or
file sees it, and the errors the instrumented client returns to your code quote URLs the same
way. Treat a profile like a log file before you paste it into an issue.
