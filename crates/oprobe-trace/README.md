# oprobe-trace

The instrumentation of [oprobe](https://github.com/hxddh/oprobe) as a library: add one layer to
an [Apache OpenDAL](https://opendal.apache.org) operator and every backend request is timed by
stage (queue, DNS, TCP, proxy, TLS, send, time to first byte, body, stall), classified by
storage API and aggregated in bounded memory.

```rust,ignore
let (layer, handle) = oprobe_trace::layer(oprobe_trace::Config::default());
let op = op.layer(layer);
// … use the operator …
let snapshot = handle.snapshot(); // per API and stage, connections, bytes, timeline
```

- **Always on:** no run or measurement window; requests outside any operation scope are
  aggregated and sampled like the others.
- **Bounded:** aggregates are exact; trace rows are sampled whole operations within
  `Config::max_rows`.
- **OpenTelemetry** (feature `otel`): `otel::OtelExporter` records
  `http.client.request.duration` with `oprobe.stage` and `oprobe.api` attributes.
- **Your own sink:** implement `Exporter` and pass it in `Config::exporter`.

License: Apache-2.0.
