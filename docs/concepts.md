# Concepts

Four things: **targets** you measure, **runs** that record a measurement, **baselines** you compare against,
and **findings** that explain a run.

## Targets

A target is a named storage endpoint: a URL, options and where its credentials come from. Any command that
takes a target accepts:

- a **name** from the config file (`oprobe target add prod s3://bucket/prefix`);
- a **URL**: `s3://bucket/prefix`, `gcs://bucket`, `azblob://container`, `oss://`, `cos://`, `b2://`,
  `webdav://`, `fs:///path`, `memory://` (with `--endpoint`, `--region`, `--option KEY=VALUE` as needed);
- `lab://SCENARIO`: an S3-compatible server with programmable faults inside oprobe (no credentials).

```toml
# <config dir>/oprobe/config.toml   (oprobe config path · oprobe config edit)
[defaults]
target = "prod"            # used when a command is given none
duration = "30s"

[targets.prod]
url = "s3://my-bucket/oprobe"
region = "eu-west-1"
credentials = "profile:prod"     # env | profile:NAME | anonymous; default: the provider's chain
options = { enable_virtual_host_style = "true" }   # OpenDAL options, checked against the service
```

Every setting has a precedence: flags > `OPROBE_*` environment > target > `[defaults]` > built-in.
`oprobe config show` prints each effective value and where it comes from.

| Command | |
|---|---|
| `oprobe target add NAME URL [--region --endpoint --credentials --option K=V]` | Save (or replace) a target; a quick read-only check follows (`--no-test` skips it). |
| `oprobe target list` / `show NAME` / `rm NAME` | Manage. Secrets in options are never printed. |
| `oprobe target test NAME [--read-only]` | The doctor: DNS, TCP, TLS, HTTP, clock skew, credentials, list/write/read/stat/delete. |

## Runs

A run is one measurement. It is immutable once finished and saved as one file,
`<data dir>/oprobe/runs/<run-id>.oprobe` (`oprobe config path` shows where; `OPROBE_DATA_DIR` moves it).
The file holds the full resolved configuration, the environment, the report and the trace: one row per
operation and one per backend request (up to 1 000 000 requests, sampled beyond that — aggregates stay exact).

Commands refer to runs with a **RUNREF**:

| RUNREF | Means |
|---|---|
| `20261120-143005-a1b2` | a run id |
| `20261120-14` | a unique id prefix |
| `last`, `last~1`, `last~2` | the newest run, the one before, … |
| `before` | the newest run with that `--label` |
| `./ci/run.oprobe` | a run file anywhere |

| Command | |
|---|---|
| `oprobe runs list [-n N]` | Saved runs: id, label, target, when, window, verdict, ops/s, p99, cost/month. |
| `oprobe runs show [RUNREF] [--detail]` | The full report (`-o html`, `-o md`, `-o json`). |
| `oprobe runs open RUNREF` | Open the run in the app. |
| `oprobe runs export RUNREF --format html\|md\|json\|ndjson\|csv\|har\|otlp` | Export. |
| `oprobe runs rm RUNREF…` | Delete runs. |
| `oprobe runs gc --keep 50 --older-than 30d [--dry-run]` | Delete old runs; runs in a baseline are kept. |

## Baselines

A baseline is a named, rolling set of runs (default: the last 10) that new runs are compared against.

```bash
oprobe run prod --baseline main          # compare against `main`, then add this run to it
oprobe baseline list
oprobe compare --baseline main last      # gate the last run against the baseline
```

`oprobe compare A B` compares two runs directly. A comparison lists every configuration difference first:
a different workload or target fails (the numbers are not comparable), different client knobs are an A/B
comparison, a different host or oprobe version is a warning. Exit code 3 means a regression.

## Findings

A finding is one diagnosis item with:

- a stable **code** (`throttling`, `tail_stragglers`, `cold_connections`, `server_latency`, …) — `oprobe explain CODE`
  documents it;
- a **severity**: info, warning or critical;
- a one-sentence **cause**;
- **evidence**: the numbers, and references to the operations and time windows that show it;
- **advice**: what to change;
- optionally a **verification**: a concrete command that tests the advice.

The **verdict** sums a run up: `ok`, `review` (worth a look), `attention` (warnings) or `critical`
(critical findings or a breached SLA), with a one-sentence headline of where the time went.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | runtime failure (authentication, network, storage, internal) |
| 2 | usage or configuration error |
| 3 | SLA failed or regression |
| 4 | a budget stopped the run (`--max-cost-usd`, `--max-requests`, `--max-bytes`, `--max-duration`) |
| 5 | safety refusal (e.g. a write under `--read-only`) |
| 130 | interrupted (Ctrl-C); the partial run is saved |

With `-o json` or `-o ndjson`, errors are printed on stdout as `{"type":"error","error":{"code":…}}`
with a stable dotted code such as `auth.denied` or `net.timeout`.
