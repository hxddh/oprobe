# Concepts

oprobe has six ideas: the **operator** you profile, the **workload** you drive through it, the
**profile** that comes out, the **findings** in it, each finding's **remedy**, and **prove**, which
turns a remedy into a number.

## Operator

An OpenDAL `Operator` is a service (S3, GCS, Azure Blob, a file system, …), a stack of layers
(retry, timeout, concurrency limit, …) and the options of its reads and writes. That is OpenDAL's
own model, and it is the unit of everything in oprobe.

oprobe reads an operator from:

- a URL: `s3://bucket/prefix`, `gcs://bucket`, `azblob://container`, `fs:///nvme/x`, `memory://`,
  or `lab://SCENARIO` for the built-in lab; the layers are OpenDAL's defaults plus `retry` and
  `timeout`;
- an operator file, `op.toml` (format `oprobe.operator/6`): the URL, service options, layers,
  read/write options and HTTP client settings ([reference](operator.md));
- a name, `[operators.NAME]` in the config file (an `op.toml` path is the recommended way;
  [the config file](operator.md#the-config-file)).

`--set PATH=VALUE` changes one value (`--set layers.retry.max_times=5`, `--set read.gap=1MiB`).
The difference between two operators is itself a value: it renders as Rust builder code and as
TOML, and it is what `prove` hands you.

Comparing two operators that differ in a layer is the same thing as comparing two that differ in
service. So `oprobe run s3://a/x r2://b/x fs:///nvme/x` races three backends with no extra
feature.

## Workload

A workload is an OpenDAL access pattern. Four presets cover the common ones:

| Preset | What it does | Default dataset |
|---|---|---|
| `small-files` (default) | whole-object reads (60 %) and writes (30 %), stats (10 %) | 256 × 64 KiB |
| `large-objects` | multipart writes (80 %) and full reads (20 %) | 2 × 32 MiB |
| `range-reads` | parquet-like: a stat, the footer, then vectored column-chunk ranges | 8 × 16 MiB |
| `list-stat` | lists (40 %) and stats (60 %) over a partitioned prefix | 512 × 4 KiB in 16 partitions |

Shape it with `-c/--concurrency`, `-d/--duration`, `--objects`, `--object-size`, `--rate`
(open loop: operations start on a schedule and latency counts from the intended start, so a slow
server cannot hide behind fewer requests), `--seed` and `--existing PREFIX` (read the objects
that are already there, write nothing).

oprobe writes its dataset under a fresh `oprobe-run-<id>/` prefix, warms up, measures, and
deletes the prefix. A run killed before it could clean up leaves its prefix behind; the next run
against the same target deletes it. The guard enforces budgets (`--max-requests`, `--max-bytes`,
`--max-duration`), `--read-only` and `--dry-run`.

**Budgets hold on the wire.** `--max-requests` and `--max-bytes` count every HTTP request the
command sends to the target and every body byte, as they are sent and received:

- in every phase before cleanup: the reachability check, the preflight, the dataset upload, the
  warm-up, the measurement and the verification of written objects;
- retries and re-sent requests included, each with its body;
- a request the budget has no room for is never sent, and a download stops at the chunk that
  crosses the byte budget, so a budget is overshot by at most the response chunks in flight,
  never by a whole operation;
- cleanup is never counted and never refused: deleting the run prefix and aborting multipart
  uploads always run.

In-process services (`memory://`, `fs://`) send no HTTP request: their calls are counted as they
complete.

**A stopped run leaves no multipart upload.** When a budget, `--max-duration`, Ctrl-C (or
SIGTERM, SIGHUP) or Stop ends a run, the requests in flight are cut and the operations they
belong to end at once; they are neither completed nor failed operations, and are not in the
profile. Every multipart upload a failed, timed-out or stopped write or dataset upload had
started is aborted (`AbortMultipartUpload`), so no part stays on the target to be billed where no
object listing shows it. An upload whose abort fails is named in the cleanup error.

## Profile

A profile is what one operator did on one workload, saved as one `.oprobe` file:

- the operator spec and the workload, so the run can be reproduced exactly;
- every operation and every backend request under it, stage by stage: **queue** (waiting for a
  connection or a permit), **connect** (DNS, TCP, proxy, TLS on a fresh connection), **send**,
  **TTFB** (the server's time to the first byte), **body** (the transfer), **stall** (the body
  waiting for your code), and **retry backoff** between attempts;
- the aggregates: throughput and goodput, latency percentiles, per API and per stage, connection
  reuse, bytes and amplification, a per-second timeline and a latency heatmap;
- the findings.

Each operation's wall time splits exactly into queue + backend + retry + client, so "network,
server or my code?" has a measured answer. Long runs keep every aggregate exact and sample whole
operations (failed, throttled and slowest ones are always kept), so memory stays bounded.

`.oprobe` files come from `oprobe run` and from [`oprobe-trace`](embedding.md) in production;
`oprobe show FILE` and the app open both. A production trace has the same shape: every OpenDAL
call your application makes is an operation with its requests, its keys hashed. What it lacks is
a workload oprobe chose, so nothing to replay: `prove` needs a run. [Reading the profile
→](reading-the-profile.md)

## Finding

A finding names a cause and shows its evidence. It is derived from the profile by fitting simple
limit models to the requests in flight over time (an in-flight ceiling where 429/503 start, a
request-rate ceiling, a bandwidth plateau, a hot key) and by reading the stage split (stragglers,
hung I/O, cold connections, small ranges, slow multipart, repeated reads, a busy client).

Every finding has a confidence: **high** only when the model explains most of the effect and
agrees across repeats. Throttling that made operations fail is reported as throttling, never as
an invalid run. The codes and their remedies are in the [README](../README.md#findings-and-their-remedy);
besides those, `errors` reports failures that are not throttling (when retries recovered every
operation, its remedy is a shorter first retry backoff with jitter, `layers.retry.min_delay`) and
`invalid_run` a run whose numbers cannot be trusted (too short, nothing measured).

## Remedy

Each finding names exactly **one** remedy, as an `op.toml` path: `layers.concurrent_limit.http`,
`layers.tail_cut`, `read.gap`, `http.keep_alive`, … A remedy that is not a layer (spreading keys
over prefixes for `key_limit`) is reported as advice, and `client_bound` has no storage remedy:
their `next` says where to look (`oprobe show RUN --detail`), and `prove` refuses them.

`run` gives the remedy and **no number**. A single run varies no load, so it cannot say how
large a limit should be or how much it would help; claiming so would be a guess.

## Prove

`oprobe prove RUN [FINDING]` is the only source of numbers:

1. **Choose the change**: the finding's remedy (the top finding by default), or `--with PATH=VALUE`.
2. **Size it**: when the remedy has a size (a limit, a chunk), short trials over that one value
   find the knee of the objective, within `--max-throttle`.
3. **A/B test**: trials of A and B alternate in pairs (the order within each pair drawn from the
   run's seed); a sequential test on the objective, against a noise floor learned from each
   side's consecutive trials (the A/A spread until B has its own), stops as soon as it
   can decide, or when the wall-time budget (`--max-duration`) would be exceeded.
4. **Decide**: `improved`, `worse` (exit 3) or `inconclusive` on the objective: **goodput**
   (successful operations per second; the default) or **p99**. The objective's interval is
   Student's t on the paired log ratios (B over A, pair by pair), with a spread no smaller than
   the A/A noise floor, so it never goes below −100 %. Shares (throttled requests, errors) are
   compared with Newcombe's interval. Secondary metrics are shown and never decide. Deciding
   takes at least 3 pairs of trials; with fewer, the verdict is `inconclusive`.
5. **Save**: every trial is saved as a run, a child of the prove, hidden from `show --list`.

The result is a **proven layer diff**: the objective's change with its interval, the evidence,
and the change as Rust and as TOML. Every result states where it was validated (the lab, MinIO,
your bucket).

## Glossary

The terms a profile and a proof print, in one line each.

| Term | Meaning |
|---|---|
| **operation** | one OpenDAL call (`read`, `write`, `stat`, `list`, …); it may send several requests |
| **request** | one HTTP request to the service; retries and range parts are requests of one operation |
| **throughput** | operations (or MiB) per second, failed ones included |
| **goodput** | *successful* operations per second: the work that got done; `prove`'s default objective |
| **p50 / p99** | the latency half / 99 % of operations stay under; p99 is the slowest 1 % |
| **TTFB** | time to first byte: from the request sent to the first byte of the answer (the server's time, plus one network round trip) |
| **queue · connect · send · body · stall** | the stages of a request: waiting for a connection or a permit, opening one (DNS, TCP, TLS), sending, receiving the body, and the body waiting for your code |
| **retry backoff** | time an operation sleeps between attempts after a 429, 503 or another retryable failure |
| **throttled** | answered 429 (Too Many Requests) or 503 (Slow Down): the service refuses load; `throttled %` is the share of requests |
| **in flight** | requests sent and not yet answered at one moment; a limit "starts above N in flight" |
| **stragglers** | the few requests far slower than a typical one, usually waiting on the server (TTFB) |
| **amplification** | bytes or requests per operation above what the operation needed (over-read ranges, retries) |
| **cold connection** | a request that had to open a new connection instead of reusing one |
| **open loop** | operations start on a schedule (`--rate`), so a slow service cannot hide behind fewer requests |
| **confidence** | how well a finding's model explains the evidence: high, medium or low |
| **95 % interval** | the range `[low, high]` that holds the true change in 95 of 100 repeats of the proof; a change whose interval includes 0 is not proven |
| **noise floor (`±`)** | how much two runs of the *same* operator differ (the A/A spread); a change smaller than it cannot be told from noise |
| **pair of trials** | one short run of A (the operator as it is) and one of B (with the change), back to back |
| **verdict** | `ok`, `review` (look before trusting the numbers), `attention` (a finding costs time) or `critical` (failures or a large cost) |

## Where oprobe stops

oprobe is not a load generator for benchmarking storage servers (use warp or elbencho), not a
monitoring system (use OpenDAL's metrics and OpenTelemetry layers, or `oprobe-trace`'s
exporter), not a CI regression gate and not a cloud bill estimator.
