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
- a name, `[operators.NAME]` in the config file.

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
deletes the prefix. The guard enforces budgets (`--max-requests`, `--max-bytes`,
`--max-duration`), `--read-only` and `--dry-run`.

## Profile

A profile is what one operator did on one workload, saved as one `.oprobe` file:

- the operator spec and the workload, so the run replays exactly;
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
`oprobe show FILE` and the app open both. [Reading the profile →](reading-the-profile.md)

## Finding

A finding names a cause and shows its evidence. It is derived from the profile by fitting simple
limit models to the requests in flight over time (an in-flight ceiling where 429/503 start, a
request-rate ceiling, a bandwidth plateau, a hot key) and by reading the stage split (stragglers,
hung I/O, cold connections, small ranges, slow multipart, repeated reads, a busy client).

Every finding has a confidence: **high** only when the model explains most of the effect and
agrees across repeats. Throttling that made operations fail is reported as throttling, never as
an invalid run. The codes and their remedies are in the [README](../README.md#findings-and-their-remedy);
besides those, `errors` reports failures that are not throttling and `invalid_run` a run whose
numbers cannot be trusted (too short, nothing measured).

## Remedy

Each finding names exactly **one** remedy, as an `op.toml` path: `layers.concurrent_limit.http`,
`layers.tail_cut`, `read.gap`, `http.keep_alive`, … A remedy that is not a layer (spreading keys
over prefixes for `key_limit`) is reported as advice, and `client_bound` has no storage remedy.

`run` gives the remedy and **no number**. A single run varies no load, so it cannot say how
large a limit should be or how much it would help; claiming so would be a guess.

## Prove

`oprobe prove RUN [FINDING]` is the only source of numbers:

1. **Choose the change**: the finding's remedy (the top finding by default), or `--with PATH=VALUE`.
2. **Size it**: when the remedy has a size (a limit, a chunk), short trials over that one value
   find the knee of the objective, within `--max-throttle`.
3. **A/B test**: A and B alternate (A B A B …); a sequential test on the objective, against a
   noise floor learned from A/A pairs, stops as soon as it can decide, or at `--budget`.
4. **Decide**: `improved`, `worse` (exit 3) or `inconclusive` on the objective: **goodput**
   (successful operations per second; the default) or **p99**. Intervals are Wilson/beta and stay
   in range; secondary metrics are shown and never decide.
5. **Save**: every trial run is saved as a child of the prove run, hidden from `show --list`.

The result is a **proven layer diff**: the objective's change with its interval, the evidence,
and the change as Rust and as TOML. Every result states where it was validated (the lab, MinIO,
your bucket).

## Where oprobe stops

oprobe is not a load generator for benchmarking storage servers (use warp or elbencho), not a
monitoring system (use OpenDAL's metrics and OpenTelemetry layers, or `oprobe-trace`'s
exporter), not a CI regression gate and not a cloud bill estimator.
