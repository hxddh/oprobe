# Profiling production: `oprobe-trace`

[`oprobe-trace`](https://crates.io/crates/oprobe-trace) is oprobe's instrumentation as one
OpenDAL layer. Add it to the operator your service already uses, and every backend request is
timed by stage and aggregated in bounded memory. Save a `.oprobe` file and open it with
`oprobe show FILE` or in the app, like any run.

It is the only crate oprobe publishes with a semver promise. (`oprobe-core` is on crates.io only
so that `cargo install oprobe` works; it is internal.)

## Three lines

```toml
[dependencies]
oprobe-trace = "6"
```

```rust
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = op.layer(layer);
handle.save("prod.oprobe")?;          // then: oprobe show prod.oprobe
```

Apply it first, before your other layers (`Operator::new(builder)?.layer(trace).layer(RetryLayer::new())`):
it installs the instrumented `HttpFetch`, and the layers above it then wrap it, so every retry
is recorded as its own request.

## What you get

- **`handle.snapshot()`**: a `Snapshot` with the aggregates so far: per storage API and stage
  (queue, connect, send, TTFB, body, stall; count, mean, p50/p90/p99/p99.9/max), per verb,
  connection reuse and dial times, retries and status codes, bytes, a per-second timeline.
  Aggregates are exact.
- **`handle.save(path)`**: a `.oprobe` profile: the snapshot plus a sample of whole requests
  (failed, throttled and slowest requests are always kept, within `Config::max_rows`). The rows
  saved are moved out of memory, so periodic saves hold the requests of each period; the
  aggregates are cumulative. `snapshot.save(path)` writes the aggregates alone.
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

- **OpenTelemetry** (feature `otel`): `oprobe_trace::otel::config(&meter)` records
  `http.client.request.duration` (seconds) once per stage, with the attributes `oprobe.stage`
  (`total`, `queue`, `connect`, `send`, `ttfb`, `body`, `stall`), `oprobe.api`,
  `http.request.method` and `http.response.status_code`.

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

For HTTP services the layer replaces the operator's `HttpFetch` with OpenDAL's own reqwest client
(rustls with aws-lc-rs, the same pool defaults: no connect timeout, unlimited idle connections per
host, 90 s idle timeout), observed through reqwest's public extension points: a timed DNS resolver,
a connector layer that times each dial, a TLS session store that timestamps the ClientHello, and a
body wrapper that times the transfer and your code's consumption. Nothing is decrypted or
proxied. For `memory` and `fs` it records one request per backend call.

Without oprobe's workload driving it, requests are not grouped into operations: a profile from
`oprobe-trace` has requests and their aggregates. `oprobe show FILE` marks its report
`run.source = "trace"` and shows `production trace` instead of a workload. Each request counts
as one operation there. The findings that requests support are derived: `errors`, `throttling`
(and `rate_limit`, `key_limit`), `hung_requests`, `tail_stragglers` and `cold_connections`.
The ones that need operations or a load oprobe varied are not: `small_ranges`,
`slow_multipart`, `repeated_reads`, `bandwidth_cap` and `client_bound`. A trace has no workload
to replay, so `oprobe prove` refuses it. Reproduce the traffic with `oprobe run OPERATOR` and
prove that run.

## Overhead

```bash
cargo bench -p oprobe-trace --bench overhead              # ~10 s
cargo bench -p oprobe-trace --bench overhead -- --quick   # ~1 s, runs in CI
```

The bench runs the same operations on a plain `memory://` operator and on a traced one,
interleaved, and reports the median difference per operation. The budget is **2 µs per
operation**; the bench fails above it. On a 4-core Linux VM:

```text
oprobe-trace overhead on memory:// (31 rounds × 50000 ops)
  plain OpenDAL         0.798 µs/op
  with oprobe-trace     1.190 µs/op
  overhead              0.379 µs/op  (p10 0.295, p90 0.578; budget 2 µs)
```

`memory://` is the worst case: its backend call costs well under a microsecond, so the difference
is all bookkeeping. Against object storage, where a request takes milliseconds, the layer is lost
in the noise.

## Stability

Within 6.x the API documented on docs.rs, the `oprobe.trace/6` file format and the meaning of
every field are stable: changes are additive. Public structs and enums are `#[non_exhaustive]`,
so read fields freely but build `Config` with its methods. The `__internal` feature exists for the
oprobe binary and has no semver promise; do not enable it.

## The file

`oprobe.trace/6` is one JSON document, readable without oprobe:

```text
{ "format": "oprobe.trace/6", "oprobe_trace_version": "6.0.0", "created_at": "…",
  "service": { "scheme": "s3", "name": "my-bucket", "root": "/data/" },
  "snapshot": { … }, "rows": { "ops": [], "reqs": [ … ], "info": { … }, "sampling": { … } } }
```

`service` is the OpenDAL service the layer was applied to: its scheme, name (the bucket or
container) and root, with no options. A bare `snapshot.save` has neither `service` nor `rows`.

Object keys, request IDs and remote addresses can be in it; credentials never are. Treat a
profile like a log file before you paste it into an issue.
