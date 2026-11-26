# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## v9.3.1 — 2026-11-26

### Removed

- **Homebrew, Scoop, winget and crates.io for the binary.** None of them was ever published, and
  their jobs, manifests and documentation are gone. The binary ships as GitHub release assets
  (with `install.sh` and `install.ps1`) and installs from source with
  `cargo install --locked --git https://github.com/hxddh/oprobe --tag vX.Y.Z oprobe`.
  `oprobe-trace` remains the one crate published to crates.io, when the release has a token.

## v9.3.0 — 2026-11-26

9.3 is a truth-and-stop release. A deep review of 9.2 (`docs/design/v9.2-review.md`: a bug hunt on
the binary and two code-review passes) found that the weakest part of the loop was Explain:
several findings said the wrong thing on ordinary runs, a proof could be improved by failing, and
a few stop paths still left data behind. 9.3 fixes those. It adds no command and no flag. It
no longer promises to read what an older version wrote (see Removed); no format version changed.

### Fixed

#### Stopping

- **A closed stderr no longer costs the cleanup.** A terminal or SSH session that goes away, or
  `oprobe run … 2>&1 | head`, made the next progress line panic (`eprintln!`): exit 101, no
  cleanup, no saved run, the test data left on the target, also after the SIGHUP 9.2 handles.
  Nothing oprobe prints for stderr can fail now, and the first line a closed stderr refuses stops
  the command as a signal does: the requests in flight are cut, the multipart uploads aborted, the
  test data deleted, the partial run saved, exit 130.
- **Nothing is left when a run stops while it opens.** The preflight's probe object is deleted
  outside the budgets and the cancellation, also when its write failed: with `--max-requests 3`,
  or Ctrl-C between its write and its delete, 9.2 left `oprobe-run-<id>/.oprobe-preflight`. A run
  whose file cannot be created now removes the dataset it had uploaded. A run stopped while it
  opens fails as `run.interrupted` (exit 130) instead of "preflight: cannot list the target" with
  a hint to check the credentials.
- **Stopping is bounded.** A `CreateMultipartUpload` in flight when the run is stopped has 10 s to
  answer, and a stopped run waits at most 45 s for its uploads to abort before it cleans up; 9.2
  waited for both without a bound, so a stalled service hung the run after Ctrl-C. An upload that
  could not be aborted is named in the cleanup error.
- **A hint or a replay never retargets a run.** After a run that cannot be judged, `prove` hints
  the run's whole command (9.2: the operator alone, which sent a run on a local endpoint to the
  real service with ambient credentials). Run Again keeps the operator's values when only the
  workload changes in New Run (9.2 dropped them with the preset), and "Runs again as" shows the
  command the form will start. `skip_signature=true` is no longer taken for a signature and
  dropped from the stored operator and the replay command.
- **Budgets say what happened.** A request body that does not fit in what is left of `--max-bytes`
  is refused with "the next request (8.0 MiB) does not fit the byte budget: 1 B of 4.0 MiB used".
  Once the byte budget is spent exactly, a request without a body is refused too. An interrupted
  open loop no longer counts the operations a stop cut as issued.
- **Quitting the app waits, shows it, and says what it left.** Stop and Quit used to quit at once
  and then freeze for up to 30 s. The app now waits up to 60 s in a dialog that shows what each
  job is doing, and past that names the job that was left and its `oprobe-run-…/` prefix.

#### Findings

- **Failures are named for what they are.** A run whose writes OpenDAL's `ThrottleLayer` refused
  (a `layers.throttle.burst` below the write size) has an `errors` finding that names the burst;
  9.2 had none and called the client the bound. The `errors` remedy stays the retry backoff when a
  few operations ran out of retries on the same transient failures the retries recovered. A
  handful of 429/503 is `throttling` at low confidence, never "random" `errors`. A 5xx whose
  response carries `access-control-allow-credentials`, or a failure on a key named
  `credentials.json`, is no longer `auth.no_credentials`.
- **A body that stalls is a hung request.** The stage was called `transfer` in one place and
  `body` in another, so body-stalled requests were never counted; it is `body` everywhere now
  (`--by stage` says so). A large download, downloads under a
  `bandwidth_cap`, and a slow upload through a kept HTTP client are not hung.
- **An open loop that fell behind is judged on evidence.** It no longer says "raise `-c`" unless
  the service answered as fast with every worker in flight; a steady overload says that one run
  cannot tell too few workers from a service at its capacity. An open loop in which more than 1 %
  of the scheduled arrivals never completed is stalled: the headline and the checks say so, with
  the run's own p99 as the lower bound, and no limit is needed. The p99 limit of 9.2, which no
  flag could set, is gone, and so are the docs that told you to set it.
- **The queueing form of `throttling` is advice.** It is fitted with one service time per storage
  API, judged against the client's own scheduling lag with the wait the model predicts (9.2: a
  1 ms mean that was biased by noise), and never made on a client-bound run. A cap moves the queue
  and shortens nothing, so its `next` shows the evidence and `prove` refuses it.
- **`repeated_reads` offers no proof `prove` would refuse.** When a whole-object cache does not
  fit the reads (ranges of large objects, or an object above 1 GiB), the remedy is advice and
  says why; the finding and `prove` share one sizing rule.
- **A healthy run's headline** says the load was not varied and how to vary it
  (`--set shape=ramp:…`) instead of "Inconclusive … prove sizes a change" above a `prove` that
  refused the run. The tail split counts an operation's own queue time. A default `large-objects`
  run no longer warns about a `chunk_size 0B` or reports a part size it did not write.

#### Prove

- **A proof is not improved by failing.** A changed operator whose operations fail more than the
  run allows, and more than the current operator's beyond chance, is `inconclusive` with a note,
  on either objective: the throttle gate's twin. 9.2 called `layers.timeout.io=200ms` "improved"
  on p99 for failing the slow operations. A pair in which a trial has no objective at all is no
  longer clamped into a ratio ("improved by 100 %"): it is not judged.
- **The ETA is learned, and corrected.** The estimate comes from the run being proved (what it
  took around its window, how long its last operations kept retrying, a cool-down per throttled
  trial), the first trial of each arm corrects it, and under `--max-duration` the plan says what
  the budget is *estimated* to fit. 9.2 printed "23–45 s" for a proof of 54 s, and "too few
  pairs to decide" before a proof that decided.
- **A cool-down never runs past the budget** (9.2: up to 5 s and a trial over).
- **A futility stop says what it gave up on,** and gives up only on a change the planned pairs
  could not have shown (9.2 looked for twice the noise floor). Secondary metrics use their own
  spread, not the objective's noise floor.
- **A p99 has a resolution.** The spread of the latencies around rank 0.99·n of a trial is a
  floor of the noise, and when they differ by more than 2× a note says "p99 sits on a cliff:
  lengthen the trial" (`trials[].p99_spread_ratio`, optional).
- **A rate limit is sized under the fitted rate:** a cap whose sizing trial sends more requests
  per second than the finding's fit is not chosen, even when its two seconds saw no throttle
  (9.2 chose it and then measured it `worse`).
- `--pairs` outside 3–12 is a usage error (9.2 clamped it silently) and, with `--trial`, is in
  `prove --help`. Notes are redacted where they enter the result. A prove with no pair offers no
  profile of the change; a prove of a run whose secrets were given by value says they were not
  stored.

#### Run files

- **A run file is bounded while it is decoded, and a run never writes one it cannot read.** A
  segment's columns are capped as they are deserialised (65 536 entries a column), so a crafted
  segment repeating `[0,{}],` no longer expands some forty times before the row budget is
  charged; a segment may hold 64 MiB of JSON (was 256 MiB; real ones are 2 to 7 MiB). The writers
  keep to the same limits, and a streamed run stops keeping rows before the file passes what a
  reader takes, with a warning in the report. Failed and throttled operations are kept in full
  for twice `max_requests`, then thinned with their weight recorded: 9.2 kept them all, so a long
  run against a failing target saved a file it then refused.
- **A time range of an open-loop run counts its missed arrivals** (`missed_arrivals_count`), and
  its percentiles are marked "at least" (`≥`, `latency_at_least`).

#### The embedded layer and privacy

- **No request waits for the layer, and the layer survives its runtime.** The lookup of a new
  connection's socket runs beside the requests; 9.2 awaited it at the response headers, so a busy
  blocking pool held cold responses back. A descriptor scan dropped by a runtime that shuts down
  gives its queue back: 9.2 left the finder busy for ever, and an operator shared across runtimes
  hung on its next new connection.
- **Credentials are removed from any text, not only from URLs**: from a query with no URL around
  it, a fragment (`#access_token=…`), a percent-encoded URL in a value and a quoted
  `Authorization` header; a password holding `/`, `?` or `#` is removed whole; `temp_url_sig`,
  `*_sig`, `upload_id` (a GCS resumable session), `jwt` and `hmac` are secret. Listing cursors
  (`continuation-token`, `pageToken`, `start_key`, …) are not.
- **A kept HTTP client is "not observed", not "100 % reused".** With
  `Config::keep_http_transport`, reuse counts only where the connection was observed; the file's
  `service` says `"http_transport": "installed"`; `oprobe show` and the app print
  `dials not observed · reuse not observed`.
- **Retransmits are charged to the upload that retransmitted**, not to the small GET that ended
  beside it. A response OpenDAL cannot use (a `Content-Encoding` that does not parse) is a failed
  request in the trace. OpenTelemetry stage points carry their own operator's labels when one
  layer is cloned onto several operators. Connection ids number a client's connections in dial
  order, so the Connections section counts one set.

#### Operator and the app

- **No cache that fetches huge objects whole**, and `docs/operator.md` documents what
  `FoyerLayer` costs (a whole-object fetch on a ranged read, a `HEAD` per miss, write-through
  buffering) and a known OpenDAL issue: a stale read after a write on a versioned bucket.
  A `layers.throttle.burst` below `write.chunk` is refused.
- **The whole operator as Rust.** The prove result's `diff` carries `operator_rust` (optional),
  code that compiles as written, and "Copy Rust" in the app copies it when no edit is pending.
- **A menu shortcut is no longer lost to a rebuilt menu bar.** The bar was rebuilt on every
  selection change; it is rebuilt only when an item changes.

### Changed

- **Lab defaults sit well inside their limits**, so the cap that fits is the same on every
  machine: `lab://parquet` 6 workers (was 8), `lab://demo` 16 (was 28), `lab://throttle` 64 (was
  48), and `lab://ratelimit` is 100 requests/s at 12 ms (was 300 at 5 ms). The convergence tests
  run every scenario once; 9.2's retried until one run passed.
- **Binary size has no hard target.** It must not cost usability or function; CI still reports it.
- Papercuts: `oprobe lab --bind` on a busy port is a usage error with a hint; an unreachable proxy
  is named instead of the endpoint; an interrupted run says "interrupted", whatever stopped it;
  `show --rm X X` removes X once; `show --list` cuts a long change; `show FILE` of an
  `oprobe run -o json` document says what it is; the `r2://` examples in the docs run as written.

### Removed

- **Compatibility with older versions and their data.** oprobe reads what its own version wrote:
  a prove result without `pairs_max_count` (9.1) is refused as unreadable, and the fallbacks for
  reports before 6.3 are gone. Also removed: the stage name `transfer` (it is `body`), the p99
  limit remnants (`sla.p99_ms` in a config and in the report), the hidden `prove --budget`
  spelling of `--max-duration`, and the 5.x preset names (`parquet`, `small`, `mixed`, …).
  `oprobe-trace`'s API and the CLI and JSON output stay stable across 9.x.

### Known issues

- A steady overloaded open loop cannot tell too few workers from a saturated service: only a run
  that varied its load can.
- A stop that arrives after the window (during verification or cleanup) still marks a complete
  run interrupted.
- A menu shortcut typed across a change that rebuilds the menu bar (a dialog, a run starting, a
  page switch) can still be lost: nsh 0.7 gives menu items new ids on each rebuild.
- The limits OpenDAL sets remain: an HTTP permit wait counts in `timeout.io`, and multipart
  uploads cannot be listed.

## v9.2.0 — 2026-11-26

9.2 is a hardening release on 9.1: what oprobe does to a target, what it reads from an untrusted
file, what `prove` may claim, what the embedded layer does to its host, and what the generated
code is worth when pasted. It adds no command and no flag, and every v9 file of 9.1 still reads
(`oprobe.trace/9`, `oprobe.run/7`, `oprobe.report/7`, `oprobe.prove/9`, `oprobe.events/9`,
`oprobe.operator/6`).

### Fixed

#### Target safety and untrusted input

- **Budgets hold on the wire.** `--max-requests` and `--max-bytes` are enforced request by request
  and chunk by chunk, not when an operation has finished: every HTTP request sent to the target
  before cleanup counts as it is sent (retries, the preflight and the dataset upload included,
  which 9.1 did not count), the request after the budget is never sent, a request body that does
  not fit is not sent, and a download stops at the chunk that crosses the byte budget. A spent
  budget cuts what is in flight instead of waiting for it. 9.1 let one large multipart write or a
  retry storm run far past the budget. Cleanup is never counted and never refused; exit 4 and the
  saved partial run are unchanged.
- **A stopped run leaves no multipart upload.** A failed, timed-out, cancelled or budget-stopped
  write or dataset upload aborts its multipart upload (9.1 dropped the writer, leaving billed
  parts no object listing shows), also when the operation was dropped half-way. Ctrl-C now cuts
  the requests in flight instead of draining them, and SIGTERM and SIGHUP stop a command the same
  way (exit 130) instead of killing it with its test data in place. An upload that cannot be
  aborted is named in the cleanup error.
- **Presigned signatures and tokens never leave.** URLs lose `X-Amz-Signature`,
  `X-Amz-Credential`, `X-Amz-Security-Token`, `Signature`, `AWSAccessKeyId`, an Azure SAS's `sig`,
  `X-Goog-*`, `x-oss-*`, `q-signature`, `access_token`, … (any case) wherever they appear: in the
  errors `oprobe-trace` records and the ones its client returns, in `.oprobe` files, reports,
  error output, warnings and log events. 9.1 removed only `user:password@` from error text and
  quoted the request URL whole in transport errors.
- **A run file is read as untrusted input.** A block's declared length is no longer allocated
  before its bytes arrive (9.1 took up to 1 GiB for a 60-byte file); a file may hold blocks of at
  most 256 MiB, decompress to at most 4 GiB and its rows may take at most 4 GiB of memory, so a
  zstd bomb or a segment that names one long string in every row is refused with
  `usage.invalid_argument` and a hint instead of exhausting memory. Names found in a file are no
  longer leaked for the life of the process (the app opens many files). Every 9.1 run file still
  loads.
- **Large conditional and metadata writes write their size.** The payload pool is sized from
  every writing operation; 9.1 cut these two kinds to about 1 MiB without a word.

#### The operator: generated code, layer order, cache

- **Generated Rust compiles, and never overwrites your credentials.** The whole-operator Rust is
  now a complete `build_operator()` (plus `reader()` / `writer()` for the `[read]` / `[write]`
  options) instead of statements that used an unbound `path` and passed `&str` options to
  `Operator::via_iter`. A secret option is left out with a comment instead of a `"<redacted>"`
  literal that replaced the credential your program loads itself; a `${NAME}` reference is read
  with `std::env::var`. `http.version = auto` with a custom client emits `.http1_only()`, as
  measured. The output for 24 operators is checked in under `crates/oprobe-core/tests/codegen/`
  and compiled against the workspace's OpenDAL, foyer and reqwest on every test build.
- **A sound layer order.** The stack is now `cache → retry → throttle → concurrent_limit →
  tail_cut → timeout → service` (was `cache → retry → tail_cut → timeout → concurrent_limit →
  throttle`): a throttled write no longer waits with an operation permit held or runs into the io
  timeout, and a call that queues for an operation permit is no longer timed out or tail-cut
  while it queues. The measured operator and the generated Rust are built from the same list.
  `layers.tail_cut.max_deadline` defaults to the shorter of `timeout.op` and `timeout.io` (was
  `timeout.op`, a ceiling the 10 s io timeout made unreachable) and never below `min_deadline`
  (OpenDAL panicked on it). The HTTP permit wait stays inside the timers by OpenDAL's design;
  `docs/operator.md` says so.
- **`layers.cache.memory` admits what it is sized for.** The cache was split in 8 shards and
  admitted only objects up to an eighth of the budget, so a cache `prove` sized for large
  objects could hold none of them, and OpenDAL fetched such an object whole on every read. The
  cache now has one shard and admits any object no larger than `memory`; `FoyerLayer` gets the
  same size limit, so larger objects are read through; `prove` sizes the budget with the same
  rule and never below the largest object the run read. The generated Rust builds the same
  cache.

#### Prove and the service-level check

- **A proof's verdict holds its 5 %.** The A/B pairs now run in a balanced order (blocks of one
  `AB` and one `BA`, drawn from the run's seed), the estimate cancels an effect of the order
  itself, and the verdict, the interval and the convergence chart share one sequential boundary
  calibrated for the planned pairs: whenever a proof stops, it calls a change that is none in
  fewer than 5 % of proofs. 9.1 drew each pair's order alone and let an unadjusted interval
  decide: when the second trial of a pair ran better or worse by several times the noise, up
  to one proof in four called that a change. Early looks now need clearer evidence, so a proof
  may take one more pair. The result names its plan (`pairs_max_count`, optional;
  `oprobe.prove/9` files of 9.1 still read).
- **A throttled trial no longer poisons the next.** After a trial that ended throttled, `prove`
  cools down (the longest `Retry-After` it saw, at least twice the trial warm-up, at most 5 s)
  before the next trial; the wait counts in `--max-duration` and is noted once.
- **`lab://throttle` defaults to 48 workers** (was 32). A burst rejected 24 of 32 workers, about
  1 % of a trial's operations: on a fast machine the baseline's p99 fell on either side of the
  backoff from one trial to the next, and the default prove could not decide. A burst now
  rejects 40, above 1 % at any speed the lab serves.
- **A p99 limit cannot be passed by a stalled open loop.** Arrivals of a `--rate` run that never
  completed have no latency, only the time they had waited; the limit is now checked against the
  worst they allow: breached when more than 1 % of the scheduled arrivals never completed ("p99
  is at least …"), and otherwise judged with each of them counted as slower than every completed
  operation. 9.1 checked completed operations only.

#### The trace layer

- **`oprobe-trace` charges a connection to the request that dialed it.** On a shared HTTP/2
  connection the DNS, TCP and TLS time went to whichever request was answered first; it now goes
  to the request that started the dial, and a request that waited for another's dial shows the
  wait as queue time. A dial its request did not use is a connection nobody waited for: counted
  once, charged to no request.
- **TCP retransmits are charged exactly once.** The retransmits of a connection shared by
  concurrent HTTP/2 streams are split among the requests in flight when the kernel counter is
  read; `detail.retrans_count` adds up to the connection's count (9.1 charged them to whichever
  request ended next and could count some twice).
- **No descriptor scans on runtime threads.** The socket of a new connection is found on the
  blocking pool, one scan for every connection dialed meanwhile, and the connection is handed
  over without waiting for it; 9.1 scanned `/proc/self/fd` on a Tokio worker for every
  connection, inside the dial.
- **`http.client.request.duration` is one observation per HTTP request** with standard attributes
  only (`http.request.method`, `server.address`, `server.port`, `http.response.status_code`,
  `network.protocol.version`, `error.type`). The stages moved to
  `oprobe.http.client.request.stage.duration` (`oprobe.stage`); 9.1 recorded every request up to
  seven times into the standard metric, and in-process `memory`/`fs` calls with it.

#### Findings and the workbench

- **A saturated uplink is a `bandwidth_cap`.** The shared-cap model is fitted on request bodies
  as on response bodies; slow uploads over one capped link name `layers.throttle.bandwidth`
  instead of reading as `hung_requests`.
- **A service that queues instead of rejecting is found.** When the time to first byte grows
  with the requests in flight, in a run that varied them at least twofold, the run reports
  `throttling` (a warning, `layers.concurrent_limit.http`) with no 429/503, provided the
  requests waited a millisecond or more on average (less is the client's own scheduling).
  9.1 fitted the model and no finding read it.
- **Run Again repeats the run.** The app rebuilt a run from New Run's five fields and lost its
  open-loop rate, dataset, seed, key popularity and operator options. It now replays the run's
  spec; for a real target New Run shows "Runs again as" with the command it keeps, and a field
  changed there changes only that. The command a profile prints names an added layer's values
  (`--set layers.concurrent_limit.http=12`) instead of `layers.NAME=on`, which could fail or mean
  the layer's defaults.
- **A time range sorts its rows once.** Every quantile, the distribution and the slowest
  operations of a range, a verb or an API come from one order (9.1 sorted the rows for each); the
  numbers are the same.

### Changed

- **`oprobe-trace` can keep your HTTP client.** `Config::keep_http_transport(true)` wraps the
  transport installed on the operator (your client, and any HTTP concurrency limit beneath the
  layer) instead of replacing it; requests are then observed from outside (no DNS/TCP/TLS split,
  no connection statistics). The default still installs the instrumented client, and the docs now
  say what that replaces and where the layer must go (`docs/embedding.md`, "Where the layer
  goes"); `format!("{layer:?}")` names which one a layer does.
- **`oprobe-trace` builds with Rust 1.91**, OpenDAL 0.59's own minimum (the workbench still needs
  1.97 for nsh); CI checks it.
- **A smaller terminal build.** The SVG renderer (resvg and its font stack, 30 crates) is built
  for native windows only (`--features gui`); the terminal app draws glyphs. The binary crate
  drops ten dependencies it did not use.
- **Tests.** The app's tests run one at a time under nextest as they do under `cargo test`, wait
  for the opened run before pressing a menu shortcut (a shortcut pressed across the menu's
  rebuild was lost, and the test timed out on a loaded runner, macOS first), and the Ctrl-C test
  waits for the run to measure instead of sleeping. The parquet proof runs six workers, which
  reach the lab's limit of 24 requests in flight without crossing it: with the default eight,
  workers scanning in step crossed it on macOS CI and the proof was inconclusive over
  `--max-throttle` (the test that failed on `main`).

### Known issues

- An HTTP permit of `layers.concurrent_limit.http` is taken inside the transport, so its wait
  counts in `timeout.io` and in the tail-cut deadline (OpenDAL's design).
- OpenDAL cannot list multipart uploads: an upload whose `CreateMultipartUpload` answer was lost
  to a timeout cannot be aborted, and cleanup cannot sweep for leftovers.
- With `keep_http_transport`, records carry no connection identity (reuse reads as 100 %).
- A run's default check (error rate, no p99 limit) still passes a stalled open loop; set a p99
  limit to judge it.
- A steady closed loop never yields the queueing finding: the model needs a run whose load varied.

## v9.1.0 — 2026-11-25

9.1 fixes the bugs found in a deep review of 9.0 (several could lose data or report wrong numbers),
moves the app to nsh 0.7.0, and makes the GUI and the terminal UI show everything a run measured.
It adds no command and no flag. The plan is `docs/design/v9.1-review.md`.

### Fixed

- **Saving never overwrites the wrong file.** `prove --save`, the app's Save and Save as… share one
  entry point. It patches the same service's `op.toml` or `[operators.NAME]` in place, writes a new
  file clean (`${ENV}` references kept, secret values — including ones given as edits — left
  out), and refuses any other existing file, a config file entry of another service included.
  9.0 could overwrite `config.toml` with an operator.
- **A proof is never lost.** `--save` is checked before the trials, and the prove result is stored
  before the save; a failing save is a note. A prove that ends early (Ctrl-C, an error, a closed
  app) deletes its trial runs; Ctrl-C between trials now exits 130 as interrupted.
- **`layers.cache.memory` is bytes.** Entries are weighed by their size (9.0 counted entries).
- **503 ServiceUnavailable counts as throttling**, in the trace layer and in findings.
- **Embedded traces have a full timeline**: new connections and requests in flight per interval
  (always 0 in 9.0 without the CLI's probe).
- **Ranges and listings tell the truth.** A trace without rows refuses `--since/--until/--by`
  instead of inventing ranges from slow-operation rows; a range counts failures when they started;
  list pages count successful answers only; `show --rm` hints never name an unrelated run.
- **Clear errors for files that are not runs**, for zstd damage and for JSON that is another
  oprobe output; `run.interrupted` has its own docs link and retryability.
- **The capacity bracket** needs three arrivals at a level; one arrival never ends it.
- **Open-loop runs** stamp operations when they start, so a backlog no longer piles every operation
  into the first second; the lateness stays in the latency as queue time.
- **`next` proves what it can.** It carries the objective (`--objective p99` for throttling and
  rate limits) and the run's own workload; findings whose remedy is not a layer point at
  `show RUN --detail`, and `prove` refuses them with a hint.

### Changed

- **Prove method.** The noise floor estimates each side's variance from its own trials
  (σA² + σB²), trial order within a pair is drawn from the run's seed, and the sizing search
  climbs and bisects correctly. `lab://throttle` defaults to 32 workers so its default prove
  decides.
- **The app runs on nsh 0.7.0**: window model v2, quit when the last window closes, reopen,
  "Copied" only when the clipboard took it, accessibility hints and busy states on Run, Prove
  and Save. The window position saved by 9.0 is not carried over.
- **The terminal UI scrolls** with PageUp/PageDown, Space, Home/End and j/k; bars are never
  blank; axis and rule labels no longer collide; long labels wrap.
- **The GUI and TUI show what was measured**: integrity and SLA checks, every error status and
  errors/throttled per API, the slowest 1 % stage split, capacity with ceiling rules, max and
  p99.9 latency, missed arrivals, the open-loop target and schedule, a traffic line (remotes,
  bytes, amplification), the prove's sizing sweep, notes and environment, five slowest
  operations, and a verb filter on the heatmap and the distribution. Charts that repeat another
  are gone; CPU has its own chart when the client was saturated.
- Run ids use 64 random start values per millisecond; events are `oprobe.events/9`.
- The trace layer keeps the per-interval in-flight peak off its aggregate lock.

### Removed

- `schemas/compare.json` (a comparison is part of `run.json`).
- `oprobe-trace` OpenTelemetry metrics `oprobe.operation.duration` and `oprobe.cache.lookups`.
- The app's display of the `cdn_pop`, `age`, `storage_class` and `encryption` headers (they stay
  in the rows and JSON). `lab --max-store-size` is hidden from `--help`.

### Known issues

- Two processes that create a run in the same millisecond can pick the same id (1 in 64).
- The suggested next `run` command does not carry the run's budget flags.
- An open-loop operation's waterfall shows its queue time after its requests.
- With sampled rows, TCP connection counts cover the kept rows only.

## v9.0.0 — 2026-11-25

9.0 establishes a clean-slate, lossless observability and data contract (`oprobe.trace/9`,
`oprobe.report/7`, `oprobe.run/7`, `oprobe.prove/9`), eliminates concurrency races across cache
instrumentation, TCP upload probes and lab store quotas, delivers comment-preserving `op.toml` /
`config.toml` round-trips, and polishes the native desktop and terminal TUI experience without
adding new commands or configuration concepts.

### Breaking

- **Clean-slate v9 formats and schemas (`https://github.com/hxddh/oprobe/schemas/9/`).**
  - `oprobe-trace` writes and reads `oprobe.trace/9`; `oprobe` writes and reads `oprobe.report/7`,
    `oprobe.run/7` and `oprobe.prove/9`. Pre-v9 files and legacy serde field aliases are no longer
    read.
  - `ReqRow.api` is strongly typed as `Api` (with `service_op` when custom); `OpRow` carries its
    exact Horvitz-Thompson stratum `weight`; `SlowOp` records `bytes`; `ServiceRecord` records
    client posture (`http_version`, `keep_alive`, `max_inflight_count`); `ProveOutput` carries
    `schema` and `operator` so `.prove.json` documents are self-contained.
  - Removed unwired `ReqDetail.{stream_wait_us, flow_stall_us}` fields and redundant
    `RunInfo.interrupted` / `RunSummary.interrupted` flags (`run.partial` is the single source of
    truth).

### Added

- **Registered JSON Schemas for every command output.** Added `schemas/compare.json`,
  `schemas/removed.json` and `schemas/lab.json` alongside the updated v9 schema suite.
- **Complete OpenTelemetry coverage in `oprobe-trace` (`otel` feature).** `OtelExporter` now emits
  `oprobe.operation.duration` and `oprobe.cache.lookups` in addition to per-stage HTTP request
  durations.
- **Read-only prefix support in the app's New Run dialog.** Added an optional **Existing prefix**
  field (`--existing`, automatically enabling read-only mode) and preserved `--existing` and
  `--set` overrides across **Run Again**.
- **Comment-preserving TOML patching (`toml_edit`).** `prove --save` and the app's **Save to
  `op.toml`** / **Save as `op.toml`…** patch existing `op.toml` and `config.toml` tables in place,
  preserving comments, whitespace, `${ENV}` placeholders and unredacted credentials.

### Fixed

- **Lossless sampling and stage/slice accuracy.** `OpSampler` records exact per-stratum weights so
  tail-pinned slow/failed operations never distort sampled range views, `--by` groupings or stage
  breakdowns; `QueueStampLayer` stamps HTTP concurrency queue wait before `ConcurrentLimitLayer`;
  `ListView` counts paginated `ListObjects` calls accurately; `WritePathView` attributes multipart
  uploads across custom chunk sizes; `404` checks and `TailCutLayer` hedges no longer inflate
  remote/API error counts or invalidate production traces.
- **Race-free `CacheObserveLayer`, retry attribution and wire fidelity.** Replaced zero-request
  cache heuristics with per-lookup task-local probes (covering `stat`, concurrent lookups and early
  reader drops without double-counting), fixed retry state reset after successful attempts,
  serialized concurrent TCP kernel upload probes per connection, tracked recent HTTP/2 stream ends
  for accurate `streams_count`, and marked truncated chunked/compressed responses `aborted`.
- **Instant `prove` cancellation, isolated warm `TailCutLayer` state and `FoyerLayer` sizing.**
  User cancellation (`Ctrl+C` / UI Stop) exits immediately as `RunInterrupted` instead of saving an
  inconclusive proof; baseline and candidate `TailCutLayer` instances are isolated by full
  `OperatorSpec`; `repeated_reads` sizes `FoyerLayer` with an `8 × object_size` shard floor and
  sampling scale-up.
- **Compilable OpenDAL 0.59 `to_rust` snippets.** Emits `use` imports in canonical layer order,
  installs `with_context(ReqwestTransport)` before `reader_with`/`writer_with`, honours
  `http.{proxy, ca_cert, insecure}`, and routes `webdav://` and relative `fs://` paths through
  `Operator::via_iter`.
- **Built-in lab concurrency & S3 fidelity.** `StoreUsage` reserves in-flight upload, multipart
  merge and `CopyObject` bytes atomically via CAS without leaking quota on throttled or aborted
  requests; batch-delete XML unescapes entities in a single pass (preserving `+` and `%2B` keys);
  `CopyObjectResult` formats RFC 3339 `LastModified`; `LabServer` checks loopback bind safety
  before opening the socket.
- **Native GUI & `80×24` TUI polish.** Fixed TUI chart focus wiping Braille canvases, added
  bidirectional `Shift+←/→` range selection and keyboard-focusable waterfall bars, eliminated
  Operator input backspace snap-back, streamed live `prove` A/B trials without 100 ms snapshot
  churn, supported standalone `.prove.json` files without local `.oprobe` runs, synced OS menu
  language switches immediately, and guarded async run/compare loads with monotonic sequence IDs.

## v8.0.0 — 2026-11-24

8.0 advances embedded and live wire observability (`oprobe.trace/8`), closes OpenDAL 0.59 Rust
code generation and engine measurement gaps, streamlines side-by-side CLI comparisons and
`prove --save`, and polishes the native desktop and terminal TUI experience without adding a
single new CLI command or configuration concept.

### Breaking

- **`oprobe-trace` writes `oprobe.trace/8`.** `oprobe show` and the app continue to open
  `oprobe.trace/7`, `oprobe.trace/6` and `oprobe.run/6` files seamlessly.

### Added

- **End-to-end wire & layer observability (`oprobe.trace/8`).**
  - `Handle::cache_layer` (`CacheObserveLayer`) wraps `FoyerLayer` to record exact cache hits,
    misses and bytes served from memory (`bytes.cache_hits_count`, `bytes.cache_misses_count`,
    `CacheView`).
  - Composable `ConcurrentLimitLayer` HTTP semaphore wait attribution (`timing.queue_us`) when
    layered above `TraceLayer` / `TraceContextLayer`.
  - HTTP/2 concurrent stream tracking (`h2_streams` peak & open streams) and coalesced connection
    dial wait (`dial_wait_us`).
  - Upload kernel-buffer drain vs server acknowledgement timing (`send_buffered_us`,
    `upload_acked_us`), explicit `TailCutLayer` cut attribution (`tail_cut`, `tail_cuts_count`)
    and cancelled-request tracking (`aborted`).
  - Multi-cloud response headers (`cdn_pop`, `age_s`, `storage_class`, `encryption`, Azure
    `x-ms-retry-after-ms`), `503 SlowDown` vs `503 ServiceUnavailable`/`InternalError`
    classification, `WritePathView` multipart straggler skew & `CompleteMultipartUpload` p99, and
    `ListView` pagination depth (`pages/list`).
  - Race-free per-connection kernel TCP statistics (`TcpProbe` owned per connection across
    concurrent HTTP/2 streams on Linux and macOS, plus `WSAIoctl(SIO_TCP_INFO)` support for owned
    sockets on Windows).
- **Concise CLI workflows.**
  - `oprobe show <A> <B> […]` compares stored runs, `.oprobe` files or production traces side by
    side (`-o text`, `-o json` matching `schemas/run.json`, `--detail` for per-API comparison).
  - `oprobe prove --save <FILE|OPERATOR>` atomically writes proven improvements back to an
    `op.toml` file or `[operators.NAME]` in `config.toml`; `show` resolves `last-prove`,
    `.prove.json` files and unambiguous prove ID prefixes.
  - Multi-cloud paste-and-run URL recognition for Alibaba Cloud OSS, Tencent COS, Huawei OBS,
    Volcengine TOS, Cloudflare R2 and Backblaze B2, plus `~/.aws/config` profile `region` and
    `AZURE_STORAGE_ACCOUNT_NAME` discovery.
  - Workload-aware `oprobe run` reproduction hints (`--preset list-stat|range-reads|large-objects`)
    on production traces, and `oprobe lab --allow-remote` / `--max-store-size`.
- **Native app & terminal TUI polish (`nsh 0.6.1`).**
  - Stored proves (`<id>.prove.json`) reload in the Operator view across run switches and app
    restarts, with **Compare A/B trials** and **Save as `op.toml`…**.
  - Full terminal clipboard support (`TermClipboard`: OSC-52 with `tmux` passthrough, OS clipboard
    tools and in-memory fallback), resizable sidebar and full keyboard shortcut parity in TUI.
  - Overlay scrollbars (`ScrollbarTheme`), horizontal scrolling on wide Compare tables, keyboard
    range selection (`←` / `→` / `Shift+←/→` / `Enter`) on timeline charts, single-instance
    file wakeup (`next_second_instance`), and smart multi-file drag-and-drop / open (`op.toml`,
    `.prove.json`, multiple `.oprobe` files → Compare).

### Fixed

- **Compilable OpenDAL 0.59 `to_rust` codegen.** Alias schemes (`gs://`, `r2://`, `lab://`) and
  `[options]` now emit `Operator::via_iter` with canonical OpenDAL schemes and required
  `HttpTransporter` / `OperationContext` / `ReqwestTransport` imports.
- **`OpKind::Write` chunk & concurrency.** Direct `write` operations now apply `write.chunk` and
  `write.concurrent` via `writer_with`, and record `"write.multipart"` when multipart uploads occur.
- **Finding & `prove` accuracy.** Unshadowed `small_ranges` `read.gap` sizing (`4 KiB .. 32 MiB`),
  refused `bandwidth_cap` layer proofs cleanly on read-only workloads, and preserved `.oprobe`
  filenames with dots when writing `.tmp` files.
- **Built-in lab fidelity.** Omitted illegal `connection: close` headers on HTTP/2 responses and
  deduplicated `CommonPrefixes` across paginated `ListObjectsV2` delimiter responses.
- **UI responsiveness & state safety.** Prevented 100 ms menu rebuilds during live jobs, isolated
  reactive effects outside language-switch rebuilds, synced worker-thread `i18n` atomically, seeded
  `http = 32` when toggling `layers.concurrent_limit` on, and routed non-lab **Run Again** through
  plan review.

## v7.2.0 — 2026-11-24

7.2 sharpens measurement accuracy, OpenDAL 0.59 code generation, `oprobe-trace` embedding safety,
and the `prove` → `op.toml` loop without adding commands or configuration concepts.

### Added

- **Save to `op.toml` in the app.** When a run came from an `op.toml` file, **Save to op.toml** in
  the Operator view writes a proven change back to that file atomically, preserving unedited
  options, `${ENV}` placeholders and relative paths.

### Changed

- **Faster `prove` on `tail_cut` remedies.** Candidate trials of one proof now share a warmed
  `TailCutLayer` once its initial 10 s blind window has rolled over, cutting `prove` wall time on
  `tail_stragglers` by more than half while keeping baseline and candidate warm-ups matched.

### Fixed

- **OpenDAL 0.59 HTTP Rust diffs.** `http.*` changes now emit `ReqwestTransport` installed via
  `OperationContext::with_http_transport` instead of the removed `HttpClient::with` / `HttpFetch`
  APIs, and `http.version = "h2"` uses `.http2_prior_knowledge()` only on plain `http://` (`h2c`)
  endpoints so HTTPS endpoints keep TLS ALPN negotiation.
- **Peak in-flight count under `layers.concurrent_limit.http`.** `FidelityTransport` now counts a
  request as in flight only after acquiring its concurrency permit (and decrements before releasing
  it), so `peak_inflight_requests_count` and the timeline reflect wire concurrency rather than
  workers waiting in the client queue.
- **In-process `copy` and `rename` requests.** `InProcessObserveLayer` (`fs://`, `memory://`) now
  records `CopyObject` requests for `copy` and `rename`, fixing stage attribution (`client_pct` was
  100 %) and enforcing `--max-requests` / `--max-bytes` budgets on copy workloads.
- **`oprobe-trace` composition and multi-operator metrics.** `OpStream::read` keeps the operation
  scope active so mid-stream retries link to their operation when `RetryLayer` sits inside
  `TraceLayer`; cloning `oprobe_trace::otel::config(&meter)` across several operators gives each
  its own `scheme`, `namespace` and `root` attributes.
- **Relative `http.ca_cert` paths and S3 region detection.** `OperatorSpec::load` resolves relative
  `http.ca_cert` paths against the `op.toml` file's directory, unreadable or malformed CA bundles
  fail as `config.invalid` (exit 2) instead of `internal`, and `detect_s3_region` honours explicit
  `http.proxy`, `http.ca_cert` and `http.insecure` settings.
- **Custom endpoint hints in production traces.** `custom_endpoint` now recognises dual-stack,
  multi-node private clusters and CGNAT/Tailscale (`100.64.0.0/10`) addresses.
- Removed redundant `package.homepage` manifest keys that warned under Cargo 1.97+.

## v7.1.0 — 2026-11-24

7.1 is the release planned in `docs/design/v7.1-review.md`: the bugs a bug hunt across every lab
scenario found, remedies that prove, and an app that shows every signal honestly. Everything is
additive (JSON fields, schemas).

### Fixed

- **The `errors` remedy proved worse.** For transient failures that retries recover, it is now
  `layers.retry.min_delay`, sized by `prove` (from 20 ms), with jitter: `lab://flaky` proves
  improved (several-fold goodput) instead of worse (−13 %). Network failures (connection resets)
  count in the finding; the flaky lab injects the rates it describes.
- **`run A B` ran a different workload per lab operator** while saying "the same workload": every
  operator now runs the first one's workload, and `differences` lists any that remains. The budget
  plan covers all of them.
- **`prove` judges throttling by what the change adds:** inconclusive only when the candidate is
  over `--max-throttle`, above the current operator by more than the limit, and beyond chance; a
  remedy that is not for throttling may keep the throttling the run already had.
- The text brief kept a partial run's `invalid_run` first, as the JSON does.
- `show PROVE_ID` shows a stored prove (as `prove` printed it); `show --list` lists proves within
  `-n` and says how many there are.
- A budget stop prints the runs that finished (text) or carries them with the error (JSON).
- Trace findings and prove hints name a concrete `oprobe run URL` (with its endpoint).
- `show FILE --by` on a file without request rows, and `--since` past the window, are refused
  with errors that name what is wrong; zero durations are refused.
- Dry-run plans count transfer time for in-process targets and print durations exactly; the prove
  ETA plans only the trials the budget fits.
- One failed request in a production trace is not an `errors` finding; open-loop runs whose
  arrivals queue name the workers, not the CPU; `bandwidth_cap` on reads is not a layer.
- Bytes on the wire are measured (they read 0); connections say how many were dialed and when;
  the kernel's smoothed RTT is shown next to the minimum RTT.
- The in-flight ceiling's bracket no longer rests on levels with fewer than 3 arrivals.
- Error docs links point at the installed version; a damaged run file is called damaged;
  requests whose connection was reset show under "no connection".

### Changed (the app and the profile)

- The timeline ends at the measured window (a drain interval no longer drops every chart to zero
  at the end).
- New charts: **problems per second** (retries, throttled responses, failed operations, on their
  own axis) and **throughput** over time.
- The latency distribution is a **tail plot** (share slower than, log–log, p50 / p99 / p99.9
  marked), also in the stragglers evidence and in Compare.
- The heatmap's slowest row is closed at the next step (stragglers at 260 ms no longer paint a
  band up to 1 s).
- Finding evidence has labels and units (zh and en); detail tables show share bars, size ranges,
  "first read / re-read", and drop empty columns; noise lines and all-zero legend entries are gone.
- Requests by API draws each API's p50 inside its p99.
- **Compare** adds p99.9, requests/s, failed operations, retries and throttled responses, p99 and
  requests over time, and a per-API table.

## v7.0.0 — 2026-11-23

7.0 is the release planned in `docs/design/v7-review.md`. Embedded traces now count operations,
not only HTTP requests; that is the one breaking change, and `oprobe.trace/6` files still open.
Everything else is additive: the report and run formats stay `oprobe.report/6` and
`oprobe.run/6`.

### Breaking

- **`oprobe-trace` records operations for every service** (it used to record only the requests
  of HTTP services). Each OpenDAL call is one operation linked to its requests (`stat`,
  `read.full` / `read.range`, `write` / `write.multipart`, `list`, `delete`, `copy`, …), so
  production traces have verbs, operation latency, the time breakdown, slow operations, hot keys
  and prefixes (hashed: the first path segment and a 48-bit hash, never the key), range sizes and
  over-read. Request counts are no longer labelled as operations.
- The trace format is `oprobe.trace/7`; `oprobe.trace/6` files still open (each request counts
  as one operation there, as before).
- `opentelemetry` 0.32 for the `otel` feature (the version OpenDAL 0.59's
  `opendal-layer-otelmetrics` uses, so both share one global meter). Metrics gain the attributes
  `scheme`, `namespace`, `root` and `operation`.

### Added

- `Handle::retry_notify()`: give it to OpenDAL's `RetryLayer::with_notify` and an embedded
  trace counts retries, their backoff and throttling, with every attempt of one call in one
  operation.
- `Handle::save_and_reset(path)`: save a window and start the next one, so hourly files are
  hourly profiles.
- **The profile shows every captured signal** (terminal, JSON and app):
  - a `resilience` line in KEY NUMBERS (retries, backoff, 429/503 and the longest Retry-After,
    the 5xx/4xx/network split, the top failure classes);
  - connections: TCP kernel stats (RTT, retransmits, window- and buffer-limited time), TLS
    resumption and HelloRetryRequests, cold start, and proxy CONNECT in the dial phases;
  - the write path: requested and effective part size, the service's limits, the parts uploaded;
  - request signals (HTTP/2 stream and flow-control waits, upload acknowledgement, cache status,
    region) in the waterfall and in `show --by stage`;
  - charts: throttled responses and failed operations per second next to retries; new
    connections and client CPU on the in-flight chart; the consumer's stall as its own waterfall
    segment; hot keys as bars of their share of throttling;
  - the same p50 / p99 in the terminal's and the app's API tables, per-verb MiB/s and throttled
    share, cold/warm slices, and slow operations' async call stacks in the app.
- Finer failure classes: `storage.condition_not_match` (412), `storage.conflict` (409),
  `storage.already_exists` and `storage.range_not_satisfied` (416), with OpenDAL's temporary /
  persistent bit on every failed request.
- Schemas for `run -o json` (`schemas/run.json`) and the `--dry-run` plans (`plan.json`,
  `prove-plan.json`).
- A pasted cloud address runs as is: `https://bucket.s3.region.amazonaws.com/x`, path-style S3,
  `storage.googleapis.com` and `*.blob.core.windows.net` become `s3://`, `gs://` and
  `azblob://` (a presigned query is dropped).
- `oprobe FILE.oprobe` (and a double click on a run file on Windows and Linux) opens the app on
  it; the `.desktop` entry and the MSI register the file type.
- Runs killed hard leave a pending marker; the next run against the same target removes what
  they left (only `oprobe-run-*` prefixes, never under `--read-only`).
- The app: Settings ▸ Your storage, Help ▸ Glossary of Terms, Help ▸ Docs for the installed
  version, and Show in Finder / Explorer / Files for a run. `show` ends with the run file's path.
- A glossary in `docs/concepts.md`, linked from every brief.

### Changed

- `prove`: a candidate that throttles over `--max-throttle`, and more than the current operator,
  is `inconclusive`, with a note.
- A run that cannot be judged (stopped early, too short) leads with `invalid_run`, suggests no
  `prove`, and `prove` refuses it. A partial run is **Review**, as documented.
- Budgets stop at the limit (`--max-requests 500` never sends 501); a run that spends exactly its
  budget is whole. `--dry-run` shows the budgets and warns when the plan would spend one.
- Errors state one plain cause first ("connection refused by 127.0.0.1:9999", "cannot resolve
  HOST", "credentials rejected by HOST (credentials from AWS_ACCESS_KEY_ID)"); the storage
  library's report is the JSON `detail`. A DNS failure is `net.dns`.
- Hints list only the services in this build; a known service that is not compiled in says so.
- The throttling headline leads with its cost (time in retry backoff); `prove` says "95 %
  interval"; the warmup warning names `--set warmup=…`.
- `lab://demo` throttles above 40 requests in flight (24 before), so the demo is about its
  stragglers and its remedy proves without throttling.
- Release notes list a package channel only when it was published.
- Dependencies: nsh 0.6.1, rustls-platform-verifier 0.7 (one copy, as reqwest uses), sha2 0.11,
  zstd 0.14, dirs 7, rcgen 0.14, reqsign (AWS role chains in profiles); unused webpki-roots and
  rustls-native-certs removed.

### Fixed

- A failed request's URL (with a presigned or SAS signature) is never recorded in a trace, and a
  refused web address is never echoed with its query.
- An embedded trace's memory stays bounded in a long-lived process, even with an operation left
  open; old intervals are merged.
- Slow operations' async call stacks are kept in stored runs.
- Hints no longer name `--endpoint` or `[targets.*]`.
- Output order is deterministic (the headline's APIs, confidence at a threshold).
- `s3-website-*` and FIPS hosts are not taken as regions.

## v6.3.0 — 2026-11-23

### Added

- **The timeline counts what it says** (`oprobe.report/6`, `oprobe.run/6`, `oprobe-trace`
  `TimelinePoint`; additive fields):
  - `goodput_ops_per_s` and `ok_ops_count`: successful operations per interval (`ops_per_s`
    stays the attempted rate, failures included);
  - `requests_count` / `requests_per_s` (every attempt, retries included) and `retries_count`,
    so rate limits show as requests per second;
  - `ops_count` (the operations behind an interval's quantiles) and `throttled_ops_count`;
  - `verbs`: per-verb counters per interval (operations, successes, failures, throttled
    responses, a latency sum and the heatmap rows), so a view can filter the timeline by verb.
  - The `interval` event carries `goodput_ops_per_s`, `requests_count` and `retries_count`.
- **The run's whole latency distribution** (`latency_histogram`: 20 bins per decade, every
  operation, never sampled) in the report, the run file and `oprobe-trace`'s `Snapshot`. The
  latency CDF is drawn from it when the trace rows are sampled.
- Core view model: `HeatmapView` (rows in 1-1.5-2-3-5-7 steps per decade when every row was kept,
  otherwise the report's 20 fixed rows), `TimelineView::for_verb` / `verbs`, goodput, requests,
  retries, p90 / p99.9 / max and per-interval operation counts on `TimelineView`, and
  `TimelineView::reliable(q)` / `min_ops_for(q)` (n ≥ 10 / (1 − q)) so renderers can mute
  quantiles of intervals with too few operations; `FindingView::remedy_line`;
  `Slice::throttle_share_ratio` for hot keys.
- `App::trash`, `App::restore` (`TrashedRuns`) and `App::configured_operators` in the service
  layer.
- Headless UI tests that run as macOS and as Windows: menus, shortcuts, Settings, alerts and
  their default button, dialog button order, the sidebar's keys, New Run's plan, and quitting
  during a run.

- **Brush and link in the app.** The timeline charts and the heatmap share one time cursor; a
  drag across any of them selects a time range, shaded on every time chart. With a range
  selected, the key numbers, the requests by API, the verbs, the latency distribution and the
  slowest operations are recomputed for it from the trace rows (sampled rows say so and are
  weighted). Escape, a double-click or **Clear** drops it.
- **`oprobe show RUN --since 3s --until 8s`**: the same range from the CLI (its goodput, latency
  quantiles, failures, requests, retries and throttling, where the time went, the verbs, the
  requests by API and the slowest operations; `-o json` adds a `range` object). With `--by`,
  only the range's requests are grouped.
- **Drill down in the app**, each on demand: a finding's **Show N examples** expands the
  waterfalls of its example operations; a click on a heatmap cell lists the slowest operations
  of that time bucket and latency row; a click on an API lists its slowest requests; a click on
  one of those opens its waterfall. Hovering a request in a waterfall also shows the provider's
  error code, `Retry-After`, rtt / min rtt / retransmits and `Server-Timing` when recorded.
- **A verb filter above the app's timeline** (All verbs, or one): goodput and latency follow it,
  and the distribution when no range is selected; requests and in-flight say "all verbs".
- The app's timeline gains a **Requests per second** chart with retries per second beside it.
- Every chart in the app has the Image role and a label that summarises it ("Goodput over 8 s,
  1.0k/s–1.1k/s").
- Core: `report::range` — `RangeView::of(run, since, until)` (also `view::RangeView`), the
  drill-down queries `ops_in` (a heatmap cell) and `slowest_requests` (an API), `view::examples`
  and `view::waterfall`, `LatencyCdf::for_verb`; `WaterfallRow` carries the wait before a retry,
  the provider's error code, `Retry-After`, rtt, min rtt, retransmits, `Server-Timing` and the
  request id.

### Changed

- `interval` events are sent once the next interval has also closed, so their requests and
  retries are final for operations shorter than an interval; `report.timeline` stays the final
  count. `show -o json` has a schema (`schemas/show.json`).
- **The demo proves.** `oprobe demo` then `oprobe prove last` (every default) proves its top
  finding `improved` on goodput; a convergence test runs exactly that.
  - Throttling's severity follows its cost, not its presence: critical when more operations
    fail out of retries than a run allows or when half the operation time is retry backoff, a
    warning when some fail or the backoff is a noticeable share, otherwise informational.
  - Within one severity, the costliest finding comes first (`time_cost_ratio`, a new evidence
    value of the throttle family and `tail_stragglers`).
- **One prove budget.** `--max-duration` is the wall-time budget of the whole proof; no trial
  starts that would not end within it. `--budget` still works as a hidden alias.
- **The ETA covers the whole proof.** `prove --dry-run` and the start of every prove print the
  number of trials (sizing, then the fewest to the most pairs) and the ETA of the whole proof.
  `prove --dry-run -o json` adds `proof` (trial counts, `eta_min_s`, `eta_max_s`).
- A tail-cut proof warms up only its candidate trials for 11.5 s; the baseline trials keep the
  short warmup.
- With no pair of trials, `prove` prints `not measured` and no diff (JSON: `inconclusive`,
  `pairs_count: 0`, a note).
- **One vocabulary**: verdict levels read OK, Review, Attention, Critical (JSON keeps `ok`,
  `review`, `attention`, `critical`); prove results read improved, worse, inconclusive; the runs
  of a proof are trials.
- **Output is never clipped.** `show` wraps long lines instead of cutting them, `show --list`
  prints whole targets and changes, and a word longer than the line stays whole.
- **Diffs show only what differs from the defaults**: the TOML of a changed layer no longer
  repeats its default fields (`retry = { jitter = true }`, not the whole table).
- **Errors lead with the provider's code**: `credentials rejected: InvalidAccessKeyId: …`. The
  raw error chain moves to the new `detail` field of the JSON error.
- `show --detail`: the timeline names its real interval (1 s, or 10 s after ten minutes), adds
  p99.9 and max, merges long runs into at most 60 rows, and labels errors and throttled as counts
  per row. A new **Spread** section shows the hottest keys, object sizes, prefixes and remotes.
- `oprobe lab` lists the app's six scenarios (`demo`, `throttle`, `stragglers`, `bandwidth`,
  `flaky`, `ratelimit`); an unknown name lists them. The other scenarios still run by name.
- The docs describe the prove interval as it is computed: Student's t on the paired log ratios,
  no narrower than the A/A noise floor, and Newcombe for shares.
- `docs/operator.md` recommends an `op.toml` path for your operator; `[operators.NAME]` in the
  config file still works.
- Core: `App::prove_finding` and `ProveRequest::for_finding` prove a finding's remedy, sized as
  the CLI sizes it.
- The timeline buckets events by **when they happened**: requests when sent, throttled responses
  when they arrived, retries when issued and failures when the operation ended (operations and
  their latency quantiles stay with the interval they started in). A 503 at +7 s is drawn at
  +7 s, not at the second its operation began. Events of operations draining after the window
  closed land in its last interval.
- `tail_stragglers` names every API with stragglers (its cause lists each API's share), not only
  the one with the largest gain; evidence gains `affected_apis_count`.
- Hot keys in the view model carry `throttle_share_ratio` (share of all throttling); the slices'
  `throttled_ratio` is the share of the slice's own requests, and is `None` for hot keys.
- **The app's Prove proves the finding, as the CLI does.** A finding's Prove (Operator) and
  **Prove this remedy** (Profile) pass the finding to `App::prove`, which sizes its remedy from
  the run like `oprobe prove RUN CODE`; the app no longer invents values (`concurrent_limit =
  16`, `timeout = 5s`, …). A value you change by hand is still proved as you set it.
- **One vocabulary in the app**: verdicts OK / Review / Attention / Critical (正常 / 值得一看 /
  需关注 / 严重), prove results improved / worse / inconclusive (有改善 / 变差 / 无定论), and
  trials (试验) instead of replays. Finding titles of 5.x codes are gone.
- **Delete moves runs to the trash, with Undo.** The app asks with the system's alert (Cancel is
  the default) and moves the runs to `<data>/trash/` (`App::trash`); **Undo** in the notice bar
  brings them back (`App::restore`). `oprobe show --rm` is unchanged.
- **Native platform behaviour**:
  - the sidebar shows the window's material (vibrancy on macOS, Mica on Windows 11; only the
    detail paints the background), is a keyboard listbox (↑/↓, Shift to extend, Home/End,
    type-select, Delete or ⌘⌫, Return) with listbox/option roles, selects the platform's way
    (accent or inactive grey on macOS, subtle fill with an accent pill on Windows and GNOME),
    outlines the row a context menu acts on, and can be resized;
  - Settings is a window of its own on macOS (changes apply at once) and a preferences dialog
    elsewhere, and shows the config file and the data directory;
  - Windows and Linux get a flat primary menu under its button, About oprobe, Quit (Ctrl+Q) or
    Exit, and full screen (F11); menu items with a system role are labelled in the app's
    language; menu shortcuts that a text field uses no longer fire while you type, and menu
    actions wait while a dialog is open;
  - quitting or closing the window during a run or a prove asks first (Stop and Quit / Cancel);
  - the sidebar, the page and (macOS, Windows) the window's placement are restored; control
    heights grow with the system's text size; the toolbar has each platform's height;
  - New Run focuses the operator field, takes Return anywhere, offers the workload as a pop-up
    and the lab's scenarios, configured operators (`[operators.NAME]`) and recent `op.toml`
    files in a Choose pop-up, and **plans a real target first** (`App::plan`: what it writes,
    reads and deletes, its ETA and budgets) before it offers Run;
  - Open Run File uses the system's open dialog filtered to `.oprobe` / `.json` and opens the
    file in place; files opened from Finder or Explorer open; a file dragged over the window
    outlines it;
  - a run or prove that ends in the background shows a notification and asks for attention;
    the notice bar is read out by screen readers; dialogs do not fade with reduced motion; the
    title in the toolbar no longer takes the title bar's double click; the detail is centred on
    wide windows.

- **Honest charts in the app.** Goodput is drawn from successful operations, with the attempted
  rate (failures included) as a faint labelled line behind it; the shading and the throttling
  evidence count throttled **responses** and say so; p50 / p99 / p99.9 of intervals with too few
  operations are drawn faint and dashed, and the readout says why (`n=12, too few for p99`); the
  heatmap uses the fine rows when every row was kept and has a colour key from the fewest to the
  most operations in a cell; the distribution comes from the run's histogram.
- **Compare stays neutral**: single-run deltas are no longer green or red; the note points to
  prove.
- The hottest keys' column reads **Share of throttling**; the other slices keep **Throttled**
  (the share of their own requests).
- (Tests) The app's plot tests now build `TimelineView`s; `the_timeline_shades_only_the_seconds_
  that_were_throttled` became `…_intervals_with_throttled_responses` (the band's label changed).

### Fixed

- `show --detail`'s Spread prints a hot key's share of all throttling (it printed `-`).
- A brushed time range (app, `show --since --until`) counts failures when they happened, as the
  timeline does; goodput stays the operations that started in it and succeeded.
- Restoring trashed runs refuses any folder but one `App::trash` made (no `..`); a trash that
  fails part-way puts back what it moved.
- `prove --budget` with `--max-duration` (both, as a 6.2 script might pass) takes the shorter
  instead of failing.
- `throttled_count` in the timeline (and `interval` events, `IntervalBlock`) counts throttled
  **responses**, as documented and charted; it counted throttled operations, so a run with 41
  throttled responses summed to 22.
- A rate limit under OpenDAL's default retry (`lab://ratelimit`) is diagnosed as `rate_limit`, not
  an in-flight limit: the service serving bursts of requests no cap of that size carries (request
  rate within 8 service times above 1.5× `c / S`) refutes the in-flight model.
- Findings weight sampled rows by the operations they stand for (`tail_stragglers`,
  `hung_requests`, `cold_connections`): the samplers always keep the slowest, failed and throttled
  operations, which over-weighed the tail. `tail_stragglers` takes the operation p50 / p99 from
  the exact summary.
- `repeated_reads` says its unit (requests saved per 1 000 successful read operations) and prints
  the same number as its evidence value; per-1 000 values print as whole numbers.
- `key_limit` (and any remedy that is not a layer) no longer prints its remedy twice in `show`.
- The fine heatmap leaves warmup operations out and starts at the step just below the fastest
  operation.
- The macOS menu bar is built again (Settings, New Run and the app's shortcuts): the sidebar's
  shortcut no longer panics there (`ctrl+cmd+s`).
- Contrast: white on the dark accent (now `#2a73d6`, 4.6:1), field and switch borders (3.3:1),
  and the warning and OK colours as small text (`#9a5c00`, `#16843f`).

### Removed

- The SLA surface of 5.x: `show` prints no SLA line, and `--set sla`, `max_error_rate`,
  `max_throttle_rate` and `p99_ms` are refused with an error. The report keeps its `sla` fields
  (always the defaults).
- Run labels from the help, the docs and `show --list` (the JSON field stays).
- Hints and warnings that named flags oprobe does not take (`--option`, `--anonymous`,
  `--ca-cert`, `--insecure`, `--proxy`, `oprobe init`, `--warmup`, `--microscope`); they name
  `--set` paths instead.

## v6.2.0 — 2026-11-23

### Changed

- **The app is a native window** ([design](docs/design/app-native.md)). The row of tabs and the
  bordered cards are gone:
  - a sidebar of runs grouped by operator, on the platform's sidebar material; select runs with a
    click, mod-click and shift-click, and two or more show Compare;
  - the title bar holds the toolbar: Profile/Operator, New Run, Run Again/Stop, and the main menu
    on Windows and Linux;
  - menus with the platform's shortcuts (the menu bar on macOS), and a context menu on runs;
  - New Run, Settings (the language) and Delete are dialogs;
  - progress shows on the Dock or taskbar; open a run file or drop it on the window;
  - control sizes follow each platform, headings are in sentence case, and Operator's settings
    are grouped.

### Added

- **Profile shows what the run measured, not only its verdict**:
  - requests by S3 API: count, status codes, TTFB p50 / p99, p99, bytes, reuse;
  - retries and failures: retries, backoff time, 429 / 503, the longest `Retry-After`, failure
    classes;
  - requests in flight and p99.9 on the timeline, and p99.9 among the key numbers;
  - **Details**: byte amplification, the hottest keys, object sizes, prefixes and server
    addresses;
  - **Run setup**: workload, load, dataset, mix, seed, client health, rows kept, and the command
    that runs it again;
  - a note up front when the numbers are less certain (a saturated client, a partial run, late
    arrivals, few operations).
- Evidence for more findings: hot keys (`key_limit`, `repeated_reads`) and byte amplification
  (`small_ranges`). Core view model: `SetupView`, `TrustView`, `ApiRow`, `RetryView`,
  `BytesView`, `SpreadView`.

### Fixed

- The request waterfall draws the wait before each retry. In a throttled run it was blank,
  though it was most of the time.
- Compare shows a change of a share (errors, throttled) in percentage points: from 0.08 % to
  0 % is −0.08 pp, not −100 %. It also notes that each column is a single run.
- Findings come before the stage split. The key numbers say what they are a share of (failed per
  operation, throttled per request) and how many operations were measured.

- `prove`: the noise floor is never below what a trial can resolve, `ln(1 + 1/ops)` for its
  fewest operations. A/A trials of a few dozen operations that happened to agree exactly let a
  one-operation difference read as a significant change (a `worse` verdict from rounding).

## v6.1.0 — 2026-11-23

### Added

- **The app draws its evidence.** Each finding carries the chart that shows it: the share throttled
  at each number of requests in flight with the fitted limit's interval (`throttling`,
  `key_limit`), throttled or failed operations per second (`rate_limit`, `errors`), bandwidth over
  time (`bandwidth_cap`), the latency distribution (`tail_stragglers`, `hung_requests`).
- **Prove is drawn as it converges**: each A/B pair's change, the running estimate with its 95 %
  interval, and the A/A noise floor around no change.
- Profile: the latency distribution (CDF) with p50 and p99 marked, the stage split of each verb,
  and the throttled and failed seconds shaded under the goodput timeline.
- Compare: goodput over time and the latency distributions of every run on one chart each.
- Core view model: `LatencyCdf`, `VerbView`, `ConcurrencyView`, `EvidenceChart`, and
  `prove::convergence` (the interval after each pair, the same computation as the verdict's).

### Fixed

- Linux: the window opens when the app starts from its desktop entry. GTK 4 unsets
  `DESKTOP_STARTUP_ID` and `XDG_ACTIVATION_TOKEN` before `main`, so the launch was taken for a
  script and the help was printed instead.

## v6.0.0 — 2026-11-22

Design [v6](docs/design/v6.md), from the review in [v6-review.md](docs/design/v6-review.md).
oprobe 6 is **a profiler for OpenDAL operators**: one workbench (the app and four CLI commands),
one layer (`oprobe-trace`) and one file (`.oprobe`). It is a deep cut, built around one loop.
Everything under **Changed** and **Removed** is breaking; 5.x run files are not read.

### The new model

- **The operator is the unit.** An OpenDAL `Operator` (a service, a layer stack, read/write and
  HTTP options) is given as a URL, an operator file (`op.toml`, `oprobe.operator/6`) or a
  configured name (`[operators.NAME]`). `OperatorSpec` replaces targets, knobs and client
  stacks; `--set PATH=VALUE` changes one value, and the difference between two operators renders
  as Rust and TOML.
- **Profile → Explain → Prove.** `run` profiles one or more operators on one workload and
  explains each finding with its evidence and exactly one OpenDAL remedy (an `op.toml` path) and
  no number. `prove` is the only source of numbers: it sizes the remedy, runs A and B
  interleaved with a sequential test against a learned noise floor, and decides
  `improved | worse | inconclusive` on your objective (goodput by default, or p99). The result is
  a proven layer diff.
- **Findings use OpenDAL's layers:** `ConcurrentLimitLayer`, `RetryLayer` with jitter,
  `ThrottleLayer`, `TailCutLayer`, `TimeoutLayer`, `FoyerLayer`, read/write chunking and the
  `HttpFetch` settings. The catalog is `throttling`, `rate_limit`, `key_limit`, `bandwidth_cap`,
  `tail_stragglers`, `hung_requests`, `small_ranges`, `slow_multipart`, `repeated_reads`,
  `cold_connections`, `client_bound`, `errors` and `invalid_run`.
- **Four workload presets** (`small-files`, `large-objects`, `range-reads`, `list-stat`) replace
  the five of 5.0.

### Changed

- **CLI: four commands** (29 in 5.0): `oprobe run OPERATOR…`, `oprobe prove RUN [FINDING]`,
  `oprobe show [RUN|FILE]` (with `--list`, `--rm`, `--detail`, `--by`) and `oprobe lab
  [SCENARIO]`; `oprobe demo` is `oprobe run lab://demo`, and a bare `oprobe` opens the app.
  Global flags: `-o text|json`, `-q`, `--color`, `--non-interactive`.
- **Exit code 3** means that `prove` refuted the change (worse on the objective); 0, 1, 2, 4, 5
  and 130 are unchanged.
- **The app** is rebuilt on nsh with four views (Runs, Profile, Operator, Compare); in the
  Operator view a toggled layer is proven live. It speaks 简体中文 and English; the CLI and JSON
  output are English only.
- **Files and schemas:** `oprobe.run/6` and `oprobe.report/6`; `oprobe-trace` writes
  `oprobe.trace/6`. Six schemas are published (`report`, `prove`, `run-list`, `operator`,
  `error`, `events`).
- **Prove trials** are saved as children of their prove run and hidden from `show --list`.
- **Trust fixes:** censored open-loop arrivals count in every percentile; tail attribution
  follows the stage split (retry backoff is not "TTFB"); a saturated client feeds the verdict;
  "high" confidence requires the fit to explain most of the effect; intervals stay in range;
  throttle-driven failures are reported as throttling, not as an invalid run; counts agree with
  totals.
- **`oprobe-trace` is slim and stable.** The API is `layer(Config) -> (TraceLayer, Handle)`,
  `Handle::snapshot()`, `Handle::save(path)` / `Snapshot::save(path)` (a `.oprobe` profile that
  `oprobe show FILE` opens) and the `Exporter` hook; `Config` is a builder and public types are
  `#[non_exhaustive]`. The CLI's error codes and exit codes moved to `oprobe-core`; trace rows
  carry `FailureCode`. `schemars` is optional; the probe code, the microscope dialer, hyper and
  the root-store builder are gone. A real overhead benchmark
  (`cargo bench -p oprobe-trace --bench overhead`) replaces the self-reported number; the budget
  is 2 µs per operation.
- **Crates:** `oprobe-trace` is the published API. `oprobe-core` is still published, only so
  that `cargo install oprobe` works; it is documented as internal with no semver promise, and
  the crates pin each other with exact versions.
- **Releases:** every platform gets the app build and a CLI-only build
  (`oprobe-cli-<ver>-<target>` archives; `--no-default-features --features cli`). `install.sh
  --cli` / `install.ps1 -Cli` install it; `install.sh --help` prints help instead of installing.
- **`prove` needs at least 3 A/B pairs to decide:** with fewer (a small `--budget`), the verdict
  is `inconclusive` with a note; `--pairs` is at least 3. The objective's interval uses Student's
  t for the number of pairs, and a noise floor from a single A/A difference is doubled.
- **A budget breach mid-prove keeps the partial result:** the prove stops, prints and saves what
  it measured (`budget_exceeded: true`) and exits 4; its trials stay removable with `show --rm`.
- **`layers.tail_cut` carries every parameter oprobe applies:** `percentile` (50..99),
  `safety_factor` (1.5), `min_samples` (32), `min_deadline` ("50ms"), `max_deadline` (the
  operation timeout) and `window` ("60s"). The measured layer and the generated Rust are built
  from one parameter list, and the Rust spells out every value that differs from OpenDAL's
  defaults, so pasted code behaves as measured.
- **Docs** are rewritten around the loop: getting started, concepts, `op.toml`, the CLI,
  reading the profile, embedding, the app, install, troubleshooting, privacy and releasing.

### Removed

- **Commands:** `tune` (and its Bayesian optimiser), `verify` and `fix` as commands (they are
  `prove` now), `compare`, `baseline`, `watch`, `matrix`, `compat`, `doctor`, `probe`, `target`,
  `analyze`, `explain`, `runs` (now `show`), `top`/`gui` (now a bare `oprobe`), `config`,
  `cleanup`, `schema`, `capabilities`, `self-update`, `completions` and `man`.
- **The MCP server** (`oprobe mcp`, 21 tools, prompts and resources). Agents use `-o json` and
  the skill in `skills/oprobe/SKILL.md`.
- **The GitHub Action** (`action.yml`), baselines, noise-floor caching for CI, JUnit and PR
  comments.
- **Distributed workers**, the SDK clients (`aws-sdk`, `exec:`) and the language adapters.
- **The cost model** and price tables.
- **Output formats:** HTML, Markdown, NDJSON (the event stream on stdout), CSV, HAR and OTLP
  export; `-o` is `text` or `json`.
- **The microscope transport** (oprobe's own HTTP pool) and the in-house cache layer
  (`FoyerLayer` replaces it).
- **Internationalisation of the CLI** (`--lang`); the app keeps its own catalog.
- **Profiles** (`profiles/*.toml`), shell completions and man pages in the packages; the MSI's
  `.oprobe` file association and `oprobe://` links.
- **The minisign claim:** the README no longer promises a signature check; the installers verify
  a minisign signature only for releases that embed a public key.
- Docs: `ci.md`, `cost.md`, `agents.md`, `metrics.md`, `tuning.md`, `workloads.md`,
  `reading-the-report.md` (what is still true moved into the new pages).

### Known limitations

- **Validated without cloud credentials.** Every finding and every `prove` verdict has been
  checked against the lab, a fault-injecting proxy, MinIO, fake-gcs-server and Azurite — not
  yet against AWS S3, R2, GCS or Azure. Results name the environment they were measured in.
- **Throttling on OpenDAL's default retry** (1 s backoff, no jitter): a concurrency cap removes
  the throttling but often leaves goodput unchanged, so `prove` reports *inconclusive* on
  goodput; use `--objective p99` to judge such changes on latency.
- **`bandwidth_cap` on reads:** OpenDAL 0.59's `ThrottleLayer` shapes writes only, so `prove`
  sizes the cap from the fitted limit but cannot show a read-side gain.
- **`key_limit`'s remedy** is a change to the key layout, not a layer; `prove` tests a client-side
  cap as a stand-in and says so.
- **Production profiles** (`oprobe.trace/6`) hold requests, not operations: each request counts
  as one operation, findings that need operations or load variation are not derived, and
  `prove` refuses them.
- **`client_bound`** fires when the scheduler lags more than 5 ms at p99; on a busy shared
  machine it can lead the brief. Latency findings whose delay dwarfs that lag are kept.

## v5.0.0 — 2026-11-22

Design [v5](docs/design/v5.md). The changes under **Changed** are breaking.

### Changed

- **Workspace split.** The repository is a cargo workspace of three crates:
  `oprobe-trace` (the embeddable instrumentation: `TraceLayer`, the fidelity and microscope
  transports, `ReqRow`/`OpRow`, the recorder and aggregates), `oprobe-core` (engine, analysis,
  model, store, tune, lab, report renderers and the service layer) and `oprobe` (the binary:
  CLI, MCP server, TUI/GUI). The `oprobe` library no longer exports the engine under its own
  modules for embedding; depend on `oprobe-trace` or `oprobe-core`.
- **Service layer.** `oprobe_core::app::App` is the only entry point of the front ends, with
  typed requests and results (plan, run, tune, verify, fix, compare, explain, analyze, doctor,
  sample, discover, targets, runs, baselines, cleanup). `RunRequest` is a `RunSpec` plus
  `Overrides`, safety (budgets, read-only, allow-write), a `ClientChoice` and an optional
  `WorkerSet`.
- **MCP runs in-process.** `doctor`, `probe_endpoint`, `cleanup`, `verify_finding` and
  `recommend_config` call `App` instead of spawning `oprobe` child processes; a read-only server
  passes `allow_write: false` instead of `OPROBE_MCP_READ_ONLY`.
- **Report `oprobe.report/5`.** New sections: `client` (what generated the load; the 4.x
  `client` CPU/RSS block moved to `self.process`), `self`, `schedule`, `workers`, `dimensions`,
  `sampling`, and `capacity.hypotheses` (today's in-flight ceiling is one `inflight_limit`
  hypothesis). `compare` and `verify` gain an optional two-sided `net` verdict.
- **Run file `oprobe.run/5`.** A zstd stream of framed blocks: `header` (spec and client),
  columnar `segment`s of op and request rows, per-second `interval`s and the `report` last,
  streamed to disk (never encoded whole in memory). 4.x run files are not read.
- **Whole-operation sampling.** Beyond the row budget an operation is kept with all its
  requests or not at all: a time-stratified reservoir plus every failed or throttled operation
  and the slowest 1 % (`report.sampling`).
- **Always-on recorder.** `oprobe_trace::layer(Config) -> (TraceLayer, Handle)` instruments any
  OpenDAL operator; requests outside an operation scope are recorded in every shard. The `otel`
  feature emits `http.client.request.duration` with `oprobe.stage` and `oprobe.api`.
- The compare gate moved from `report/diff.rs` to `analysis/gate.rs`; `report/view.rs` is the
  shared presentation model of the terminal, Markdown and HTML renderers. The lab is a module
  directory (`lab/`), the finding catalog lives in `analysis/catalog.rs`.
- **Smaller binary.** Release builds use one codegen unit, fat LTO and stripping. Rarely used
  OpenDAL services (azdls, azfile, swift, tos, webhdfs, upyun, http) are behind cargo features
  (`all-services`); the default build covers s3, gcs, azblob, oss, cos, obs, b2, webdav, fs and
  memory.

### CLI, automation and distribution

- **Brief output wraps** to the terminal width (100 columns off a terminal, `COLUMNS` when set)
  and never truncates; `next` commands are printed whole. Throttled requests show their share,
  and an SLA without a throttle objective says so instead of "SLA met" alone.
- **Help tiers.** `--help` lists the common flags of large commands and names the headings of
  the rest; `--help-all` lists every flag.
- **Flags with units.** One throttle flag, `--max-throttle-ratio` (the SLA of `run`, the budget of
  `tune` and `fix`; `tune --max-throttle-ratio` and `--max-throttle-rate` are merged into it);
  `--max-error-ratio`; compare `--max-p99-increase-pct`, `--max-p50-increase-pct`,
  `--max-throughput-drop-pct`, `--max-error-increase-pp`, `--max-request-amp-increase-pct`,
  `--max-read-amp-increase-pct`, `--max-cost-increase-pct`; `compat --min-score-pct`. The 4.x
  names remain as hidden aliases.
- **Dynamic completions.** `oprobe completions SHELL` completes target names, run ids, labels and
  finding codes live; `--static` prints the 4.x script.
- **简体中文.** `--lang zh-CN` (or `OPROBE_LANG`, `LC_ALL`, `LC_MESSAGES`, `LANG`) localizes the
  brief, the findings' cause and advice, and error labels and codes; the catalog
  (`oprobe_core::i18n`) is shared with the app. JSON output stays English.
- **watch exports.** OTLP metrics are one cumulative stream per session with a stable
  `service.instance.id` and the target (no run id per window), honour
  `OTEL_EXPORTER_OTLP_HEADERS` and are protobuf by default (`OTEL_EXPORTER_OTLP_PROTOCOL=http/json`
  for JSON). Prometheus series carry a `target` label, with `oprobe_build_info`,
  `oprobe_last_success_timestamp_seconds`, HELP text and `/healthz`.
- **GitHub Action.** `compare --strict` by default (`strict`), the data directory (noise floor,
  capacity history) cached per target across jobs (`cache-data`), and `repeat`, `calibrate`,
  `calibrate-repeat` inputs.
- **Verified updates.** `install.sh`, `install.ps1` and `oprobe self-update` check the GitHub
  build provenance (`gh attestation verify`) when the GitHub CLI is logged in, else a minisign
  signature over `SHA256SUMS` (`SHA256SUMS.minisig`, signed by the release when `MINISIGN_KEY`
  is set); `OPROBE_VERIFY` / `--verify auto|require|checksum`. The self-update result gains
  `verified_by`.
- **Supply chain and CI.** Every action is pinned by commit SHA with Dependabot updates, release
  jobs use no caches, `cargo deny` checks advisories, licenses, bans and sources (`deny.toml`;
  RUSTSEC-2023-0071 documented), pull requests test on Linux, macOS and Windows in debug with
  cargo-nextest with cancel-in-progress, and release builds and installers run on main and
  nightly only. crates.io publishes `oprobe-trace`, `oprobe-core`, `oprobe` in order.
- Text: "1 request" (not "1 requests"); watch prints SLO values in their unit.

### Added (stream SCALE: clients and distributed load, design v5 §7–§8)

- **`--client aws-sdk | exec:COMMAND`.** Drive any workload with aws-sdk-s3 (cargo feature
  `aws-sdk`, over oprobe's instrumented transport) or with an external adapter speaking the
  versioned oprobe client protocol `oprobe.client/1` (NDJSON over stdin/stdout, documented in
  [docs/clients.md](https://github.com/hxddh/oprobe/blob/v5.0.0/docs/clients.md); schemas `client-hello`, `client-hello-reply`,
  `client-op`, `client-result`). `report.client` names the client, its version and the stages
  it observes; findings that need other stages are suppressed with a warning. The client is
  never stored in a run's spec. Reference adapters in `adapters/`: `minimal` (Python standard
  library), `boto3`, `awscrt` and `go-sdk` (aws-sdk-go-v2 with `httptrace`).
- **`oprobe worker` and `--workers host:port[*WEIGHT],… | local:N`.** Token-authenticated
  workers (`oprobe.worker/1`, h2c or TLS); the coordinator checks versions and clock offsets,
  splits concurrency, rate, dataset and budgets, starts every worker at one instant, enforces
  the global budgets, fans out cancellation and merges the workers' snapshots and trace rows
  into one run with `report.workers[]` and `report.dimensions.workers[]`.

### Added (stream MEAS: measurement, design v5 §3)

- **Open-loop schedule stage.** Open-loop runs time every operation from its intended start: the
  `schedule` stage (intended → actual start) is part of its latency, and arrivals that never
  started before the window ended are counted as censored latencies in the tail ("≥" values)
  instead of being dropped. `report.schedule` gives `open_loop`, `arrivals`,
  `intended_ops_count`, `missed_arrivals_count`/`_ratio`, `schedule_stage` and
  `max_schedule_lag_ms`; a closed-loop run says `open_loop: false`.
- **Arrival shapes.** `--arrivals constant|poisson` and a time-varying rate with
  `--shape ramp:FROM..TO/D | step:R1,R2,…/D | diurnal[:MIN..MAX/PERIOD]`. Over `--workers` the
  shape is scaled by each worker's weight and every worker draws its own Poisson seed.
  A constant pace never bursts its backlog after a stall: late arrivals leave at most 4 at
  once, then at the nominal spacing, keeping their intended start (the lag shows in latency).
- **Log replay.** `--replay FILE` replays an S3 server access log, CloudTrail S3 data events or
  an oprobe run file, with `--speed N` and `--map-prefix FROM=TO` (repeatable); writes outside
  the run prefix are refused. Replays are split by key over local workers and refused for
  remote ones.
- **Self probe.** `report.self` measures the load generator: scheduling lag, Tokio runtime busy
  share, CPU after warmup, RSS, the observer's cost per operation and `client_saturated`.
- **Streamed run file.** Rows go to a writer thread that appends columnar segments while the
  run measures; past `trace.max_requests` every failed or throttled operation is kept and the
  rest thinned. A run keeps only a 100k-row sample in memory, so memory stays flat on long runs.
  The interval series adds p90, p99.9, max, in-flight, new connections, CPU, missed arrivals and
  heatmap columns, at 1 s for the first ten minutes and 10 s after.
- **Server signals.** Requests record the provider's error code (from the body, first 4 KiB),
  S3 `x-amz-id-2`, the bucket region, `Server-Timing` and the CDN cache status.
- **HTTP/2 stream waits.** The time a request waits for a stream slot (`stream_wait`, part of
  `queue`), flow-control stalls and the streams open on the connection; the microscope transport
  opens several HTTP/2 connections per host by stream budget.
- **Lab protocols and HTTP/2 preflight.** `lab://NAME?proto=h1|h2c|https|h2`; lab servers speak
  h2c by default, so `--http h2` works on `lab://` targets, and TLS variants trust the lab CA.
  With `--http h2` a target without HTTP/2 is refused up front (`net.http`, with a hint) instead
  of failing every request.
- **Kernel upload acknowledgement, min RTT, retransmits.** TCP samples keep the minimum RTT,
  unsent and unacknowledged bytes, lost segments, bytes acknowledged and pacing; every request
  samples its connection and carries the retransmits since the previous sample. On Linux,
  HTTP/1 uploads through the microscope transport poll `TCP_INFO` until the last byte is
  acknowledged: `upload` ends at the acknowledgement and `server` (TTFB) starts from it.
- **`report.dimensions`.** Where the traffic and the trouble are, from aggregates: prefixes,
  hot keys (at most 20, with their share of the throttling), size buckets and cold vs warm reads,
  each with its share, throttled and error ratios and p99.
- **Object size distributions.** `--object-size dist:lognormal(MEDIAN,SIGMA)` or
  `--object-size file:PATH` (one size per line) next to a fixed size.
- **Cold-start sample.** Warmup used to absorb every dial; `connections.cold_start` now reports
  the connections dialed at the start of warmup, their connect time and the latency of the
  operations that waited for them.
- **`schedule_backlog` finding.** More than 1 % of the open-loop arrivals never started: the
  tail holds censored values and throughput is what the client managed; capacity advice is
  suppressed.
- Distributed runs merge all of the above across workers (self, dimensions, the open-loop
  fields, timelines and cold starts).

### Added (stream ANA: analysis, design v5 §4)

- **Competing limit models.** Capacity fits in-flight, token-bucket rate, per-key,
  time-varying, queueing, bandwidth-cap and client-bound hypotheses and selects one by BIC
  (`capacity.hypotheses`); the 4.x ceiling and rate fields follow the selected hypothesis. The
  lab gains limit faults with one scenario each: `ratelimit`, `hotkey`, `bandwidth`, `queueing`
  (an M/M/4 queue), `timevarying` and `noisy` (random 5xx unrelated to the load).
  The in-flight ceiling's `lo..hi` bracket spans every plausible split plus one level of
  measurement error; `requests` is the likeliest split and the safe concurrency, tune and the
  in-flight advice apply their headroom to it (4.x used `lo`).
- **Advice per limit model.** The throttling finding's advice depends on the selected
  hypothesis (rate, per-key, time-varying, random, inconclusive); a queue, a bandwidth cap or a
  busy client no longer gets "add workers". Retried 5xx are reported even under throttling, and
  `cache_opportunity` needs real data or a stated skew.
- **Two-sided verify with an SPRT.** `verify` runs 2 to 8 interleaved pairs (`--repeat`) and
  stops a finding early once its sequential test decides, with bounded log-ratio and
  Wilson/Newcombe intervals; each finding gets a net verdict (`improved`, `regressed`,
  `traded`, `no_change`, `inconclusive`), the same one `compare` shows for those runs. A loss
  within a confirmed prediction (fewer streams under a bandwidth cap) never refutes a finding.
- **Three-tier gate.** Each compare check is `fail` (significantly worse and over the limit),
  `warn` (significantly worse within the limit) or `inconclusive` (not significant, but the
  interval reaches past the limit), next to `pass`/`improved`; the gate verdict gains `warned`
  and `inconclusive`. Intervals are log-ratio and pooled block bootstraps instead of
  t-intervals on two runs (run-level once either side has 3+ runs), `compare` reports a net
  verdict, and `compare --fail-on-warn` makes a warn fail the gate (exit 3).
- **Fix menu.** `fix` offers 2 to 4 Pareto options, each with goodput, p99, throttling, monthly
  cost and whether it was verified.
- **Tune 5.** A rate-limit hypothesis adds paced rate trials; Gaussian-process surrogates of
  goodput and the throttle ratio pick the next concurrency by constrained expected improvement;
  a measured configuration beyond the fixed headroom is accepted when the posterior risk of
  exceeding the throttle budget is at most `--risk` (default 0.05), and is then reported as
  `safe_concurrency`. Under a queue or a bandwidth cap the tuner prefers the lowest-latency
  configuration on the plateau. Trial goodput counts only the in-window operations that
  finished by the deadline, so a large closed loop no longer looks faster than it is.
- **Bill-grade cost.** Price tables gain storage classes (minimum billable size and duration,
  retrieval fees), egress tiers with free allowances, inter-region transfer and per-provider
  error billing; the cost spec takes `storage_class` (`standard`, `infrequent`, `archive`) and
  `inter_region_fraction`. The AWS egress note is corrected.
- **Deterministic lab queue.** The lab's queue runs on virtual time, so its capacity is exactly
  servers ÷ service time whatever the timer resolution; on Windows the lab asks for 1 ms timers.
  In-flight ceilings tolerate a client that counts in-flight requests one level off.

### Security and MCP (stream SEC, design v5 §5–§6)

- **Private store files.** On unix, store files are created `0600` and new directories `0700`.
- **Lab exposure.** `oprobe lab` refuses a non-loopback `--bind` unless `--allow-remote`, caps
  object bodies, the in-memory store and open connections (`--max-object-size`,
  `--max-store-size`, `--max-connections`) and times out slow request headers.
- **MCP default budgets.** Every run the MCP server starts is limited to 50 000 requests,
  2 minutes, $1 and 5 GiB unless the server's own flags say otherwise; a tool call can only
  lower them.
- **Credential-safe overrides.** A tool call may not point a configured or credentialed target
  elsewhere (endpoint, proxy, insecure, options, `set`): overrides are accepted only for
  unsigned requests and `memory://` / `lab://`, never towards link-local addresses.
  `probe_endpoint` refuses loopback, private, link-local and reserved addresses unless
  `--allow-private-endpoints`.
- **HTTP bearer token.** `oprobe mcp --http` requires a bearer token (a new one written to
  `<data>/mcp-token`, mode 0600, at every start, or `--token-file`), compared in constant time.
  A non-loopback bind needs `--http-allow-remote` and TLS (`--tls-cert`/`--tls-key`) or
  `--insecure-remote`; connections (`--max-connections`), bodies and header time are limited.
- **Untrusted text.** Object keys and service messages are escaped, bounded and kept in
  untrusted fields, out of the tools' text content.
- **No client or workers via MCP.** Tool arguments and `set` keys that name a load client or
  workers are refused (`safety.refused`), and so are replays of runs that used them.
- **Jobs.** `verify_finding` and `recommend_config` are jobs; `doctor`, probes, `sample` and
  `cleanup` continue as jobs after 5 s; `start_run` waits at most 5 s; `get_run` and
  `cancel_run` handle every job.
- **Compact responses.** `tools/list` shrinks from 442 KB to 51 KB (compact input schemas,
  output schemas that name their `oprobe://schema` resource); results are summaries by default
  (`detail=summary|full`; `include_report` is accepted as an alias of `full`), `list_runs`
  pages with a cursor and `oprobe://runs/{id}/slowest` pages five operations at a time.
- **New tools.** `ask_run` (questions about a stored run, answered from its report with cited
  fields, no model), `save_target` and `apply_profile` (both need `--allow-write`).
- **Next steps as MCP calls.** Every next step carries an MCP call, derived from its CLI command
  when it has none (`target add` → `save_target`, `fix` → `recommend_config`, `run --from` →
  `start_run`, …); capabilities report the default budgets and the bearer auth.

### App (terminal and native workbench, design v5 §9)

- **Target Health main screen.** The app opens on live tiles with sparklines of the last runs
  and the baseline band, the selected limit hypothesis, a primary Fix action with its
  prediction (opening the fix menu) and an annotated run history.
- **Per-operation waterfall.** Drill into one operation's requests (stages, retry backoff,
  critical path, compared with a typical operation) from the timeline, the heatmap brush or the
  waterfall command. The timeline opens on the p99 operations, slowest first.
- **One verdict source.** Compare and Verify show one net verdict through `App::compare`, with
  one vocabulary and colour scale for verdicts, severities and gate checks; a warn check reads
  "worse (within limit)", and a large change without significance is inconclusive, never
  "same". Test Connection runs a read-only doctor through `App`.
- **Honest terminal charts.** Gutters as wide as their labels, no grid in a terminal, x labels
  never overlapping, text swatches in legends.
- **Toasts** take the terminal's status row instead of drawing over content; terminal
  notifications use OSC 9 or the bell.
- **Provisional live findings** with a confidence while a run measures, an early-numbers flag
  and the tuner's ETA. Findings use the shared presentation with readable evidence labels, and
  CLI commands in advice become buttons.
- **`oprobe://` view links.** `oprobe://run/ID?view=…` (and `oprobe gui LINK`) opens the run and
  restores its view; the scheme and `.oprobe` files are registered by the Linux desktop entry,
  the macOS `Info.plist` and the Windows installer.
- **Markdown exports carry links** to the shown view.
- Also: keyboard heatmap brush and sort keys, a fuzzy command palette with shortcuts and the
  reason a command is disabled, per-platform shortcut labels and icons, app text size, and the
  language taken from the OS. `RunSummary` gains `preset` and `throttled_request_ratio`.
- **Live snapshots on the event bus.** `EventBus` gains an in-process live stream (engine
  snapshots, cleanup, the finished run, budget stops); it is never serialized, so CLI and MCP
  output are unchanged. The app's run job goes through `App::run`.

### Known limitations

- There are no `oprobe probe limits` or `oprobe probe semantics` commands.
- TCP info (RTT, retransmits, upload acknowledgement) is not collected on Windows; the upload
  acknowledgement is Linux only.
- `--workers` refuses `--repeat` and `--baseline`.
- `compare` does not flag runs made with different load clients (`report.client`).
- The app's tuner job still calls `engine::measure` directly instead of going through `App`.
- Plain `oprobe run` reports still count operations that started in the window and finish
  while draining after the deadline (the tuner's trials do not).
- The release binary (about 60 MB with the default features) misses the 45 MB target of design
  v5.
- Installers are still not signed with a developer certificate (see 4.0's notes).

## v4.0.2 — 2026-11-21

### Fixed

- MCP `recommend_config` with `from` and no `target` forwards the call's target overrides
  (`region`, `anonymous`, `endpoint`, `options`, …) to the `fix` it runs, as they were applied to
  the profile the server checked; they were dropped, so the measurement used the stored settings.

## v4.0.1 — 2026-11-21

A patch release for four review findings in 4.0.0; no breaking changes.

### Security

- **MCP `recommend_config` could write on a read-only server.** With `from` naming a run that used
  `--existing`, the call passed the read-only check, but the child `oprobe fix` was rebuilt from the call's own
  arguments: the `--existing` prefix (and any `set`) was lost and the child uploaded a generated dataset to the
  target, bypassing `--allow-write`. The child is now `fix --from RUN --set …` with every workload flag, so it
  measures exactly the checked workload, plus `--read-only` whenever the server would refuse the write;
  `verify_finding` passes `--read-only` the same way. As defense in depth, a read-only server sets
  `OPROBE_MCP_READ_ONLY=1` on its child commands, which then refuse any measurement that would write to a
  target outside their own process.
- **Secrets in target URLs were stored in reports and run files.** `spec.target_ref` kept the target as typed
  (`webdav://alice:hunter2@host/files`, `s3://bucket?secret_access_key=…`) and `spec.profile.target.url` kept
  secret query parameters; both reached run files, `--json` reports, replay commands, next steps and the
  dry-run plan. Passwords and query parameters with secret names (`secret`, `key`, `token`, `password`,
  `credential`, `sas`, `sig`, `signature`, `session`, …) are now removed; replay resolves credentials again from
  the environment, profile or configured target. Run files written by 4.0.0 keep what they stored.
- **`verify`, `fix` and `baseline calibrate` budgets now cover the whole command.** Each replay of `verify
  --repeat N` used a fresh guard, so the command could spend N × (findings + 1) times `--max-requests`,
  `--max-bytes`, `--max-cost-usd` and `--max-duration` (MCP `verify_finding` forwards the server's budgets into
  the same path). One guard is now shared by every replay, by the diagnosis, tuning and verification of
  `fix`, and by the runs of `baseline calibrate`, as with `run --repeat`; the first breach stops the command
  (exit 4, the partial run kept).

### Fixed

- `oprobe fix --no-save` failed at verification (it replayed the diagnosis run by id, and that run was never
  stored); the replays now start from the diagnosis in memory.
- `oprobe baseline calibrate --from RUN --dry-run` replayed the run instead of printing the plan.

### Added

- `oprobe fix --from RUNREF [--set KEY=VALUE]…`: fix the workload of a stored run (resolved like `run --from`).

## v4.0.0 — 2026-11-21

oprobe 4: *one model, one spec, one loop* ([docs/design/v4.md](docs/design/v4.md)). Nothing reads 3.x run files
or reports.

### Foundation (breaking)

- **Run spec**: every report carries `spec` (`oprobe.report/4`; run files are `oprobe.run/4`) — the resolved
  profile with secrets redacted, the target as given, the seed, `--existing` and the label. `oprobe run --from
  RUNREF [--set key=value]… [flags]` replays a stored run (credentials are resolved again from the target);
  `--set` changes any knob, workload, SLA or cost setting (`oprobe run … --set seed=7` works without
  `--from` too). `config::defaults()` is the single source of built-in defaults.
- **Findings' verifications** are `{ description, overrides, command, rerun, replay, expect }`: `command` is
  `oprobe verify RUN CODE`, `rerun` is `oprobe run --from RUN --set …`, `replay` is a self-contained
  `oprobe run TARGET …` that keeps every setting of the run (warmup, seed, mix, dataset, endpoint, region, HTTP
  version, tail-cut, rate, `--existing`, `--max-ops`) and changes only the advice. The 3.x `command: Option`
  is gone.
- **Capacity model**: `report.capacity` estimates the in-flight ceiling from the request rows (the levels at
  which throttling starts, bracketed `lo`/`hi`), the per-operation request shape (a parquet scan peaks at 4
  requests in flight, not its 6.3 requests per op), `safe_concurrency(headroom)` and `predict(concurrency)`.
  Ceilings are remembered per target and workload in `<data>/capacity.toml`.
- **Target identity**: `--allow-target` on the MCP server compares normalised identities (scheme and bucket
  case, default ports, `.`/`..`/`//`, trailing `/`, query) on whole path segments; `..` above a root and
  `existing` prefixes that leave the allowed target are refused (fixes the `fs://…/allowed/../other` bypass).
- **Typed input**: one parser per input kind (`src/input.rs`) behind clap, the profile and config loaders and
  MCP, with "did you mean" hints. Bad input is never `internal` any more, and the CLI and MCP give the same
  code; the message-matching classifier is gone. Provider aliases (`aws`, `gcp`, `google`, `azure`, `r2`, `b2`,
  `oss`, `aliyun`) are accepted. Missing credentials are `auth.no_credentials`. New code
  `usage.unknown_baseline`. `--object-size 0`, `--endpoint ftp://…`, a missing `--config` file, negative
  durations and amounts and `target add` of an existing name (now needs `--replace`) are refused; `top -o json`
  and `gui` without the feature exit 2 with a JSON error.
- **Envelope**: every JSON command result carries `next` (`{ title, cli, mcp: { tool, arguments } }`) and
  `warnings` (including the command's stderr warnings). Doctor's `next_command`, explain's `next_commands`,
  the demo's `next_commands` and MCP `get_run`'s prose `next` moved onto it. Schemas are under
  `…/schemas/4/`; new schemas: `run-spec`, `overrides`, `capacity`, `capacity-history`, `verification`,
  `recommendation`, `next-step`, `target-id`, `defaults`.
- **Lab**: new `lab://parquet` scenario (503 above 24 in flight, no stragglers) for the parquet preset.

### Analysis: one capacity model behind every recommendation

- Every finding that mentions concurrency (`throttling`, `server_latency`, `small_objects`, `client_queue`)
  and `simulate::concurrency` ask the capacity model. Following a suggestion never pushes past the measured
  ceiling, and runs under the ceiling reuse the remembered one. The 3.x ping-pong is gone: on a parquet lab
  the findings used to say 16 → 3 → 6 → 12, and on small files "try 44" after a clean run at 22. Each
  verification's `expect` holds the predicted goodput with its interval and the throttle budget.
- **Throttling is defined once**: `summary.throttled_request_ratio` (throttled responses / requests, used by
  the SLA) and `summary.throttled_op_ratio`. `throttle_ratio` is gone, and `sla.max_throttle_ratio` is now
  `max_throttled_request_ratio`.
- **Throughput counts successful operations** (`summary.ops_per_s`; `attempted_ops_per_s` counts all).
- **Invalid runs** (no ops, errors above the SLA, a window shorter than the warmup) and **interrupted runs**
  keep only correctness findings plus `run_invalid` / `run_partial`. `report.run.partial` records why, the
  warning is in `report.warnings`, and partial or invalid runs are refused as baselines.
- **Tail-cut**:
  - The simulator uses the layer's own rule (p95 × 1.5, at least 50 ms, 32 samples). A run with tail-cut on
    is never told to try it.
  - OpenDAL 0.59's tail-cut layer is blind for its first 10 s. Runs show how long it was blind, and the
    suggestion lengthens short runs.
- **Error advice per class**: credential advice only for authentication failures. Retry-exhausted 429/503
  failures fold into throttling.
- **Tail attribution** ignores the kernel RTT when it tracks server delay (loopback, correlated with TTFB).
  The straggler API is chosen stably.
- **Units**: integer `_count` fields everywhere; fractions renamed `_mean`. One percentile estimator.
- **Readable evidence**: the HTML, Markdown and text evidence uses human labels, units and percentages.
  Every bill that includes storage says "(assumed 1 TiB stored — set --stored-gib)".

### Tune, verify, fix, compare

- **`oprobe verify RUN [CODE|--all] [--repeat N]`** replays the run interleaved (A B A B…) with each
  finding's change. It compares the sides and answers `confirmed`, `refuted` or `inconclusive` per
  expectation, with numbers. Outcomes calibrate the capacity history (`capacity::calibrate`).
- **`oprobe fix TARGET`** runs diagnose → tune → verify. It returns a `Recommendation` (knobs, profile
  TOML, Rust snippet, deltas, `confirmed`) and a profile patch (`--write-profile`).
- **Tune**:
  - **Objective and budget**: optimises **goodput** by default (`p99`, `cost` and `balanced` also exist)
    within a throttle budget (`--max-throttle-ratio`, default 0.1 % of requests). Throttling within the
    budget no longer stops the ceiling search.
  - **Search**: bisection repeats trials at the boundary (`--boundary-repeats`). The margin
    (`--headroom-ratio`, default 15 %) is counted in requests in flight and re-checked after io and range
    steps.
  - **Reporting**: an infeasible baseline is reported as such, and deltas are taken against the best
    feasible trial. There is no more "−61 % vs baseline" headline. The cost of the margin and the Pareto
    front are shown.
  - **Recommendation**: in the tune JSON and in MCP `start_tune`. The emitted profile has no environment
    defaults such as `proxy`.
- **Compare**:
  - **Identity**: a verdict needs the same target identity and workload; otherwise `comparable: false`
    with the reasons (exit 2 under `--strict`).
  - **Statistics**: one run per side uses a block bootstrap over time windows, labelled low confidence.
    Several runs use a Welch interval over per-run values.
  - **Noise floor**: `<data>/noise.toml`, filled by `oprobe baseline calibrate` and verify's A/A pairs.
  - **Count floors**: 0 → 1 throttled response no longer fails the gate.
  - **Gate checks**: `{ id, unit, baseline, candidate, delta_ratio, limit_ratio, verdict }`.
  - **Text output**: compare sizes its columns to their content and summarises long values.

### CLI and agents

- **Next steps everywhere**: every command result has `next` steps with both a CLI command and an MCP call.
  MCP `start_run` and `plan_run` take `from` and `set`.
- **New MCP tools**: `discover_targets`, `sample_objects` (read-only), `verify_finding`, `recommend_config`.
  New CLI commands: `oprobe target discover` (credential sources by name only) and `oprobe target sample`.
  `oprobe` with no command and no terminal prints the discovery.
- **Formats per command**: `-o` outside a command's formats is refused before it runs, and `capabilities`
  lists them. `completions` and `man` are text only.
- **Schema rules**: the unit and enum rules are checked on every schema.
- **MCP server**: `lab://` targets are allowed on a read-only server. Unknown tool arguments are refused with
  a hint. `get_run` turns `done` atomically with its last event, and progress never overshoots.
- **Safer defaults**:
  - `doctor` is read-only (`--write-probe` opts in) and names what it sees, with an object sample.
  - `cleanup` is a dry run unless `--yes`.
  - Doctor failures show a summarised cause (raw output with `-v`).
- **Better errors**:
  - Backend-config errors name `--option KEY=VALUE` and `[targets.NAME.options]`.
  - New codes `net.rate_limited` and `net.http` for non-storage APIs.
  - A warmup at least as long as the duration warns.

### App (GUI and terminal)

- **Add Target**: discovery of credential sources, a read-only connection test, and save to the config.
- **Re-run and Verify** are built from the run's spec, so nothing is dropped. Verify opens Compare with a
  confirmed/refuted/inconclusive banner.
- **One set of defaults** with the CLI (tune, demo, cost).
- **Run list**:
  - The list is live: it watches the runs directory and refreshes on focus.
  - Partial runs get a badge; a budget stop is not a failure.
  - "Stopped during warmup" replaces 0 ops/s.
  - Search, "show more" and a truthful run count.
- **Compare**: defaults to the same target and workload. Selectors never wrap, and a non-comparable pair says
  why.
- **Commands**: one context-aware command list drives the menu bar and the ⌘K palette (Verify, Open Run,
  Switch Target, Add Target, Fix, Export…).
- **Keyboard**: letter shortcuts never fire in text fields, sheets focus their first field, Enter submits,
  Esc cancels, and the terminal shows a focus band.
- **Accessibility**: every button has an accessible label, and charts carry a text summary and a data table.
- **Overview**: a THROTTLED tile, and the cost tile labels assumed storage.
- **Layout**: works at 120×30 and in narrow windows (tiles wrap, Cost becomes one column, numbers never
  wrap).
- **Live views**:
  - live series append instead of rebuilding;
  - chart paths are cached;
  - the crosshair has its own layer;
  - live phases (preparing, warmup, measuring, cleanup) are shown;
  - the tune result offers Run, Verify, Save profile and Rust snippet.

### Onboarding and distribution

- **The demo is the lab**: `oprobe demo` runs exactly `oprobe run lab://demo`. Every lab scenario has a
  default workload, applied when no flag says otherwise: `lab://demo` is small-files (64 objects of 64 KiB),
  32 workers, 0.5 s warmup, 8 s; `lab://parquet` is the parquet preset with 8 workers. The demo used to
  measure `s3://demo/bench`, so `compare` flagged a different target, dataset and mix against
  `run lab://demo`; now both have the same target identity and `workload_hash`.
- **Demo next steps are replays**: `oprobe run --from RUN`, `oprobe compare RUN last`, `oprobe verify RUN
  CODE` and `oprobe run --from RUN s3://YOUR-BUCKET/…`, with MCP calls in the envelope's `next`.
- **Installers**: `install.sh` and `install.ps1` resolve the latest version from the release's `latest.json`,
  then the `releases/latest` redirect, then the API, so GitHub's anonymous API rate limit no longer breaks an
  install. `OPROBE_VERSION` still pins a version; `OPROBE_RELEASE_BASE` points them at a mirror. `install.sh`
  offers to add the install directory to the zsh, bash or fish rc file (asking on `/dev/tty` when piped;
  `OPROBE_MODIFY_PATH=1|0` decides without asking). `install.ps1` updates the user `PATH` and the current
  session's `$env:Path`, uses `-UseBasicParsing` and a silent progress bar, and detects ARM64 Windows from the
  OS (it used to install the x64 build under emulation).
- **`self-update`** reads `latest.json` before the API (`release_source: latest_json | api` in its output)
  and verifies against the SHA-256 listed there. A GitHub 403 or 429 is `net.unreachable` (retryable), never
  `storage.throttled`; an unknown `--to` tag is `usage.invalid_argument`.
- **Package managers are published**: on each stable release, the workflow updates the Homebrew tap
  `hxddh/homebrew-tap` (`brew install hxddh/tap/oprobe`, `brew install --cask hxddh/tap/oprobe-app`), the
  Scoop bucket `hxddh/scoop-bucket` and opens a winget-pkgs pull request, each when its secret (`TAP_TOKEN`,
  `SCOOP_TOKEN`, `WINGET_TOKEN`) exists and skipped with a warning otherwise. The formula and the cask have
  distinct names (`oprobe`, `oprobe-app`; the cask asset is now `oprobe-app.rb`). The Scoop manifest's
  `checkver` reads `latest.json` and its `autoupdate` takes hashes from `SHA256SUMS`. The docs say that
  `winget install hxddh.oprobe` works only once the package is accepted.
- **GitHub Action** (`uses: hxddh/oprobe@v4`): `version` defaults to `latest-major`, the newest release of
  the action's major version (`latest` and exact versions still work). The pull-request comment step has
  `continue-on-error` (fork PRs get a read-only token), a named baseline is saved to the cache only when it
  is replaced, and `args` / `compare-args` are split with shell quoting but never evaluated.
- **CI** runs both installers against a local fake release (`scripts/test-install.sh`,
  `scripts/test-install.ps1`), shellchecks the scripts strictly and checks the generated manifests.

## v3.0.0 — 2026-11-20

oprobe 3 turns a benchmark into a product: **see every request your object-storage client makes, know where
the time and money go and what to change — usable by people, CI and agents alike.** Every run is saved as one
`.oprobe` file; every command has typed output (`-o text|json|ndjson|md|html`), stable error codes and exit
codes. Nothing reads 2.x reports or flags.

### Highlights

- **`oprobe demo`**: zero flags. A local lab with throttling above a concurrency limit and 1-in-40 stragglers,
  measured and narrated step by step, ending with how to measure your own bucket.
- **Guided first run**: `oprobe run` with no target in a terminal offers the demo, the AWS profiles, Google
  application default credentials and Azure storage variables found on the machine, or a URL, and saves the
  choice as a named target.
- **Answers first**: the default output of `oprobe run` is at most 20 lines in 80 columns — verdict, top
  findings (cause · evidence · advice · next command), key numbers, and `oprobe runs show <id>` for the rest.
- **Findings with evidence**: every diagnosis item has a stable code, severity, one-sentence cause, evidence
  (numbers, operation ids, time windows), advice and, where possible, a command that verifies it;
  `oprobe explain` narrates a run or documents a finding code.
- **Single-file HTML report** (`-o html` on `run`, `runs show`, `compare`): SVG charts with real axes (log
  latency), a latency heatmap, stage shares, findings with evidence, waterfalls of the slowest operations,
  a filterable and sortable table of every request, network, cost and environment; light/dark, offline,
  printable.
- **Run history**: `oprobe runs list|show|open|export|rm|gc`; RUNREFs (`last`, `last~1`, an id prefix, a
  label or a file); `runs show --detail`; `runs open` opens the run in the app.
- **Named targets and configuration**: `oprobe target add|list|show|rm|test` (`add` runs a quick read-only
  check; `test` is the full doctor), `credentials = "env" | "profile:NAME" | "anonymous"`,
  `oprobe config show` (every value with its source), `config path`, `config edit`.
- **Regression gate on runs**: `oprobe compare A B` and named baselines (`oprobe baseline`, `run --baseline`),
  configuration differences listed first, Markdown and JUnit.
- **GitHub Action** (`uses: hxddh/oprobe@v3`): installs a pinned, checksum-verified release, runs, compares
  against a file or a named baseline kept in the Actions cache, uploads the run and the HTML report, keeps one
  sticky PR comment up to date, and fails on regression.
- **Agents**: `oprobe mcp` (MCP server; read-only unless `--allow-write`, budgets, progress notifications),
  `oprobe schema` (JSON Schemas of every output), `oprobe capabilities`, the `oprobe.events/1` NDJSON stream,
  `AGENTS.md`, `llms.txt` / `llms-full.txt`.
- **Deeper observability**: storage API attribution from OpenDAL's `ServiceOperation`, retry backoff as its own
  share, per-remote-address and per-connection splits, TCP_INFO and TLS handshake kind/resumption, outlier
  attribution (`oprobe analyze`), capability-aware workloads (batched delete, copy, paged list, presigned GET,
  footer reads, conditional writes), `oprobe compat` (correctness and compatibility score), `oprobe watch`
  (continuous probing, SLOs, Prometheus, OTLP), `oprobe matrix` (rank targets or knob values), exports to
  CSV, HAR, NDJSON and OTLP.
- **Safety rails**: `--dry-run` plans (requests, bytes, objects, USD, ETA), budgets (`--max-cost-usd`,
  `--max-requests`, `--max-bytes`, `--max-duration`, exit 4), `--read-only` (exit 5), writes confined to
  `<prefix>/oprobe-run-<id>/`, `--idempotency-key`, preflight before any write.
- **The app**: three panes (targets · runs · baselines / run inspector / selected request), virtualized request
  table, timeline waterfall with zoom and brush, compare, command palette; `oprobe gui --open FILE`.
- **`oprobe self-update`**: checks GitHub Releases, verifies the download against `SHA256SUMS`, checks that
  the new binary runs and replaces it atomically; refuses package-managed installs with their update command.

### Distribution

- New release assets: Homebrew cask for the `.dmg` (`oprobe.cask.rb`) next to the formula, winget manifests
  (`winget-manifests.zip`), `latest.json` (every asset's URL, size and SHA-256); `SHA256SUMS` covers every
  asset including the manifests.
- **SLSA build provenance** for every asset (`gh attestation verify FILE --repo hxddh/oprobe`).
- Packages remain unsigned (no certificates); [docs/install.md](docs/install.md#first-open-of-unsigned-packages)
  explains the first open on macOS 15+ (Privacy & Security → Open Anyway) and Windows SmartScreen.
- Release notes come from this file; CI checks the docs (links, the generated CLI reference in `docs/cli.md`).

### Documentation

Rewritten around the product: getting started, concepts (targets, runs, baselines, findings), the generated
CLI reference, workloads and presets, metrics (with `--microscope`-only values marked), reading the report,
CI, install, troubleshooting, privacy (no telemetry), releasing; README in English and Chinese.

### Shared core (breaking)

- **Run store**: every `oprobe run` / `tune` is saved as `runs/<id>.oprobe` (zstd NDJSON,
  `oprobe.run/3`: report + one row per operation and per request) with a `.json` sidecar under
  `$OPROBE_DATA_DIR`. `oprobe runs list|show|rm|gc`; runs are referenced by id, id prefix,
  `last`, `last~N`, label or path.
- **Report v3** (`oprobe.report/3`): `run`, `verdict {level, headline, exit_code}`,
  `knobs {requested, effective}`, `stages`, `network`, `heatmap`, `findings`, `slowest` (op ids);
  every numeric field carries its unit. 2.x reports are not read.
- **Global `-o/--output text|json|ndjson|md|html`**; stdout carries
  only the result; `-o ndjson` streams `oprobe.events/1`. `--format` is gone; `--option` lost
  its `-o` short flag.
- **Classified errors**: stable codes (`net.timeout`, `safety.read_only`, …) and exit codes
  (2 usage/config, 3 gate, 4 budget, 5 safety refusal, 130 interrupted); JSON errors in machine
  modes.
- **Safety rails**: `--dry-run` plan, `--read-only` (implied by `--existing`), budgets
  (`--max-requests`, `--max-bytes`, `--max-duration`, `--max-cost-usd`), `--idempotency-key`,
  and a preflight (list + tiny write/delete) before any data is uploaded.
- **Config file** with named targets (`oprobe target add|list|show|rm`, `oprobe config
  show|path`); `oprobe run prod` uses target `prod`.
- **Command tree**: `diff` is now `compare A B` (RUNREFs or files); `report` is gone (`runs show
  FILE`), `init` is hidden; `demo`, `analyze`, `explain`, `watch`, `matrix`, `compat`,
  `baseline`, `mcp`, `schema`, `capabilities`, `self-update`, `runs open|export`, `target test`
  and `config edit` are new (see Highlights).

### Trust fixes (breaking)

Places where oprobe could lead to a wrong conclusion. Reports use schema `oprobe.report/3`;
v2 reports are rejected by `oprobe compare`.

- **Retry backoff** is its own `retry` share of operation time (terminal, Markdown, app, JSON
  `stages.shares.retry_pct`, `verbs[].breakdown.retry_ms`) instead of being counted as client time; throttling
  headlines state how much time went to waiting for retries.
- **SLA gate**: `oprobe run` / `tune` exit 3 when the run breaches its SLA — by default more
  than 1% failed operations or any corrupted data (`verdict.sla` in the report). `--sla none`
  disables it; `--max-error-rate`, `--max-throttle-rate`, `--p99-ms` override it. `--fail-on`
  still adds a gate on diagnosis findings.
- **Write chunks**: the tuner no longer recommends chunk sizes OpenDAL raises to the service's
  minimum part size (5 MiB on S3); reports carry `knobs.requested` and `knobs.effective`.
- **Generated code** reproduces exactly the measured layers and parameters (one `ClientStack`
  feeds both the measured operator and `--emit-rust` / `--emit-python`).
- **`r2://` / `minio://`** require `account_id` / an endpoint instead of silently using AWS.
- **Cost** is projected only for recognised cloud endpoints or an explicit price table;
  otherwise `cost.applicable = false` and the section is hidden.
- **Tuner** narrative states the throttling ceiling once, explains the safety margin with
  numbers, no longer writes `scheme = ""`, and prints its report (trial table included) on stdout.
- **`oprobe compare`** (formerly `diff`) lists every configuration change; a different workload or target fails the
  gate (`--allow-workload-change`, `--allow-target-change`), knob changes are an A/B note.
- **Default transport** keeps OpenDAL's reqwest defaults (no connect timeout, unlimited idle
  connections per host).
- **Unknown `[target.options]` keys** are rejected with a "did you mean" suggestion.
- **`--anonymous`** sets each service's own unsigned-access options; unsupported services error.
- **Error classes** cover every OpenDAL error kind; timeouts and connect failures are detected
  from typed errors, not message text.
- Docs: the TLS cipher suite is only reported with `--microscope`.

## v2.0.1 — 2026-11-20

Fixes from a full review of 2.0.0. Some numbers change: see *Measurement changes*.

### Fixed

- **macOS: `oprobe.app` could not be opened.** Opened from Finder, the app runs `oprobe` without arguments, which
  was rejected as a usage error before the window opened. A bare `oprobe` now opens the workbench; CI and the release
  build open the app through LaunchServices and fail unless it stays up. The install docs describe the macOS 15
  Gatekeeper flow for unsigned apps (*Privacy & Security → Open Anyway*).
- **Secrets**: customer-supplied encryption keys (`server_side_encryption_customer_key`, `encryption_key`, …) and
  credentials embedded in URLs (`webdav://user:pass@host`) were written to reports, diff summaries and generated
  profiles. They are now redacted/stripped; `webdav://user:pass@…` maps to `username`/`password` options.
- **Generated profiles** (`tune --emit-profile`) rewrote `${AWS_ACCESS_KEY_ID}` as `${ACCESS_KEY_ID}` and failed to
  load; existing `${VAR}` references are kept, and defaulted or URL-derived options are no longer written.
- **Test data left behind** when the dataset upload failed or was stopped, or when the workbench was quit mid-run:
  uploads observe Stop/Ctrl-C and clean up; quitting the workbench stops the run and cleans up first (up to 30 s).
  An unreachable endpoint (connection refused, DNS) fails in seconds instead of retrying for a minute.
- **False throttling**: "429"/"503" (and "401"/"403" for hints) were matched anywhere in error text, including object
  names such as `obj-00429.bin`; classification now uses the error kind and the HTTP status. New hint for missing
  credentials.
- **Diff gate**: an operation missing from the candidate now fails; throttling is gated on its rate, not only when the
  baseline had none; new open-loop *missed %* check; warnings for closed- vs open-loop, different durations and
  interrupted runs.
- **Tuner**: never recommends a trial that failed the SLA when one passed (and says so when none did); throughput
  counts successful operations only; `Retry-After` is honoured per trial (capped at 60 s, also with `--cooldown 0`)
  and cooldowns can be stopped; `--max-trials` is respected; the cache trial no longer wins just because the test
  dataset fits in the cache.
- **Open-loop writes** could hit the same key concurrently and report false corruption.
- **Workbench**: *Apply to workbench* could apply the last run's settings instead of the tuner's winner; an engine
  panic left the UI stuck in *Running*; keyboard focus was lost when Run turned into Stop; window start-up errors
  were silently ignored (they are now reported and logged); defaults now match the CLI (preset `parquet`, tuner
  5 s trials, 24 trials); inputs have accessibility labels; terminal saves never overwrite files.
- **CLI**: an interrupted `run`/`tune` exits 130 and its report is marked `"interrupted": true`; `oprobe top`
  without a terminal and `oprobe gui` without a display exit 2 with a hint; `-c 0` and `--max-trials 1` are
  rejected; `top`/`gui` accept `--proxy`, `--ca-cert`, `--region` and honour `--color`/`NO_COLOR`.
- A cache-layer bug served the head of an object for a suffix (footer) read and could panic on out-of-bounds ranges.
- Release assets no longer include the WiX debug database.

### Measurement changes

- **Throughput** (ops/s, MiB/s, achieved rate) no longer includes the time spent draining in-flight operations
  after the window closed; values are higher than 2.0.0 reported, and correct.
- **Open loop** queues arrivals that find all workers busy and measures them from their intended start, so
  saturation shows in p50/p99 (2.0.0 dropped them); `missed` counts arrivals that never started.
- **Uploads** with the default client: `send` now ends near the last byte written (it ended before the first), so
  upload time moves from `ttfb` to `send`.
- Requests cut off by a timeout or tail-cut are recorded (with a `cancelled` error) instead of disappearing;
  bodies dropped short of their length are marked.
- Cold/warm: connections are identified by local *and* remote address; a dial that finished before the request was
  ready (hyper's pool race) is counted but not charged to that request; failed sends are not counted as warm.
- Time before your code first reads a response body is *stall*, not *transfer*.
- `Retry-After` in HTTP-date form is understood. Probe clock skew lost a +0.5 s bias.
- Cost: storage and egress are billed per GiB (2.0.0 overstated them by ~7 %); requests that never left the client
  are not billed. GCS batch requests are classified as deletes.
- Long runs use bounded memory (per-second statistics no longer keep a histogram each).

## v2.0.0 — 2026-11-19

oprobe 2.0 is a rewrite. Everything reported is measured per request; nothing is estimated.

### Highlights

- **Request-level white-box tracing of the client you deploy.** Requests go through OpenDAL's own
  HTTP client (reqwest + rustls/aws-lc-rs, configured as OpenDAL builds it), observed through
  reqwest's extension points: every backend request records DNS, TCP, proxy and TLS (when it
  dialed), request send, TTFB, transfer and consumer stall, plus status, connection, cold/warm, bytes
  and the provider's request ID. `--microscope` switches to oprobe's own instrumented connection pool
  for pool queueing, per-dial proxy timing and HTTP/2 stream detail. The observer effect is measured
  (`examples/overhead.rs`) and documented.
- **Exact attribution through OpenDAL.** A scope follows each logical operation through OpenDAL's
  layers and into its background tasks (concurrent readers/writers), and each operation's wall time
  is split exactly into queue / cache / backend / client by sweeping its request intervals.
- **Slow-operation forensics.** The slowest operations keep their full request timelines and
  async call stacks (await-tree, including OpenDAL sub-tasks) captured while in flight.
- **Realistic workloads.** Presets `parquet`, `checkpoint`, `small-files`, `metadata`, `mixed`;
  custom weighted mixes; zipf/hot-set key popularity; closed-loop concurrency or open-loop arrival
  rate without coordinated omission; warmup and time windows; `--existing <prefix>` read-only
  benchmarks of your own data; persisted datasets.
- **Throttle-aware tuner.** Concurrency ladder with 429/503 ceiling detection, binary refinement,
  Retry-After-aware cooldown and an 80 % safety margin, then coordinate descent over the knobs that
  matter for the workload, and a confirmation run. Emits a profile and Rust/Python OpenDAL code.
- **Measured cost model.** Requests are classified by real API (S3, GCS, Azure, B2 conventions)
  into billing classes; usage per operation is projected to your monthly volume for six providers,
  with waste from failed/throttled requests and over-reads and savings from caching.
- **CI gate.** `oprobe diff` checks throughput, per-operation p50/p99 (with noise floors and
  minimum samples), errors, new throttling, integrity, request/read amplification and cost; outputs
  terminal, Markdown, JSON and JUnit; exits 3 on regression.
- **New commands.** `top` / `gui` (the workbench in a terminal / window), `doctor` (step-by-step checks with fixes),
  `probe` (handshake microscope with clock-skew check), `lab` (S3-compatible fault-injection
  server), `cleanup` (sweeper with dry-run), `init`, `report`, `completions`, man pages.
- **Egress proxy support.** `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` with HTTP CONNECT
  tunnelling (incl. basic auth) and timed proxy handshakes; `--proxy` / `--proxy none`.
- **Workbench in a window and in the terminal.** `oprobe` with no arguments opens an interactive workbench built
  with Native Shell (NSH): a native window on macOS (AppKit), Windows (Win32/Direct2D) and Linux (GTK 4), or the same
  UI in any terminal (`oprobe top`, also over SSH). Live charts, time breakdown, per-API tables, cost, the slowest
  operation as a request waterfall, the tuner with apply/save, and the endpoint probe. One binary: macOS and Windows
  builds include the window (`.dmg`, `.msi` with a Start menu entry); Linux server packages stay headless and a
  `oprobe-desktop` tarball carries the window.
- **Two-layer architecture.** OpenDAL and its own HTTP client for measurement; NSH for every UI.
- **Native distribution.** Release archives named `oprobe-<version>-<target>`, static Linux musl
  builds, `.deb`, `.rpm`, macOS universal binary and `.pkg`, Windows `.msi` (x64 and ARM64),
  Homebrew formula, Scoop manifest, checksum-verifying install scripts, `SHA256SUMS`.

### Known limitations

- The macOS `.pkg`/`.dmg` and Windows `.msi` are not yet signed with a developer certificate: macOS asks to confirm
  on first open, Windows SmartScreen asks to *Run anyway* (see `docs/install.md`). Homebrew, Scoop, the install
  scripts and `cargo install` are unaffected.
- With OpenDAL's own client (the default), time spent waiting inside reqwest's pool for an idle connection is part
  of TTFB, proxy CONNECT is reported together with TCP to the proxy, and wire bytes and the TLS cipher are not
  available; `--microscope` measures all of these with oprobe's own pool.
- Observer effect: on tens of thousands of tiny requests per second over loopback, tracing costs up to ~10–18 %
  throughput; on real storage latencies it is negligible (`examples/overhead.rs`, `docs/metrics.md`).

### Breaking changes

- Report schema is now `oprobe.report/v2`; v1 reports are rejected by `oprobe diff` with a clear
  message. Re-create baselines with v2.
- Profile format changed (`[target] url`, `[workload] preset/duration/warmup/mix/dataset`,
  `[knobs]`, `[sla]`, `[cost]`). See `docs/workloads.md` and `oprobe init`.
- `serve-s3` is now `lab` (alias kept), `net-probe` is now `probe`, `tui` is now `top` (alias kept).
- Exit code for gate failures is 3 (was 2).

### Fixed (from v1)

- Secrets passed via environment variables were copied into profiles and JSON reports; option
  values with secret-like keys are now always redacted and emitted profiles use `${ENV}`.
- Stage timings were global sums divided by request counts; HTTP/2 TTFB used a connection-wide
  write timestamp; the "multipart merge" stage timed the entire upload for single PUTs; tracing was
  lost inside OpenDAL's concurrent sub-tasks. All replaced by per-request measurement.
- The auto-tuner fabricated synthetic "throttled" trials on errors; trials are now always real.
- Cost projections relied on invented scaling factors; now based on measured per-op usage.
- Release artifact names embedded the `v` tag prefix and lacked native packages.

## v1.0.0

- Pure-Rust CLI (`run`, `tune`, `diff`, `net-probe`, `tui`, `serve-s3`) on Apache OpenDAL 0.59.

## v0.1.0

- Initial prototype.
