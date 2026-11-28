# CLI reference

oprobe has four commands and an alias. Without a command it opens [the app](app.md).

```text
oprobe                     # the app (a native window, or the terminal without a display)
oprobe run   OPERATOR…     # profile; several operators = side by side
oprobe demo                # = oprobe run lab://demo
oprobe prove RUN [FINDING] # A/B test → the proven layer diff
oprobe show  [RUN|FILE]…   # the profile (several = side by side); --list for history
oprobe lab   [SCENARIO]    # a local S3 with faults
```

`oprobe COMMAND --help` prints the same flags as this page; a test keeps the two in sync.

## Global flags

| Flag | Meaning |
|---|---|
| `-o, --output text\|json` | the result on stdout as text (default) or one JSON document (env `OPROBE_OUTPUT`); logs and progress always go to stderr |
| `--color auto\|always\|never` | colors (default: `[defaults] color` in the config file, else `auto`; `NO_COLOR` is honoured) |
| `-q, --quiet` | no progress or informational messages on stderr |
| `--non-interactive` | never prompt or redraw (implied when stdin or stdout is not a terminal) |
| `-h, --help` | help (`-h` for a summary) |
| `-V, --version` | the version |

## `oprobe run OPERATOR…`

Profile one or more operators on the same workload. Several operators run one after another on
the same workload and form one comparison. Prints the brief: the verdict, at most three findings
with their remedy, and the next command. The run is saved; `-o json` prints the runs, each
with its report, and the comparison when there are several (`schemas/run.json`; the report alone
is `schemas/report.json`).

`OPERATOR` is a URL (`s3://bucket/prefix`, `lab://demo`, `fs:///tmp/x`, `memory://`) or an
operator file ([`op.toml`](operator.md), the recommended way to describe your operator). A name
from the config file's `[operators.NAME]` also works.

**Workload** ([concepts](concepts.md#workload))

| Flag | Meaning |
|---|---|
| `--preset NAME` | `small-files` (default), `large-objects`, `range-reads`, `list-stat` |
| `-c, --concurrency N` | concurrent operations |
| `-d, --duration DURATION` | the measurement window (`30s`, `2m`) |
| `--objects N` | dataset objects to generate |
| `--object-size SIZE` | size of each generated object (`16MiB`) |
| `--rate OPS` | open-loop arrival rate (operations per second, 0.001 to 1 000 000); latency counts from the intended start |
| `--existing PREFIX` | profile the objects already under `PREFIX`; writes nothing (implies `--read-only`) |
| `--seed N` | seed of the workload's random choices |
| `--set PATH=VALUE` | change one value of the operator or the workload, repeatable (`--set layers.retry.max_times=5`, `--set read.gap=1MiB`, `--set layers.cache=off`; [paths](operator.md#--set)) |

**Safety**

| Flag | Meaning |
|---|---|
| `--dry-run` | print the plan (requests, bytes, objects written and deleted, ETA, budgets; a warning when it would spend one) and exit (`-o json`: `schemas/plan.json`) |
| `--read-only` | refuse every write and delete (exit 5) |
| `--max-requests N` | send at most this many HTTP requests to the target (calls, for `fs://` and `memory://`), retries and the dataset upload included; the next one is not sent (exit 4) |
| `--max-bytes SIZE` | transfer at most this many body bytes, up and down, e.g. `10GiB`; a request body that does not fit is not sent, a download is cut where it crosses the budget, and a dataset larger than the budget is refused before it is uploaded (exit 4) |
| `--max-duration DURATION` | stop after this much wall time, e.g. `5m` (exit 4) |

A budget that stops a run cuts what is in flight, aborts the multipart uploads it interrupted,
saves the partial run and cleans up. Budgets cover the whole command: every operator of the
run, and every phase but cleanup (see [concepts](concepts.md)). `[defaults.budgets]` in the
config file sets defaults. Ctrl-C, SIGTERM and SIGHUP stop a run the same way (exit 130).

```bash
oprobe run s3://bucket/oprobe
oprobe run op.toml --preset range-reads -c 16 -d 1m
oprobe run s3://a/x 'r2://b/x?account_id=ACCOUNT_ID' --set layers.concurrent_limit.http=32
oprobe run prod --existing data/2026/ --max-requests 50000
```

## `oprobe demo`

`oprobe run lab://demo`, with the same workload and safety flags as `run`. Every 40th request
of the lab is a straggler that waits 250 ms, and it throttles above 40 requests in flight. Its default
workload is small files (64 × 64 KiB) with 16 workers for 8 s; `oprobe prove last` then proves
the top finding's remedy in about a minute.

## `oprobe prove RUN [FINDING]`

A/B-test a change of a run's operator. The change is the remedy of `FINDING` (default: the top
finding), or `--with`. When the remedy has a size, short trials pick it first. Then the current
operator and the changed one run interleaved until a sequential test decides on the objective, or
the budget ends. Prints the proven layer diff (Rust and TOML) with its evidence; `-o json` prints
the prove result (`schemas/prove.json`). Exit 3 when the change is worse on the objective.

The proof is made of **trials**: short runs of the current operator (A) and the changed one (B),
plus the sizing trials. It starts by printing how many trials it may take and the ETA of the
whole proof; `--dry-run` prints the same estimate (and the plan of one trial) and sends nothing.

A verdict other than `inconclusive` needs at least **3 pairs** of trials (one A and one B). The
orders are balanced in blocks of two pairs, one `AB` and one `BA`, and which comes first in a
block is drawn from the run's seed (`notes` says so). When the
budget allows fewer, the verdict is `inconclusive` (exit 0) with a note. When no pair ran at all,
the text says `not measured` and prints no diff; `-o json` still prints a valid result
(`verdict: "inconclusive"`, `pairs_count: 0`, a note). When the changed operator's trials
throttle more than `--max-throttle` allows, and more than the current operator's by more than
that allowance (beyond chance), the verdict is `inconclusive` too, with a note: the service's
limit, not the change, set what was measured. Throttling the change did not cause passes: a
service that answers 503 whatever the operator does (`lab://noisy`) throttles both alike.

Sizing follows the same bound. A remedy for throttling (`throttling`, `rate_limit`) is sized
among the values whose trials stay within `--max-throttle`; any other remedy among the values
that add no more than `--max-throttle` to what the run already had. When no value qualifies,
the least throttled one is tested and a note says so ("no value met --max-throttle"); the rule
above still decides the verdict.

The same holds for failures, on either objective: a changed operator whose operations fail more
than the run allows (1 %), and more than the current operator's, is `inconclusive` with a note
(a timeout that fails the slow operations lowers the p99 and improves nothing). A pair in which
a trial has no objective to compare (no successful operation, or nothing measured) is not
judged: the proof stops there and a note says so.

When the proof stops early because more pairs would not help, a note names the smallest change
its planned pairs could have shown at the noise it measured. On `--objective p99`, when the
latencies around a trial's p99 differ by more than 2×, a note says `p99 sits on a cliff: lengthen
the trial`: pass a longer `--trial`, so that each trial has enough operations to place its p99.

`prove` refuses a run that cannot be judged on performance (its first finding is `invalid_run`:
it failed, or it is partial and too short); profile the operator again first.

The verdict is whether the objective's interval excludes no change. The interval is on the
paired log ratios of the objective (B over A, pair by pair):

- Its estimate is the mean of the `AB` pairs' mean and the `BA` pairs' mean, so an effect of the
  order itself (the second trial of a pair running better or worse) cancels.
- Its spread is no smaller than the noise floor: the spread a pair's ratio would have from the
  trial-to-trial spread of A and of B alone, each learned from that side's consecutive trials
  (until B has two trials, it is assumed as noisy as A: the A/A spread; a floor learned from a
  single A/A difference is doubled).
- It is sequential: `prove` looks after every pair from the third on, and the interval of each
  look is as wide as the plan of `--pairs` looks requires, so that the verdict is wrong about a
  change that is none in fewer than 5 % of proofs, whenever the proof stops. A plan of more
  pairs therefore needs clearer evidence at its early looks. `pairs_max_count` in the result is
  that plan.

Shares, such as throttled requests, are compared with Newcombe's interval; the secondary
metrics' intervals are plain 95 % intervals on each metric's own trial-to-trial spread, shown
and never decisive. Besides the other latency metric, the throttled share and the error share,
they show what the change costs: `requests_per_op` (a tail cut or a hedge sends more) and
`backend_bytes_per_logical_byte` (a cache that fetches whole objects for small ranges moves
more). After an early stop the estimate overstates the change on average (the
proof stopped because it was large); the interval's coverage holds.

The ETA printed first is learned from the run being proved: what it took around its warm-up and
window, how long its last operations kept retrying after the window, and a cool-down per
throttled trial. The first pair corrects it when it was off. Under `--max-duration` the line
says how many trials the budget is estimated to fit; the proof runs every trial that really
fits.

A trial that ends throttled (429/503) is followed by a cool-down before the next trial starts:
the longest `Retry-After` it saw, at least twice the trial warm-up (250 ms to 2 s), at most 5 s.
The cool-down counts in the budget, Ctrl-C interrupts it, and `notes` says how often it
happened.

`--max-duration` is the one wall-time budget of the whole proof: no trial starts that would not
end within it, and no cool-down is waited past it (a pair whose second trial no longer fits
after the first one's cool-down is left incomplete and out of the result). When a budget (`--max-duration`, `--max-requests`, `--max-bytes`) stops the prove
part-way, it still prints and saves the partial result (`budget_exceeded: true`, usually
`inconclusive`) and exits 4; its trials stay attached, so `oprobe show --rm PROVE` removes them.

`RUN` is a run id, an id prefix, `last`, `last~N` or a run's `.oprobe` file. A production trace
has no workload to replay: `prove` refuses it and prints the `oprobe run` command for the service
it recorded. `FINDING` is a finding code (`throttling`, `tail_stragglers`, …); a finding whose
remedy is not a layer (`key_limit`, `client_bound`, a bandwidth cap on reads) has nothing to prove,
and `prove` refuses it with where to look instead. A finding's `next` command names the objective
its remedy is proved on: `--objective p99` for `throttling` and `rate_limit` (a cap takes the
throttled operations' backoffs out of the tail; it cannot raise goodput while spare workers keep
the service at its limit). The objective's change prints with its 95 % interval
([glossary](concepts.md#glossary)).

| Flag | Meaning |
|---|---|
| `--with PATH=VALUE` | the change to prove instead of the finding's remedy, repeatable |
| `--objective goodput\|p99` | what the change is judged on: successful operations (or bytes) per second (default), or the worst per-operation p99 latency |
| `--max-throttle RATIO` | share of throttled requests a change may cause, e.g. `0.1%` or `0.001` |
| `--trial DURATION` | measurement window of one trial (default `2s`, at most the run's own window) |
| `--pairs N` | A/B pairs at most, 3 to 12 (default 6); a value outside that range is a usage error (exit 2) |
| `--max-duration DURATION` | wall-time budget of the whole proof, e.g. `5m` |
| `--save FILE\|OPERATOR` | when the verdict is `improved`, write the proven change to an `op.toml` file or to `[operators.OPERATOR]` in the config file. A new file gets the operator with its `${ENV}` references and without secret values; the run's own `op.toml` (or another of the same service) is patched in place; the config file's path saves to the run's `[operators.NAME]`. Any other existing file is refused before a trial runs. The proof is saved first: a save that fails is a note |
| `--dry-run` | print the number of trials and the ETA of the whole proof, and exit (`-o json`: `schemas/prove-plan.json`) |
| `--read-only` | refuse every write and delete (exit 5) |
| `--max-requests N`, `--max-bytes SIZE` | send at most this many HTTP requests or body bytes, over every trial (exit 4) |

Every trial is saved as a run, a child of the prove, and hidden from `show --list`. A prove
stopped before its result is saved (Ctrl-C) deletes its trials.

```bash
oprobe prove last throttling
oprobe prove last throttling --save op.toml
oprobe prove last --with layers.tail_cut=on --objective p99
oprobe prove 20261122-0915 small_ranges --max-duration 10m
```

## `oprobe show [RUN|FILE]…`

Print a profile: a stored run, or a file (`.oprobe` from `run` or from
[`oprobe-trace`](embedding.md), or a report `.json`). Without an argument, the last run.
Pass two or more runs or files to compare them side by side on goodput, bandwidth, latency,
retries, throttling and stage split (`-o json`: the `run` document, `schemas/run.json`, `--detail` adds the per-API
comparison table). `-o json` for one run prints the shown run (`schemas/show.json`: the report of `schemas/report.json`, where it is stored, and the `by` and `range` sections when asked for); `--list -o json` the run list
(`schemas/run-list.json`; `-n` limits the runs and the proves alike), and `--rm -o json` the deleted ids, a prove's trial runs after it (`schemas/removed.json`). A prove id, unambiguous prove prefix, `last-prove`, or `.prove.json` file (`show --list`
lists them) prints that prove's stored result as `prove` printed it (`schemas/prove.json`), also
after its run was removed.

A trace from `oprobe-trace` is recognised by its content, whatever its name. Its report has
`run.source = "trace"` and shows `production trace` where a run shows its workload. Its
operations are the application's OpenDAL calls (keys hashed), and the findings a trace supports
are derived: errors, throttling, hung requests, stragglers, cold connections, small ranges, slow
multipart and repeated reads (not bandwidth cap or client bound). `prove` refuses a trace
(exit 2): it needs a run oprobe drove.

| Flag | Meaning |
|---|---|
| `--list` | list the stored runs (prove trials hidden) and the proves; a long change is cut in the list (`show PROVE` prints it whole) |
| `--all` | with `--list`: include the prove trials |
| `-n`, `--limit N` | with `--list`: at most N runs and N proves, newest first (default 20) |
| `--rm RUN…` | delete stored runs (a run named twice is deleted once) |
| `--detail` | every section of the profile: the timeline (one row per interval of 1 s, or 10 s after ten minutes; long runs merge rows), the spread of the requests over keys, sizes, prefixes and remotes, the operator and the five slowest operations |
| `--by stage\|api\|conn` | group the requests by stage, storage API or connection |
| `--since DURATION` | only the time range from this far into the window: its goodput, latency quantiles, failures, requests, retries and throttling, where the time went, the verbs, the requests by API and the slowest operations, recomputed from the trace rows (`-o json`: the `range` object). With `--by`, only the range's requests are grouped. In an open-loop run (`--rate`), the arrivals due in the range that never started are counted (`missed_arrivals_count`) and count in its percentiles, each at the least it waited, as in the whole run: the percentiles are then "at least" values, printed with `≥` (`latency_at_least`) |
| `--until DURATION` | only the time range up to this far into the window (default: its end) |

An operation counts in the range it started in, a request in the range it was sent in. When the
run kept only a share of its rows, the range says so: its counts are estimates and its quantiles
weighted by the operations each row stands for.

```bash
oprobe show --list
oprobe show last --since 3s --until 8s
oprobe show last --by stage
oprobe show prod.oprobe --detail
oprobe show --rm last~3 last~4
```

## `oprobe lab [SCENARIO]`

Serve a local S3-compatible lab with faults until Ctrl-C and print its endpoint (`-o json`:
`schemas/lab.json`). Point any S3 client at it, or use `lab://SCENARIO` as an operator to start one
inside a run.

Scenarios (the same six as the app's): `demo`, `throttle`, `stragglers`, `bandwidth`, `flaky`,
`ratelimit`. Without one, the lab has no faults.

| Flag | Meaning |
|---|---|
| `--bind ADDR` | the address to listen on (default `127.0.0.1:9000`) |
| `--allow-remote` | allow binding to a non-loopback address (`0.0.0.0`, LAN IP); refused without this flag because the lab accepts static lab credentials without authentication |
| `--max-store-size SIZE` | (hidden from `--help`) cap total in-memory object bytes stored by the lab (default `8GiB`; returns `507 InsufficientStorage` above it) |

## Run references

`last` (the newest run), `last~N` (N runs before it), a full id (`20261122-091512-ab12`) or an
unambiguous id prefix. `show` also takes a path to a file.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | runtime failure (auth, network, storage, internal) |
| 2 | usage or configuration error |
| 3 | `prove`: the change is worse on the objective |
| 4 | a budget stopped the command |
| 5 | a safety rail refused the command |
| 130 | interrupted (Ctrl-C, SIGTERM, SIGHUP) |

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
