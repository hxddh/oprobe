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
| `tail_cut` | `TailCutLayer` | cancel and re-issue requests slower than `percentile` (95, within 50..99) of the recent latencies × `safety_factor` (1.5, within 1..5), once `min_samples` (32) latencies are recorded within `window` ("60s", at most 120 s); the deadline is clamped to `min_deadline` ("50ms") .. `max_deadline` (unset: the shorter of `layers.timeout.op` and `layers.timeout.io`, the timeouts that sever the same calls, so the ceiling is one the layer can reach; 60 s without the timeout layer). These defaults are oprobe's, not OpenDAL's (1.3, 200, "500ms", "30s"); generated Rust spells out every value that differs from OpenDAL's, so pasted code behaves as measured |
| `throttle` | `ThrottleLayer` | `bandwidth` ("100MiB" per second), `burst` ("16MiB") |
| `cache` | `FoyerLayer` over an in-memory foyer cache, each entry weighed by its size | `memory` ("256MiB" of cached data): any object no larger than `memory` is cached whole; a larger one is read through |

### The order of the stack

The layers are stacked in one fixed order, whatever the order of the keys. A call passes them
from the outside in:

```text
cache → retry → throttle → concurrent_limit → tail_cut → timeout → service
```

The operator oprobe measures and the Rust it generates are both built from this one list, so
pasted code stacks the layers exactly as they were measured.

- **`timeout` and `tail_cut` time one call to the service and nothing else.** `tail_cut` is the
  outer of the two; its default ceiling is the shorter of the two timeouts, because OpenDAL's
  `TimeoutLayer` gives `stat`-like calls `op` and every read, write, list and delete call `io`.
- **Waiting for an operation permit is not timed.** `concurrent_limit` is outside both timers: a
  call that queues for `operations` neither runs into a timeout nor counts as a slow call that
  `tail_cut` would cut and re-issue into the same queue.
- **A throttled write waits before it takes a permit.** `throttle` is outside the limiter and the
  timers, so its wait is neither a held permit nor a slow call.
- **A write larger than `throttle.burst` fails; it does not wait.** `ThrottleLayer` refuses a
  single write above the burst with `rate_limited` ("burst size is smaller than the request
  size"). Keep `burst` at least as large as the largest single write: `write.chunk` when it is
  set (oprobe refuses a `burst` below it), otherwise the largest buffer the application writes
  at once, which for a workload without `write.chunk` can be a whole object.
- **Every attempt starts over.** `retry` is outside all of them: a retried call is throttled,
  limited and timed again, and a `stat` or a read gives its permit back during the backoff.
- **A cache hit touches nothing else.**

Two things OpenDAL's design leaves as they are:

- The HTTP permit of `concurrent_limit.http` is taken inside the service, where OpenDAL wraps
  the HTTP transport. Waiting for it is therefore always inside `timeout` and `tail_cut`: a
  request that queues behind a small `http` limit can time out, or be tail-cut, while it queues.
  The profile shows that wait as queue time.
- A writer, a lister and a deleter keep their operation permit from their first call until
  they are dropped, a retry's backoff included. So only the first write of an upload waits for
  the throttle without a permit; its later parts are throttled while the writer holds it.

### The cache

`cache = { memory = "256MiB" }` is one byte budget. The foyer cache behind it has a single
shard, weighs each entry by its object's size and admits any object no larger than `memory`;
`FoyerLayer` gets the same size limit, so a larger object is read through with ordinary ranged
requests and is never fetched or buffered whole. (Foyer's default of 8 shards splits the budget
by key hash: an object above an eighth of it could not be cached, and a budget that covers the
objects read still evicted whenever one shard filled first.) The generated Rust builds the
same cache: `.with_shards(1)`, the weighter, and `.with_size_limit(..=memory)`.

What `FoyerLayer` costs, as OpenDAL 0.59 implements it:

- **A ranged read caches the whole object.** The first read of an object, of any range, fetches
  the object whole and keeps it; later reads are slices of it. A cache therefore pays off for
  objects that are read again, and costs more than it saves for a few small ranges of large
  objects.
- **A `HEAD` before every read that misses.** The layer asks for the object's size first. An
  object larger than `memory` is then read through, and the next read of it asks again: one
  extra `HEAD` per read of an oversize object.
- **A write is buffered until it is closed.** A write through the layer is also kept in memory,
  up to `memory`, and enters the cache when the writer closes. A writer of a larger object
  stops buffering once it passes `memory`.
- **Nothing expires.** An entry stays until the budget evicts it. An object that another client
  changes or deletes is served from the cache as it was.

`prove` sizes `memory` from the distinct objects the run read, with a quarter of headroom, up to
1 GiB. It sizes no cache at all, and says why, when the largest object read is above 1 GiB, or
when the reads are ranges of objects that hold more bytes than the run read of them: the layer
would fetch more than it could save. `--with layers.cache.memory=SIZE` still tests any cache.

**Known issue (OpenDAL 0.59, versioned buckets): a stale read after a write.** The layer keys a
written object by the version the service returns for the write, and an ordinary read by no
version. On a bucket with versioning on, an object that was read, then overwritten through the
same operator, is still served as it was before the write, until the old entry is evicted. On a
bucket without versioning the write replaces the entry. Do not put `cache` on an operator that
overwrites objects it also reads in a versioned bucket.

## `[read]` and `[write]`

The options of `read_with` / `reader_with` and `writer_with`; unset means OpenDAL's default.

| Key | Meaning |
|---|---|
| `read.chunk` | read in chunks of this size |
| `read.concurrent` | chunks fetched at once |
| `read.gap` | merge ranges closer than this into one request |
| `write.chunk` | multipart part size (OpenDAL clamps it to the service's limits; the profile shows the effective value). Unset, OpenDAL fixes no part size: it buffers up to the service's minimum and sends each write as it comes, so the parts are as large as the caller's writes, and the profile shows the parts that were uploaded |
| `write.concurrent` | parts uploaded at once |

## `[http]`

OpenDAL's HTTP transport (`ReqwestTransport`, installed with
`OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client)))`).

| Key | Meaning |
|---|---|
| `version` | `auto` (OpenDAL's default: HTTP/1.1), `h1` or `h2` |
| `keep_alive` | reuse connections (default true) |
| `proxy` | egress proxy URL; `"none"` (also `--set http.proxy=none`) means no proxy, whatever `HTTPS_PROXY`/`HTTP_PROXY` say; unset (`--set http.proxy=unset`): the environment |
| `ca_cert` | an extra PEM CA bundle to trust (relative paths in `op.toml` resolve against the file's directory) |
| `insecure` | skip certificate verification (self-signed development endpoints only) |

## `--set`

`--set PATH=VALUE` (repeatable) changes one value of the operator before a run; `prove --with`
takes the same paths. The paths: `url`, `options.KEY`, `layers.NAME` (`on`/`off`),
`layers.NAME.FIELD`, `read.chunk`, `read.concurrent`, `read.gap`, `write.chunk`,
`write.concurrent`, `http.version`, `http.keep_alive`, `http.proxy`, `http.ca_cert`,
`http.insecure`. `none` removes an optional value, except for `http.proxy`, where it means no proxy
(`unset` goes back to the environment's).

`--set` also takes the workload's keys, the ones without a flag of their own included:
`concurrency`, `preset`, `duration`, `warmup` (e.g. `--set warmup=2s`), `max_ops`, `rate`,
`arrivals`, `shape`, `access`, `objects`, `object_size`, `partitions`, `existing`, `persist`,
`verify`, `cleanup`, `seed` and `mix`; and the run's `label`. There are no limits to set: the
findings judge a run, an open loop that stalled fails its checks by itself (see
[reading the profile](reading-the-profile.md#how-it-is-measured)), and `prove --max-throttle`
bounds the throttling a sized change may cause.

```bash
oprobe run op.toml --set layers.concurrent_limit.http=32 --set read.gap=1MiB
oprobe run s3://bucket/bench -d 10s --set warmup=1s
oprobe prove last --with layers.retry.jitter=true
```

## The config file

`$OPROBE_CONFIG`, or `<config dir>/oprobe/config.toml` (`~/.config/oprobe/config.toml` on Linux,
`~/Library/Application Support/oprobe/config.toml` on macOS, `%APPDATA%\oprobe\config.toml` on
Windows). It sets defaults:

```toml
[defaults]
preset = "range-reads"
duration = "30s"

[defaults.budgets]
max_requests = 200000
max_bytes = "20GiB"
```

Precedence of every setting: flags, then `OPROBE_*` environment variables, then `[defaults]`,
then the built-in default.

**Your operator belongs in an `op.toml` file**, passed by its path: `oprobe run prod.toml`. It
can live next to your code, be reviewed and versioned, and is what `prove`'s TOML diff patches.
The config file can also name an operator, as a shortcut for the same table:

```toml
[operators.prod]
url = "s3://prod-bucket/oprobe"
[operators.prod.options]
region = "eu-west-1"
[operators.prod.layers]
concurrent_limit = { http = 64 }
```

`oprobe run prod` then profiles that operator.

## The diff

`prove` and the app's Operator view show the difference between two operators as Rust builder
code and as TOML:

```text
+ ConcurrentLimitLayer::new(1 << 20).with_http_concurrent_limit(24)
~ RetryLayer::new().with_jitter().with_max_times(4)

[layers]
concurrent_limit = { http = 24 }
retry = { max_times = 4, jitter = true }
```

`+` adds a layer, `~` changes one, `-` removes one. The lines are in the order of the stack,
innermost first, and each expression is the one oprobe built the measured layer from.

Secrets are never part of the code. A credential option given as a value shows as
`"<redacted>"` in the TOML and, in the Rust, as a line that names the option without a value:
nothing to paste over the credential your program loads itself. An option given as an
environment reference (`secret_access_key = "${MY_SECRET}"`) is kept as the reference in the
TOML and read with `std::env::var("MY_SECRET")?` in the Rust.
