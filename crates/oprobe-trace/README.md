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
  feature (opentelemetry 0.32), `otel::OtelExporter` records every HTTP request once into the
  standard `http.client.request.duration` (standard attributes only) and its stages into
  `oprobe.http.client.request.stage.duration` (`oprobe.stage`, with OpenDAL's `scheme`,
  `namespace`, `root` and `operation` labels, those of the operator that issued the request:
  one layer may be cloned onto several operators).
- **Faithful:** for HTTP services the layer installs an instrumented `HttpTransport` built like
  OpenDAL's default reqwest client (rustls with aws-lc-rs, the same pool defaults); the HTTP
  version, keep-alive, extra CA, proxy and in-flight cap are `Config` settings.
- **Your client stays yours, if you say so:** by default that instrumented client *replaces*
  the operator's HTTP transport, so add the trace layer **before** any layer that wraps the
  transport (`ConcurrentLimitLayer::with_http_concurrent_limit` added before it would stop
  limiting), and a custom client installed with `Operator::with_context` is not used. With
  `Config::keep_http_transport(true)` the layer wraps the installed transport instead: your
  client and the limiters beneath carry every request, and the layer records what is visible
  from outside (no DNS/TCP/TLS split, no connection statistics: the profile says "not
  observed", it does not report none dialed and all reused).
- **Nothing to leak:** error texts lose the credentials they carry (`user:pass@`, presigned
  signatures, SAS and STS tokens, in a URL, a bare query, a fragment or a quoted
  `Authorization` header) before they reach an exporter, a snapshot or a file, and the errors
  the instrumented client returns quote URLs the same way.
- **Never in the way:** no request waits for the layer. The socket lookup behind the kernel
  TCP statistics runs beside the requests, and survives a runtime that is shut down under it
  (an operator shared by tests that each build a runtime).
- **Bounded:** aggregates are exact; trace rows are sampled whole within `Config::max_rows`; a
  month-long timeline keeps under 2 000 intervals.
- **Cheap:** `cargo bench -p oprobe-trace --bench overhead` compares the same operations with
  and without the layer; the budget is 2 µs per operation.
- **Stable:** the documented API is stable across 9.x; public structs and enums are
  `#[non_exhaustive]`. A trace file is read by the version that wrote it.
- **Requires:** OpenDAL 0.59, Tokio, Rust 1.91 (OpenDAL 0.59's own minimum).

Features: `otel` (OpenTelemetry metrics), `schemars` (`JsonSchema` for the snapshot types).
The `__internal` feature is for the oprobe binary only and has no semver promise.

More: [docs/embedding.md](https://github.com/hxddh/oprobe/blob/main/docs/embedding.md).

License: Apache-2.0.
