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
it throttles (503 SlowDown) above 24 requests in flight, and every 40th request is a straggler
that waits 250 ms for its first byte. The default workload writes, reads and stats small objects
with 28 workers for 8 seconds:

```text
[Attention] 2.4% of GetObject requests wait far longer for the server's first byte than their peers
  (stragglers).
  lab://demo · small-files · 8.0 s · run 20261123-063611-3d82
  ▲ 1 tail_stragglers (high confidence) — 2.4% of GetObject requests wait far longer for the
      server's first byte than their peers (stragglers).
      162 stragglers wait ~261ms for the first byte against ~9.78ms for a typical GetObject;
      operation p50 10.0ms · p99 261ms
      remedy: layers.tail_cut + layers.retry.min_delay — re-issue requests far slower than their
        peers, after a short retry backoff (TailCutLayer + RetryLayer::with_min_delay)
      next: oprobe prove 20261123-063611-3d82 tail_stragglers
  ▲ 2 throttling (medium confidence) — 15% of operation time is spent in retry backoff after
      429/503: the service throttled 0.1% of requests.
      16 throttled responses (429/503) in 11416 requests (0.14%); 15% of operation time in retry
      backoff; the rejections start above a number of requests in flight (the in-flight limit
      explains 81% of the throttled requests)
      remedy: layers.concurrent_limit.http + layers.retry.jitter — cap the HTTP requests in flight
        below the service's limit and retry with jitter
        (ConcurrentLimitLayer::with_http_concurrent_limit + RetryLayer::with_jitter)
      next: oprobe prove 20261123-063611-3d82 throttling
  1425.0 ops/s · 80.1 MiB/s · p50 10.0ms · p99 261ms · errors 0.00% · 0.14% of requests throttled
  full profile: oprobe show 20261123-063611-3d82 --detail · terms: https://github.com/hxddh/oprobe/blob/v6.3.0/docs/concepts.md#glossary
```

The brief that `run` prints has three parts:

- **the verdict**: its level (OK, Review, Attention, Critical) and the top finding's cause;
- **at most three findings**, costliest first, each with its cause, its evidence (counts and the
  requests themselves) and its remedy as an `op.toml` path, such as `layers.tail_cut`;
- **the next command**, `oprobe prove RUN FINDING`.

Here the stragglers come first: they hold the lab's request slots for a quarter of a second each.
The throttling costs latency (the throttled operations wait out OpenDAL's one-second retry
backoff) but no goodput, because the other workers keep the lab busy.

The run is saved. `oprobe show last` prints the whole profile, `oprobe show last --by stage`
groups the requests by stage, and `oprobe show --list` lists the history.

## 3. Prove the remedy

```bash
oprobe prove last
```

`prove` takes the run's operator (A) and the same operator with the top finding's remedy (B);
`oprobe prove last throttling` proves another finding's. For a remedy with a size, such as a
concurrency limit, it first runs short sizing trials over that one value and picks the knee of
the objective. Then it runs trials of A and B interleaved (A B A B …) until a sequential test
against the noise floor decides, or the budget (`--max-duration`) runs out. It starts with the
number of trials and the ETA of the whole proof, and prints each trial as it ends (on stderr):

```text
prove tail_stragglers · goodput · run 20261123-063611-3d82 · prove 20261123-063621-f7e6
6–12 trials (3–6 pairs of A and B) · ETA 57s – 1m 53s
· A #1 : 1485.5 ops/s · p99 261ms · 0.27 % throttled
· B #1 : 2078.0 ops/s · p99 82.5ms · 4.40 % throttled
· A #2 : 1503.0 ops/s · p99 261ms · 0.27 % throttled
· B #2 : 2043.0 ops/s · p99 84.3ms · 4.29 % throttled
· A #3 : 1468.5 ops/s · p99 262ms · 0.27 % throttled
· B #3 : 1873.0 ops/s · p99 85.3ms · 4.98 % throttled
```

The verdict is `improved`, `worse` (exit 3) or `inconclusive`, on goodput unless you pass
`--objective p99`, and the result ends with the diff:

```text
improved on goodput: +34.4 %, 95 % interval [+19.5, +51.1] (3 pairs of trials, noise ±2.0 %) · lab://demo (local lab)
secondary (not decisive; 95 % intervals): p99 -67.8 % [-69.4, -66.2] · throttled 0.27 % → 4.54 % · errors 0.00 % → 0.02 %
note: TailCutLayer re-issues through RetryLayer: its first backoff is shortened so a cut request is retried at once; trials warm up 11.5 s because the layer ignores its first 10 s
+ TailCutLayer::builder().safety_factor(1.5).min_samples(32).min_deadline(Duration::from_millis(50)).max_deadline(Duration::from_secs(60)).build()
~ RetryLayer::new().with_min_delay(Duration::from_millis(20))
op.toml:
[layers]
retry = { min_delay = "20ms" }
tail_cut = true
```

The diff is Rust you can paste into your operator, and the same change as `op.toml`. TailCutLayer
ignores its first 10 seconds, so its candidate trials warm up for 11.5 s: this proof took a
minute. To prove a change of your own: `oprobe prove last --with layers.retry.jitter=on`.

## 4. Open the app

```bash
oprobe
```

The app shows the same runs in a sidebar. A run opens in Profile or Operator, and two or more selected runs open in Compare. In the Operator
view you can toggle a layer, press *Prove* and watch the trials arrive. See [the app](app.md).

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
