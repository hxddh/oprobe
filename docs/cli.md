# CLI reference

oprobe has four commands and an alias. Without a command it opens [the app](app.md).

```text
oprobe                     # the app (a native window, or the terminal without a display)
oprobe run   OPERATOR…     # profile; several operators = side by side
oprobe demo                # = oprobe run lab://demo
oprobe prove RUN [FINDING] # A/B test → the proven layer diff
oprobe show  [RUN|FILE]    # the profile; --list for history
oprobe lab   [SCENARIO]    # a local S3 with faults
```

`oprobe COMMAND --help` prints the same flags as this page; a test keeps the two in sync.

## Global flags

| Flag | Meaning |
|---|---|
| `-o, --output text\|json` | the result on stdout as text (default) or one JSON document (env `OPROBE_OUTPUT`); logs and progress always go to stderr |
| `--color auto\|always\|never` | colors (default `auto`; `NO_COLOR` is honoured) |
| `-q, --quiet` | no progress or informational messages on stderr |
| `--non-interactive` | never prompt or redraw (implied when stdin or stdout is not a terminal) |
| `-h, --help` | help (`-h` for a summary) |
| `-V, --version` | the version |

## `oprobe run OPERATOR…`

Profile one or more operators on the same workload. Several operators run one after another on
the same workload and form one comparison. Prints the brief: the verdict, at most three findings
with their remedy, and the next command. The run is saved; `-o json` prints the report
(`schemas/report.json`).

`OPERATOR` is a URL (`s3://bucket/prefix`, `lab://demo`, `fs:///tmp/x`, `memory://`), an
operator file ([`op.toml`](operator.md)) or a name from the config file's `[operators.NAME]`.

**Workload** ([concepts](concepts.md#workload))

| Flag | Meaning |
|---|---|
| `--preset NAME` | `small-files` (default), `large-objects`, `range-reads`, `list-stat` |
| `-c, --concurrency N` | concurrent operations |
| `-d, --duration DURATION` | the measurement window (`30s`, `2m`) |
| `--objects N` | dataset objects to generate |
| `--object-size SIZE` | size of each generated object (`16MiB`) |
| `--rate OPS` | open-loop arrival rate (operations per second); latency counts from the intended start |
| `--existing PREFIX` | profile the objects already under `PREFIX`; writes nothing (implies `--read-only`) |
| `--seed N` | seed of the workload's random choices |
| `--set PATH=VALUE` | change one value of the operator or the workload, repeatable (`--set layers.retry.max_times=5`, `--set read.gap=1MiB`, `--set layers.cache=off`; [paths](operator.md#--set)) |

**Safety**

| Flag | Meaning |
|---|---|
| `--dry-run` | print the plan (requests, bytes, objects written and deleted, ETA) and exit |
| `--read-only` | refuse every write and delete (exit 5) |
| `--max-requests N` | stop after this many backend requests (exit 4) |
| `--max-bytes SIZE` | stop after transferring this many bytes, e.g. `10GiB` (exit 4) |
| `--max-duration DURATION` | stop after this much wall time, e.g. `5m` (exit 4) |

A budget that stops a run saves the partial run and cleans up. Budgets cover the whole command:
every operator of the run. `[defaults.budgets]` in the config file sets defaults.

```bash
oprobe run s3://bucket/oprobe
oprobe run op.toml --preset range-reads -c 16 -d 1m
oprobe run s3://a/x r2://b/x --set layers.concurrent_limit.http=32
oprobe run prod --existing data/2026/ --max-requests 50000
```

## `oprobe demo`

`oprobe run lab://demo`, with the same workload and safety flags as `run`. The lab throttles
above 24 requests in flight and turns some requests into stragglers.

## `oprobe prove RUN [FINDING]`

A/B-test a change of a run's operator. The change is the remedy of `FINDING` (default: the top
finding), or `--with`. When the remedy has a size, short trials pick it first. Then the current
operator and the changed one run interleaved until a sequential test decides on the objective, or
the budget ends. Prints the proven layer diff (Rust and TOML) with its evidence; `-o json` prints
the prove result (`schemas/prove.json`). Exit 3 when the change is worse on the objective.

`RUN` is a run id, an id prefix, `last`, `last~N` or a label. `FINDING` is a finding code
(`throttling`, `tail_stragglers`, …).

| Flag | Meaning |
|---|---|
| `--with PATH=VALUE` | the change to prove instead of the finding's remedy, repeatable |
| `--objective goodput\|p99` | what the change is judged on: successful operations (or bytes) per second (default), or the worst per-operation p99 latency |
| `--max-throttle RATIO` | share of throttled requests a change may cause, e.g. `0.1%` or `0.001` |
| `--budget DURATION` | wall-time budget of the whole proof, e.g. `5m` |

Every trial run is saved as a child of the prove run and hidden from `show --list`.

```bash
oprobe prove last throttling
oprobe prove last --with layers.tail_cut=on --objective p99
oprobe prove 20261122-0915 small_ranges --budget 10m
```

## `oprobe show [RUN|FILE]`

Print a profile: a stored run, or a file (`.oprobe` from `run` or from
[`oprobe-trace`](embedding.md), or a report `.json`). Without an argument, the last run.
`-o json` prints the report (`schemas/report.json`); `--list -o json` the run list
(`schemas/run-list.json`).

A trace from `oprobe-trace` is recognised by its content, whatever its name. Its report has
`run.source = "trace"` and shows `production trace` where a run shows its workload. Each
request counts as one operation, and only the findings that requests support are derived:
errors, throttling, hung requests, stragglers and cold connections. `prove` refuses a trace
(exit 2): it needs a run oprobe drove.

| Flag | Meaning |
|---|---|
| `--list` | list the stored runs (prove trials hidden) |
| `--all` | with `--list`: include the prove trials |
| `--rm RUN…` | delete stored runs |
| `--detail` | every section of the profile |
| `--by stage\|api\|conn` | group the requests by stage, storage API or connection |

```bash
oprobe show --list
oprobe show last --by stage
oprobe show prod.oprobe --detail
oprobe show --rm last~3 last~4
```

## `oprobe lab [SCENARIO]`

Serve a local S3-compatible lab with faults until Ctrl-C and print its endpoint. Point any S3
client at it, or use `lab://SCENARIO` as an operator to start one inside a run.

Scenarios: `clean`, `demo`, `throttle`, `stragglers`, `lossy`, `flaky`, `parquet`, `ratelimit`,
`hotkey`, `bandwidth`, `queueing`, `timevarying`, `noisy`.

| Flag | Meaning |
|---|---|
| `--bind ADDR` | the address to listen on (default `127.0.0.1:9000`) |

## Run references

`last` (the newest run), `last~N` (N runs before it), a full id (`20261122-091512-ab12`), an
unambiguous id prefix, or a label. `show` also takes a path to a file.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | runtime failure (auth, network, storage, internal) |
| 2 | usage or configuration error |
| 3 | `prove` refuted the change (worse on the objective) |
| 4 | a budget stopped the command |
| 5 | a safety rail refused the command |
| 130 | interrupted (Ctrl-C) |

Errors carry a stable code (`-o json`: `{"type":"error","error":{"code":…}}`, schema
`schemas/error.json`); [troubleshooting](troubleshooting.md#error-codes) lists them.

## Environment

| Variable | Meaning |
|---|---|
| `OPROBE_OUTPUT` | default `-o` |
| `OPROBE_CONFIG` | the config file |
| `OPROBE_DATA_DIR` | where runs are stored (default `<data dir>/oprobe/`) |
| `NO_COLOR` | no colors |
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` | the egress proxy, unless `http.proxy` is set |
