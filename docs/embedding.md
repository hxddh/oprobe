# Profiling production: `oprobe-trace`

[`oprobe-trace`](https://crates.io/crates/oprobe-trace) is oprobe's instrumentation as one
OpenDAL layer. Add it to the operator your service already uses, and every backend request is
timed by stage and aggregated in bounded memory. Save a `.oprobe` file and open it with
`oprobe show FILE` or in the app, like any run.

It is the only crate oprobe publishes with a semver promise. (`oprobe-core` is on crates.io only
so that `cargo install oprobe` works; it is internal.)

**Requirements.** OpenDAL 0.59 (`opendal` or `opendal-core`; another minor shows up as trait
errors about `Layer` or `Servicer`), the Tokio runtime, Rust 1.97 or later, and, for the `otel`
feature, `opentelemetry` 0.32 (the version OpenDAL's `opendal-layer-otelmetrics` uses, so both
layers share one global meter).

## Three lines

```toml
[dependencies]
oprobe-trace = "7"
```

```rust
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = Operator::new(builder)?.layer(layer);
handle.save("prod.oprobe")?;          // then: oprobe show prod.oprobe
```

Apply it first, before your other layers (`ConcurrentLimitLayer`, `TailCutLayer`, `RetryLayer`): it
installs the instrumented `HttpTransport` on `OperationContext`, and the layers above it then wrap
it, so HTTP concurrency caps stay in force and every retry is recorded as its own request. Give
OpenDAL's retry layer the handle's notifier, so retries land in the profile's Retries section
(count, backoff, throttling) and the attempts of one call count as one operation:

```rust
let (trace, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = Operator::new(builder)?
    .layer(trace)
    .layer(RetryLayer::new().with_notify(handle.retry_notify()));
```

Without the notifier every attempt is an operation of its own, and retries are not counted.

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
  records `http.client.request.duration` (seconds) once per stage, with the attributes
  `oprobe.stage` (`total`, `queue`, `connect`, `send`, `ttfb`, `body`, `stall`), `oprobe.api`,
  `http.request.method` and `http.response.status_code`, and OpenDAL's own metric labels
  (`scheme`, `namespace`, `root`, `operation`), so its series join `opendal-layer-otelmetrics`'.

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

Within 7.x the API documented on docs.rs, the `oprobe.trace/7` file format and the meaning of
every field are stable: changes are additive. Public structs and enums are `#[non_exhaustive]`,
so read fields freely but build `Config` with its methods. The `__internal` feature exists for the
oprobe binary and has no semver promise; do not enable it.

## The file

`oprobe.trace/7` is one JSON document, readable without oprobe:

```text
{ "format": "oprobe.trace/7", "oprobe_trace_version": "7.0.0", "created_at": "…",
  "service": { "scheme": "s3", "name": "my-bucket", "root": "/data/" },
  "snapshot": { … }, "rows": { "ops": [ … ], "reqs": [ … ], "info": { … }, "sampling": { … } } }
```

`service` is the OpenDAL service the layer was applied to: its scheme, name (the bucket or
container) and root, with no options. A bare `snapshot.save` has neither `service` nor `rows`.
`rows.ops` are the operations; each request row names its operation (`op_id`), the length of the
byte range it asked for (`detail.range_bytes`) and, when it failed, its class
(`storage.condition_not_match`, `storage.already_exists`, `storage.conflict`,
`storage.range_not_satisfied`, `storage.throttled`, …) and whether the failure is temporary
(`detail.temporary`).

**Keys are hashed.** An operation's `key` is its first path segment (at most 32 bytes) and a
48-bit hash of the whole key: `logs/2026/11/a.parquet` is recorded as `logs/#3f2a…`. Prefixes and
hot keys still slice the traffic, and repeated reads are still told apart, but the file never
holds a key. Request IDs and remote addresses can be in it; credentials never are. Treat a
profile like a log file before you paste it into an issue.

**Migrating from 6.x.** `oprobe.trace/6` files still open: they hold requests only, so each
request counts as one operation there, and the report says so. A 7.x file counts operations;
request counts are no longer labelled as operations. `TraceLayer` wraps every service now, not
only `memory` and `fs`.
