# The operator file: `op.toml`

An operator file describes one OpenDAL `Operator`: the service, its layer stack, the options of
its reads and writes, and its HTTP client. Its format is `oprobe.operator/6` (JSON Schema:
[`schemas/operator.json`](../schemas/operator.json)). The same value builds the operator oprobe
measures, is what `--set` changes, and renders the difference between two operators as Rust and
TOML.

```toml
url = "s3://bucket/prefix"          # any OpenDAL scheme oprobe is built with; lab:// too
[options]                            # OpenDAL service options, checked against the service
region = "eu-west-1"
[layers]                             # each key = one OpenDAL layer; absent = its default
concurrent_limit = { http = 24 }                    # ConcurrentLimitLayer
retry = { max_times = 3, jitter = true, min_delay = "50ms", max_delay = "5s" }   # RetryLayer
timeout = { op = "30s", io = "10s" }                # TimeoutLayer
tail_cut = { percentile = 95 }                      # TailCutLayer
throttle = { bandwidth = "100MiB", burst = "16MiB" }  # ThrottleLayer
cache = { memory = "256MiB" }                       # FoyerLayer (in-memory)
[read]
chunk = "8MiB"
concurrent = 4
gap = "64KiB"
[write]
chunk = "8MiB"
concurrent = 4
[http]
version = "auto"          # auto | h1 | h2
keep_alive = true
```

Unknown keys are errors. Sizes are `"64KiB"`, `"8MiB"`, `"1GB"` or a number of bytes; durations
are `"50ms"`, `"10s"`, `"5m"`.

## `url`

The service: `s3://bucket/prefix`, `gcs://bucket/prefix`, `azblob://container/prefix`,
`oss://…`, `cos://…`, `obs://…`, `b2://…`, `webdav://…`, `fs:///path`, `memory://`, or
`lab://SCENARIO` for the built-in lab. Release builds include S3, GCS, Azure Blob, OSS, COS, OBS,
B2 and WebDAV; `cargo install oprobe --features all-services` adds the rest OpenDAL offers to
oprobe (Azure Data Lake, Azure Files, Swift, TOS, WebHDFS, UpYun, HTTP).

## `[options]`

OpenDAL service options, by their OpenDAL names: `region`, `endpoint`, `access_key_id`,
`enable_virtual_host_style`, `account_name`, `credential_path`, … An option the service does not
know is an error that names the closest valid one (`config.unknown_option`). Values may reference
environment variables as `${NAME}`. Credential options are redacted before anything is printed or
saved.

## `[layers]`

Each key is one OpenDAL layer. The defaults are OpenDAL's, plus `retry` and `timeout`, which
OpenDAL recommends:

- a key that is absent keeps its default: **on** for `retry` and `timeout`, **off** for the rest;
- `false` (or `--set layers.NAME=off`) takes the layer out of the stack;
- `true` (or `--set layers.NAME=on`) puts it in with its default parameters;
- a table puts it in with those parameters.

| Key | OpenDAL layer | Fields (default) |
|---|---|---|
| `concurrent_limit` | `ConcurrentLimitLayer` | `http`: HTTP requests in flight (`with_http_concurrent_limit`); `operations`: operations in flight (`ConcurrentLimitLayer::new`; unset: unlimited) |
| `retry` | `RetryLayer` | `max_times` (3), `jitter` (false), `min_delay` ("1s"), `max_delay` ("60s"), `factor` (2.0) |
| `timeout` | `TimeoutLayer` | `op`: one operation ("60s"); `io`: one read or write call ("10s") |
| `tail_cut` | `TailCutLayer` | cancel and re-issue requests slower than `percentile` (95, within 50..99) of the recent latencies × `safety_factor` (1.5, within 1..5), once `min_samples` (32) latencies are recorded within `window` ("60s", at most 120 s); the deadline is clamped to `min_deadline` ("50ms") .. `max_deadline` (unset: the operation timeout `layers.timeout.op`, 60 s without the timeout layer). These defaults are oprobe's, not OpenDAL's (1.3, 200, "500ms", "30s"); generated Rust spells out every value that differs from OpenDAL's, so pasted code behaves as measured |
| `throttle` | `ThrottleLayer` | `bandwidth` ("100MiB" per second), `burst` ("16MiB") |
| `cache` | `FoyerLayer` over an in-memory foyer cache | `memory` ("256MiB") |

## `[read]` and `[write]`

The options of `read_with` / `reader_with` and `writer_with`; unset means OpenDAL's default.

| Key | Meaning |
|---|---|
| `read.chunk` | read in chunks of this size |
| `read.concurrent` | chunks fetched at once |
| `read.gap` | merge ranges closer than this into one request |
| `write.chunk` | multipart part size (OpenDAL clamps it to the service's limits; the profile shows the effective value) |
| `write.concurrent` | parts uploaded at once |

## `[http]`

The `HttpFetch` client (OpenDAL's reqwest client).

| Key | Meaning |
|---|---|
| `version` | `auto` (OpenDAL's default: HTTP/1.1), `h1` or `h2` |
| `keep_alive` | reuse connections (default true) |
| `proxy` | egress proxy URL; `"none"` ignores `HTTPS_PROXY`/`HTTP_PROXY` (unset: the environment) |
| `ca_cert` | an extra PEM CA bundle to trust |
| `insecure` | skip certificate verification (self-signed development endpoints only) |

## `--set`

`--set PATH=VALUE` (repeatable) changes one value of the operator before a run; `prove --with`
takes the same paths. The paths: `url`, `options.KEY`, `layers.NAME` (`on`/`off`),
`layers.NAME.FIELD`, `read.chunk`, `read.concurrent`, `read.gap`, `write.chunk`,
`write.concurrent`, `http.version`, `http.keep_alive`, `http.proxy`, `http.ca_cert`,
`http.insecure`. `none` removes an optional value.

```bash
oprobe run op.toml --set layers.concurrent_limit.http=32 --set read.gap=1MiB
oprobe prove last --with layers.retry.jitter=true
```

## The config file

`$OPROBE_CONFIG`, or `<config dir>/oprobe/config.toml` (`~/.config/oprobe/config.toml` on Linux,
`~/Library/Application Support/oprobe/config.toml` on macOS, `%APPDATA%\oprobe\config.toml` on
Windows). It names operators and sets defaults:

```toml
[defaults]
preset = "range-reads"
duration = "30s"

[defaults.budgets]
max_requests = 200000
max_bytes = "20GiB"

[operators.prod]
url = "s3://prod-bucket/oprobe"
[operators.prod.options]
region = "eu-west-1"
[operators.prod.layers]
concurrent_limit = { http = 64 }
```

`oprobe run prod` then profiles that operator. Precedence of every setting: flags, then
`OPROBE_*` environment variables, then `[defaults]`, then the built-in default.

## The diff

`prove` and the app's Operator view show the difference between two operators as Rust builder
code and as TOML:

```text
+ ConcurrentLimitLayer::new(24)
~ RetryLayer::new().with_jitter().with_max_times(4)

[layers]
concurrent_limit = { http = 24 }
retry = { max_times = 4, jitter = true }
```

`+` adds a layer, `~` changes one, `-` removes one.
