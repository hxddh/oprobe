# Clients and distributed load

By default oprobe generates load with OpenDAL through its own instrumented transport, so it
sees every stage of every request. Two questions need more than that:

- **Is my SDK the bottleneck?** Run the same workload with the client your application uses
  (`--client`), then `oprobe compare` the runs.
- **What happens at a load one machine cannot generate?** Spread the run over several machines
  or processes (`--workers`).

Both work with every workload, budget and report section of a normal run.

## Choosing the client (`--client`)

| `--client` | What drives the load | Stages observed (`report.client.phases_available`) |
|---|---|---|
| `opendal` (default) | OpenDAL over oprobe's transport | queue, connect, send, ttfb, body, stall, backoff |
| `aws-sdk` | aws-sdk-s3 (Rust) over oprobe's transport | queue, connect, send, ttfb, body |
| `exec:COMMAND` | an adapter speaking the [oprobe client protocol](#the-oprobe-client-protocol) | what the adapter reports |

```sh
oprobe run s3://bucket/prefix --client aws-sdk
oprobe run s3://bucket/prefix --client "exec:python3 adapters/boto3/oprobe_boto3.py"
oprobe compare RUN_OPENDAL RUN_BOTO3
```

- **`aws-sdk`** is compiled in with the cargo feature `aws-sdk`
  (`cargo install oprobe --features aws-sdk`); builds without it refuse the choice. The SDK's
  HTTP client is oprobe's transport, so its requests (retries included) are recorded with
  their stages; the SDK's own retry backoff counts as client time. It drives S3 targets and
  takes credentials from the target (`access_key_id`, `secret_access_key`, `session_token`) or
  the `AWS_*` environment variables.
- **`exec:COMMAND`** starts COMMAND once per run (no shell: quotes group words) and drives it
  over stdin and stdout. Request rows come from the adapter's report, timed by the adapter's
  clock, so the protocol's own cost is not counted as the SDK's. Integrity checks of reads are
  off (adapters report sizes, not bytes).
- **The client is not part of the run's spec.** `oprobe run --from RUN` replays with the default
  client unless `--client` is given again; a run file can never start a command.
- **Findings follow the stages.** A finding that needs a stage the client cannot observe (for
  example `server_latency` needs `ttfb`, `cold_connections` needs `connect`) is removed, and
  `report.warnings` names it. With `exec`, `client_overhead` and `client_cpu` are removed too:
  time outside the SDK includes the protocol.
- `report.client` is `{ kind, version, phases_available }`, e.g.
  `{"kind": "exec", "version": "boto3 1.44.9 (botocore 1.44.9)", "phases_available": ["ttfb", "body"]}`.

### Reference adapters (`adapters/`)

| Adapter | Run with | Stages |
|---|---|---|
| [`minimal`](../adapters/minimal/oprobe_minimal.py) | `exec:python3 adapters/minimal/oprobe_minimal.py` | connect, send, ttfb, body |
| [`boto3`](../adapters/boto3/oprobe_boto3.py) | `pip install boto3`, then `exec:python3 adapters/boto3/oprobe_boto3.py` | ttfb, body (botocore event hooks) |
| [`awscrt`](../adapters/awscrt/oprobe_awscrt.py) | `pip install awscrt`, then `exec:python3 adapters/awscrt/oprobe_awscrt.py` | ttfb, body (meta request callbacks) |
| [`go-sdk`](../adapters/go-sdk/main.go) | `cd adapters/go-sdk && go build -o oprobe-go-sdk .`, then `exec:adapters/go-sdk/oprobe-go-sdk` | queue, connect, send, ttfb, body (`net/http/httptrace`) |

`minimal` uses only the Python standard library and sends unsigned path-style requests: it
drives the lab (`oprobe lab`) and is the template for an adapter of your own. The others use
their SDK's credential chain; oprobe sets `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_SESSION_TOKEN` and `AWS_REGION` in the adapter's environment when the target has them.
Each is checked against the lab with every operation:

```sh
OPROBE_TEST_ADAPTER="python3 $PWD/adapters/boto3/oprobe_boto3.py" \
  cargo test -p oprobe-core --lib adapter_from_env -- --ignored
```

## The oprobe client protocol

Version **`oprobe.client/1`**. NDJSON: one JSON object per line, UTF-8, `\n`-terminated.
oprobe writes to the adapter's stdin and reads its stdout; the adapter writes diagnostics to
stderr only (oprobe shows its last lines when it fails). The JSON Schemas are
`oprobe schema client-hello`, `client-hello-reply`, `client-op` and `client-result`.

Numeric fields carry their unit in the name (`_bytes`, `_us`, `_ms`, `_count`), as in every
oprobe JSON document.

**Versioning.** The major version is in the name. Adapters must accept any `oprobe.client/1.x`
and ignore fields they do not know; oprobe accepts an adapter whose `hello.protocol` is
`oprobe.client/1` or `oprobe.client/1.x`, and ignores unknown result fields. Fields are only
added within a major version.

### Session

1. oprobe starts the adapter and sends `hello`:

   ```json
   {"type":"hello","protocol":"oprobe.client/1","target":{"scheme":"s3","bucket":"bkt","endpoint":"http://127.0.0.1:9000","region":"us-east-1","path_style":true,"insecure":false},"concurrency_count":32,"retries_count":3,"timeout_ms":30000}
   ```

   `target.endpoint` is absent for the provider's default endpoint; `ca_cert_file` names an
   extra PEM CA bundle. Credentials are never sent over the protocol.

2. The adapter answers within 60 s:

   ```json
   {"type":"hello","protocol":"oprobe.client/1","client":"boto3","version":"1.44.9","phases":["ttfb","body"],"ops":["get","put","head","delete","list","copy"],"features":["if_match","if_none_match","metadata","batch_delete"]}
   ```

   | Field | Meaning |
   |---|---|
   | `client`, `version` | shown as `report.client.version` |
   | `phases` | stages the adapter measures: `queue`, `connect`, `send`, `ttfb`, `body` |
   | `ops` | operations it implements (empty: all); the workload mix is adapted to them |
   | `features` | `if_match`, `if_none_match` (conditional writes), `metadata` (user metadata), `batch_delete` (one request deletes many keys) |

3. oprobe sends operations, at most `concurrency_count` in flight. The adapter answers each
   with one result carrying its `id`, **in any order**: run them concurrently.
4. `{"type":"shutdown"}`, or EOF on stdin: finish what is in flight and exit.

### Operations

Keys are full object keys (the target's root included).

| `op` | Fields | The adapter |
|---|---|---|
| `get` | `key`, `range` (absent: whole object; `{"offset_bytes":0,"length_bytes":4096}`, `{"offset_bytes":10}` to the end, `{"suffix_bytes":8192}` the last bytes) | GETs; reports `bytes` read and the object's total `size_bytes` |
| `put` | `key`, `size_bytes`, `data_b64` (standard base64), `if_match`, `if_none_match` (`*`: create only), `content_type`, `metadata` | PUTs exactly these bytes; reports `etag` |
| `head` | `key` | reports `size_bytes`, `etag` |
| `delete` | `keys` | deletes them (a missing key is not an error); lists failures in `failed_keys` |
| `list` | `prefix`, `recursive` (false: delimiter `/`), `limit` (page size) | lists every page; reports `entries` (`{"key","size_bytes","dir"}`) and the pages as `requests_count` |
| `copy` | `source`, `key` | server-side copy |

### Results

```json
{"id":7,"ok":true,"status":200,"bytes":4096,"size_bytes":65536,"etag":"\"9b2c…\"","t_start_us":81234001,"t_end_us":81236512,"phases":{"connect_us":0,"send_us":40,"ttfb_us":2100,"body_us":350},"attempts_count":1}
{"id":8,"ok":false,"status":404,"t_start_us":81236600,"t_end_us":81237100,"error":{"code":"NoSuchKey","message":"The specified key does not exist."}}
```

| Field | Meaning |
|---|---|
| `id`, `ok` | required |
| `status` | HTTP status of the final attempt; `0`: no response (network error) |
| `t_start_us`, `t_end_us` | start and end inside the client on any monotonic clock; only the difference is used |
| `phases` | stage durations of the final attempt (µs): `queue_us`, `dns_us`, `tcp_us`, `tls_us` (or `connect_us` when they cannot be split), `send_us`, `ttfb_us`, `body_us`. Time not attributed counts as `ttfb` |
| `attempts_count` | 1 + retries (default 1) |
| `attempt_log` | optional: every attempt `{status, t_start_us, t_end_us, phases?, error?}`; each becomes a request row, so throttled (429/503) attempts count as throttling |
| `requests_count` | HTTP requests of the operation (list pages, per-key deletes; default 1) |
| `error` | `{code, message}`: the service's code (`NoSuchKey`, `SlowDown`, `PreconditionFailed`, …) or the exception name |
| `http_version` | `http1` or `http2` |
| `bytes`, `size_bytes`, `etag`, `entries`, `failed_keys` | per operation, above |

oprobe maps a failure to its error kinds by status and code: 404/`NoSuchKey` is not found,
412/`PreconditionFailed` a failed condition, 403 denied, 429/503/`SlowDown` throttled.

### Writing an adapter

Start from `adapters/minimal/oprobe_minimal.py`: read `hello`, answer it, then for each line
run the operation on a pool of `concurrency_count` workers and write its result under one lock.
Retry the way your application does (`retries_count`), and report attempts (`attempts_count`, `attempt_log`). Never exit while
operations are in flight, never write anything but results to stdout, and answer every `id`,
also when an exception escapes. Check it against the lab with `adapter_from_env` (above).

## Distributed load (`--workers`)

```sh
# on each load machine
openssl rand -hex 32 > token && chmod 600 token
oprobe worker --listen 0.0.0.0:7070 --token-file token

# on the coordinator
oprobe run s3://bucket/prefix -c 96 -d 60s --workers 10.0.0.11:7070,10.0.0.12:7070*2 --worker-token-file token

# or several processes on this machine (more cores, or testing)
oprobe run s3://bucket/prefix -c 64 --workers local:4
```

A fixed set of workers can live in the config file and be used with `--workers config`
(the token file there is also the default for explicit addresses):

```toml
[workers]
addrs = ["10.0.0.11:7070", "10.0.0.12:7070*2"]
token_file = "/etc/oprobe/worker.token"
```

**The worker** serves the worker protocol (`oprobe.worker/1`: JSON over HTTP/2, h2c or TLS
with `--tls-cert`/`--tls-key`). Every request needs the token (`Authorization: Bearer`); the
worker refuses a token file other users can read and tokens under 16 characters. It runs one
run at a time, and an `exec:` client only when started with `--allow-exec` (local workers
always allow it); the command is resolved on the worker's machine.

**The coordinator** (the `oprobe run` with `--workers`):

1. checks every worker: protocol and oprobe version (they must match), the clients it can
   drive, and its clock offset by an NTP-style exchange (8 pings, the shortest round trip wins);
2. splits the run by weight (`host:port*WEIGHT`, default 1): concurrency, `--rate`, a
   `--shape` (scaled to each share; `--arrivals poisson` runs an independent process per
   worker) and `--max-ops` are divided; `--replay` is split by key (local workers only:
   each worker reads the log from its own disk); a generated dataset is divided (each worker seeds its share under
   its own prefix `oprobe-run-<id>/wN/`); an existing dataset (`--existing`) is split by key
   range;
3. prepares every worker (session, preflight, dataset), then starts them all at one instant,
   each on its own clock;
4. polls each worker every second: merges their live snapshots for progress, adds their
   requests and bytes to the global budgets, and fans a Ctrl-C, a breached budget or a failed
   worker out to every worker;
5. collects their snapshots and trace rows and merges them into one run.

**Safety.** Budgets are global: each worker gets its share and enforces it, and the coordinator
enforces the totals. Each worker deletes only its own prefix. Credentials are sent only to
loopback or TLS workers; other workers get the redacted spec and resolve credentials from their
own configuration and environment, as `oprobe run --from` does.

**The merged run** is saved like any other. Counters, bytes and rates are exact sums. Latency
distributions are the mixture of the workers' distributions (exact counts, means and maxima;
close quantiles), and where the run's trace rows are complete the report's percentiles come from
the merged rows, so the capacity fit and the findings see the load of every worker. The trace row
limit is shared by weight, so the saved run file keeps it. Operation ids
are made unique per worker and row times are shifted onto the common window.

- `report.workers[]`: `{ id, addr, clock_offset_ms, ops_count, requests_count, ops_per_s,
  warnings[] }`. A worker warns when its clock differs by more than 100 ms (start times are
  corrected either way), when the offset is uncertain by more than ±5 ms, or when it started its
  window more than 50 ms after the first worker.
- `report.dimensions.workers[]`: per-worker slices (share of requests, throttled and error
  ratios, p99).

Not combined with `--workers`: `--repeat` and `--baseline` (run the distributed run once per
repetition).
