# Workloads & profiles

A profile is a TOML file describing **what** to benchmark (target), **how** (workload) and **with
which client settings** (knobs), plus the SLA used by `tune` and the volume used to project costs.
`oprobe init --preset parquet > my.toml` prints a fully commented template. Every flag of
`oprobe run` overrides the corresponding profile value.

## Target

```toml
[target]
url = "s3://my-bucket/oprobe"       # s3, gcs, azblob, oss, cos, b2, webdav, fs, memory
# insecure = false                  # skip TLS verification (dev only)
# ca_cert = "corp-ca.pem"           # extra CA bundle
# proxy = "http://proxy:3128"       # or "none"; default: HTTPS_PROXY / HTTP_PROXY / NO_PROXY

[target.options]                    # passed to Apache OpenDAL's service builder
region = "eu-west-1"
# endpoint = "https://minio.internal:9000"
# access_key_id = "${AWS_ACCESS_KEY_ID}"   # ${ENV} references are expanded at runtime
```

Option keys are checked against the OpenDAL service's configuration (its field names and
aliases): a key the service would silently ignore — a typo such as `regin`, or another service's
option — is an error with a "did you mean" suggestion.

Secrets are never written to reports: option keys containing `secret`, `key`, `token`,
`password`, `credential` or `sas` are redacted, and `--emit-profile` writes `${ENV}` placeholders.
Without explicit keys, the backend's usual credential chain is used (e.g. `AWS_*` variables,
`~/.aws`, instance metadata, `GOOGLE_APPLICATION_CREDENTIALS`).

URL forms: `s3://bucket/prefix`, `gcs://bucket/prefix`, `azblob://container/prefix` (set
`account_name`), `oss://bucket`, `cos://bucket`, `b2://bucket`, `webdav[s]://host/path`,
`fs:///absolute/path`, `memory://`.

`r2://bucket/prefix` and `minio://bucket/prefix` are S3 with a provider endpoint and never fall
back to AWS: R2 needs `account_id` (`-o account_id=<ID>`, giving
`https://<ID>.r2.cloudflarestorage.com` with region `auto`) or `--endpoint`; MinIO needs
`--endpoint` (path-style addressing is used). Without them oprobe stops with an error.

## Workload

```toml
[workload]
preset = "parquet"        # parquet | checkpoint | small-files | metadata | mixed
duration = "30s"          # measurement window
warmup = "5s"             # not measured: connections, TLS sessions, caches warm up
access = "zipf:1.1"       # uniform | zipf:<s> | hotset:<fraction>/<probability>
# rate = 500.0            # open-loop arrivals per second (default: closed loop)
# max_ops = 20000         # stop after N measured operations
verify = true             # CRC32C checks
cleanup = true            # delete everything under oprobe-run-*/ at the end
seed = 42                 # deterministic op/key selection

[workload.dataset]
objects = 16
object_size = "64MiB"
partitions = 4            # objects spread over part=NN/ prefixes (used by LIST)
# persist = true          # keep the dataset in oprobe-dataset/<fingerprint>/ for later runs
# existing_prefix = "warehouse/events/"   # read-only benchmark of your own objects
```

### Operation mix

If `[[workload.mix]]` is present it replaces the preset's mix. Each entry has an `op` and a
`weight`:

| `op` | Fields | What it does | Verb in reports |
|---|---|---|---|
| `read` | `size` (optional) | random aligned range of `size`, or the whole object | `read.range` / `read.full` |
| `parquet_scan` | `columns`, `column_size`, `footer_size` | `stat`, footer read, one vectored read of non-overlapping column chunks (coalesced with `range_gap`) | `parquet.scan` |
| `write` | `size` | single PUT | `write` |
| `multipart_write` | `size` | streaming writer, parts of `chunk_size`, `io_concurrency` parts in flight | `write.multipart` |
| `stat` | — | HEAD a dataset object | `stat` |
| `list` | `recursive` | LIST one partition | `list` |

```toml
[[workload.mix]]
op = "read"
size = "256KiB"
weight = 70

[[workload.mix]]
op = "write"
size = "1MiB"
weight = 30
```

Writes go to a bounded set of scratch keys that are overwritten, so long runs do not grow storage
without bound. Writes are skipped automatically for `existing_prefix` datasets.

## Knobs

```toml
[knobs]
concurrency = 16            # concurrent operations (closed loop) / in-flight cap (open loop)
io_concurrency = 4          # OpenDAL reader/writer .concurrent(n)
chunk_size = "8MiB"         # reader/writer .chunk(n); multipart part size
range_gap = "64KiB"         # reader .gap(n): merge ranges closer than this
max_inflight_requests = 0   # cap on concurrent HTTP requests (0 = unlimited)
http_version = "auto"       # auto | h1 | h2
keep_alive = true
cache = false               # in-memory byte-bounded LRU read cache
cache_size = "256MiB"
retries = 3                 # RetryLayer attempts (jittered exponential backoff)
tail_cut = false            # TailCutLayer: cancel & retry stragglers above adaptive p95
timeout = "60s"
```

## SLA and cost

```toml
[sla]                       # `oprobe run` / `tune` exit 3 when the result breaches it
enforce = true              # false (or `--sla none`): report the verdict, never fail on it
max_error_rate = 0.01       # default: at most 1% failed operations
p99_ms = 250.0              # worst per-operation p99 (unset: no latency objective)
max_throttle_rate = 0.0     # throttled-operation share (unset: `run` ignores throttling that
                            # retries absorb; `tune` treats any 429/503 as infeasible)
```

Corrupted data (a CRC32C mismatch on read-back) always breaches the SLA. Every SLA field can also
be set from the command line: `--max-error-rate 1%`, `--max-throttle-rate 0%`, `--p99-ms 250`,
`--sla none`.

```toml
[cost]
provider = "aws-s3"         # pricing preset; default: detected from the endpoint
monthly_ops = 100000000     # projection volume
stored_gib = 1024
egress_fraction = 0.0       # share of downloads that leave the cloud
# [cost.pricing]            # custom prices override the preset
```

See [cost.md](cost.md) for the pricing model.
