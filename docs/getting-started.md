# Getting started

Ten minutes from install to a proven change: profile the local lab, explain what it finds, prove
the fix, then do the same on your own storage. No account is needed until the last step.

## 1. Install

```bash
curl -fsSL https://github.com/hxddh/oprobe/releases/latest/download/install.sh | sh
# or: brew install hxddh/tap/oprobe · cargo install oprobe --locked · Windows: docs/install.md
```

## 2. Profile the lab

```bash
oprobe demo
```

`demo` is `oprobe run lab://demo`. The lab is an S3-compatible server inside oprobe on 127.0.0.1:
it throttles (503 SlowDown) above 24 requests in flight and turns some requests into stragglers.
The default workload writes small objects, then reads them back with 32 workers for a few seconds.

The brief that `run` prints has three parts:

- **the verdict**: what bounds this operator on this workload (throttling, the server, the
  network, your client);
- **at most three findings**, each with its cause, its evidence (counts and the requests
  themselves) and its remedy as an `op.toml` path, such as `layers.concurrent_limit.http`;
- **the next command**, usually `oprobe prove last FINDING`.

The run is saved. `oprobe show last` prints the whole profile, `oprobe show last --by stage`
groups the requests by stage, and `oprobe show --list` lists the history.

## 3. Prove the remedy

```bash
oprobe prove last throttling
```

`prove` takes the run's operator (A) and the same operator with the finding's remedy (B). For a
remedy with a size, such as a concurrency limit, it first runs short trials over that one value
and picks the knee of the objective. Then it runs A and B interleaved (A B A B …) until a
sequential test against the noise floor decides, or the budget runs out. The verdict is
`improved`, `worse` (exit 3) or `inconclusive`, on goodput unless you pass `--objective p99`:

```text
proved on goodput: +31 % [+27, +35], p99 −42 %, 0 throttled (4 A/B pairs)
+ ConcurrentLimitLayer::new(24)
~ RetryLayer::new().with_jitter().with_max_times(4)
```

The diff is Rust you can paste into your operator, and the same change as `op.toml`. To prove a
change of your own: `oprobe prove last --with layers.tail_cut=on`.

## 4. Open the app

```bash
oprobe
```

The app shows the same runs in four views: Runs, Profile, Operator and Compare. In the Operator
view you can toggle a layer, press *Prove* and watch the A/B pairs arrive. See [the app](app.md).

## 5. Your storage

oprobe uses the credentials your OpenDAL code would use (`AWS_*`, `~/.aws` profiles,
`GOOGLE_APPLICATION_CREDENTIALS`, `AZURE_STORAGE_*`), or the options in `op.toml`.

```bash
oprobe run s3://my-bucket/oprobe --dry-run      # the plan: requests, bytes, objects, ETA
oprobe run s3://my-bucket/oprobe --set options.region=eu-west-1
```

Writes go under `s3://my-bucket/oprobe/oprobe-run-<id>/` and are deleted at the end. To touch
nothing, profile existing objects read-only: `--existing data/2026/`. Budgets stop a run hard:
`--max-requests 100000 --max-bytes 10GiB --max-duration 5m`.

Put your real operator in a file and profile it on the workload that looks like yours:

```bash
cat > op.toml <<'EOF'
url = "s3://my-bucket/oprobe"
[options]
region = "eu-west-1"
[layers]
retry = { max_times = 4, jitter = true }
[read]
chunk = "8MiB"
concurrent = 4
EOF
oprobe run op.toml --preset range-reads
```

The four presets are `small-files`, `large-objects`, `range-reads` and `list-stat`
([concepts](concepts.md#workload)). Several operators run one after another on the same workload
and form one comparison: `oprobe run op.toml r2://bucket/x`.

## 6. Production

To profile the traffic your service already sends, add the [`oprobe-trace`](embedding.md) layer
to its operator and open the `.oprobe` file it saves with `oprobe show FILE` or in the app.

Next: [concepts](concepts.md) · [op.toml](operator.md) · [reading the profile](reading-the-profile.md)
· [CLI](cli.md)
