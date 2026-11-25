# oprobe-trace

Profile any [Apache OpenDAL](https://opendal.apache.org) operator in production. One layer times
every backend request by stage (queue, DNS, TCP, proxy, TLS, send, time to first byte, body,
consumer stall), classifies it by storage API, keeps bounded aggregates, and writes `.oprobe`
files that the [oprobe](https://github.com/hxddh/oprobe) workbench opens like any other profile.

```rust,ignore
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = op.layer(layer);
handle.save("prod.oprobe")?; // then: oprobe show prod.oprobe
```

- **Operations:** every OpenDAL call is an operation linked to the requests it issued (verbs,
  latency and where it went, slow operations, hot keys and prefixes with keys hashed);
  `Handle::retry_notify()` for OpenDAL's `RetryLayer::with_notify` counts retries.
- **The API:** `layer(Config) -> (TraceLayer, Handle)`, `Handle::snapshot()` for the aggregates
  (per API and stage, per verb, connections, bytes, timeline), `Handle::save(path)` /
  `Handle::save_and_reset(path)` / `Snapshot::save(path)` for a `.oprobe` file (format
  `oprobe.trace/9`, one JSON document); `Handle::cache_layer()` records local cache hits
  (`FoyerLayer`, `MokaLayer`) and `Handle::queue_layer()` measures semaphore wait in outer
  concurrency layers.
- **Your own sink:** implement `Exporter` and pass it with `Config::exporter`. With the `otel`
  feature (opentelemetry 0.32), `otel::OtelExporter` records `http.client.request.duration`,
  `oprobe.operation.duration` and `oprobe.cache.lookups` with OpenDAL's `scheme`, `namespace`,
  `root` and `operation` labels.
- **Faithful:** for HTTP services the layer installs an instrumented `HttpFetch` built like
  OpenDAL's default reqwest client (rustls with aws-lc-rs, the same pool defaults); the HTTP
  version, keep-alive, extra CA, proxy and in-flight cap are `Config` settings.
- **Bounded:** aggregates are exact; trace rows are sampled whole within `Config::max_rows`; a
  month-long timeline keeps under 2 000 intervals.
- **Cheap:** `cargo bench -p oprobe-trace --bench overhead` compares the same operations with
  and without the layer; the budget is 2 µs per operation.
- **Stable:** the documented API, the file format and the meaning of every field are stable
  across 9.x; public structs and enums are `#[non_exhaustive]`.
- **Requires:** OpenDAL 0.59, Tokio, Rust 1.97.

Features: `otel` (OpenTelemetry metrics), `schemars` (`JsonSchema` for the snapshot types).
The `__internal` feature is for the oprobe binary only and has no semver promise.

More: [docs/embedding.md](https://github.com/hxddh/oprobe/blob/main/docs/embedding.md).

License: Apache-2.0.
