# 4.0 review evidence (input to docs/design/v4.md §13)

Findings of the 3.0.0 deep review, with file:line evidence as of commit 1c833b1. The IDs match
the v4.md checklist. This is a working document for the 4.0 streams and is deleted once 4.0 ships.

## Analysis / tune / compare (streams A, T)

- **VER-2 / AN-1:** there is no shared capacity model.
  - The throttling finding uses Little's law on mean latency, divides by requests per op and keeps
    no margin (`analysis/findings.rs:377-441`).
  - Tune uses 80 % of the lowest throttled concurrency (`tune/mod.rs:336`).
  - `server_latency` and `small_objects` always suggest doubling through `simulate::concurrency`
    (`simulate.rs:271`), and every caller passes `ceiling=None` (`findings.rs:1030,1218`).
  - The parquet lab ping-pongs: c=16 → "3", c=3 → "6", c=6 → "12". Tune says 5 (ceiling 7), and
    its confirmation run says "try 10". On small-files, c=22 is clean yet "try 44".
  - Parquet ops are HEAD, then the footer, then 4 parallel column reads (6.3 requests per op), so
    dividing by 6.3 is wrong. c=6 peaks at 24 in flight; c=8 peaks at 32.
- **AN-2:** zero throttle tolerance. Parquet at c=16 (4.8 % throttled) does 406 MiB/s, while the
  tune winner does 154 MiB/s (`improvement_pct -61.9`). `throttled()` (`tune/mod.rs:478-545`)
  stops at the first 503 and ignores `--max-throttle-rate`, which is used only for feasibility
  (`mod.rs:188`).
- **AN-3:** deltas vs an infeasible baseline (`tune/mod.rs:782-787`). The tune note says c=24 was
  +20 % but was excluded by the margin.
- **AN-4:** one 503 sets the ceiling; bisection is 3 steps; the margin is counted in workers, and
  the later io_concurrency and range_gap steps change in-flight requests per op without a re-check.
- **AN-5:** throttle rate by requests (finding 4.68 %, compare) vs by ops (SLA
  `diagnosis.rs:571`, tune `mod.rs:155`). For the same run, tune shows 0.147 and the finding 0.046.
- **AN-6:** with `--http h2` on the lab every op failed (`unexpected` ×341), yet `small_objects`
  still said "double to 44". `summary.ops_per_s` counts failed ops, and compare's throughput check
  uses it.
- **AN-7:** at c=22 with `--tail-cut`, p99 stayed at 261 ms (8 cuts out of about 270 stragglers),
  and the run still said "tail-cut at 40 ms would cut p99 to ~50 ms". The simulator's rule is
  max(3·p75, 4·p50) (`simulate.rs:64-77`); the layer's is p95×1.5, minimum 50 ms, 32 samples
  (`engine/stack.rs:86-93`). Why the layer misses TTFB stragglers is not established yet;
  investigate.
- **AN-8:** `rerun_command` (`findings.rs:156-220`) drops warmup (0.5 s → 2 s), seed, the mix
  override, verify, max_ops, partitions and `--profile`, and silently replaces a custom mix with
  the preset's.
- **AN-9:** the `errors` finding on 23 `rate_limited` failures says to run `oprobe doctor` for
  credentials.
- **AN-10:** with one run per side, compare bootstraps ops and seconds as independent samples
  (`compare.rs:138-178`). Two identical configs gave p50 +0.9 %, CI [+0.6, +1.1], "significant".
  Going from 0 to 1 throttled response fails the gate, with no floor (`diff.rs:541`).
- **AN-11:** the attribution "rtt=10-50ms over-represented (53 % vs 2 %)" appears on loopback; the
  kernel's tcp_info RTT seems inflated by the server's response delay (`outliers.rs:184`). The API
  named by `tail_stragglers` changes from run to run.
- **AN-12:** after Ctrl-C, a 0.52 s window still produced "double concurrency" and
  `tail_stragglers`; `report.warnings` was empty, and the run can be a baseline.
- **AN-13:** histogram p99 263.167 vs rows 262.918 shown side by side. `ceiling_inflight_count`
  is 23.8, and `predicted_extra_requests_per_1k_ops_count` is −1031 ("saves 1031 per 1000 reads").
- **P2-8:** the HTML evidence `<dl>` prints raw keys and three decimals (`report/html.rs`).
- **P2-15:** the default `stored_gib = 1024` (`config.rs:935`, `cost.rs:376-380`) is unlabelled;
  the text summary line shows `$275.66 /month`.
- **CMP-1:** text compare pads columns to the longest JSON value (the mix array): the table is
  more than 250 columns wide.

## CLI / MCP / distribution (streams F, C, D)

- **SEC-1:** `url_prefix` and `allowed` (`mcp/tools.rs:816-842`) compare raw strings. With
  `--allow-write --allow-target fs://…/allowed/`, `start_run fs://…/allowed/../other` wrote into
  `other/`. `s3://B/oprobe/../prod`, `//`, `./` and `existing: "../../prod/"` also pass, while
  `S3://` and bucket-case variants are refused.
- **CLI-1:** a sweep of bad inputs, with code and exit:
  - `internal`, exit 1, for all of: `-d 5x`, `lab --latency 5x`, `--preset nope`,
    `--provider aws`, `--access weird`, `foo://bar`, `s3://`, `s3:///x`, a config target with
    `url="s3://"`, `--profile missing.toml`, and gcs with no credentials;
  - accepted silently: `--object-size 0`, `s3 --endpoint ftp://x`, a non-existent `--config`, and
    `target add` of a duplicate name (overwritten);
  - usage with a misleading message: `-d -3s` and `--max-cost-usd -1` say "unexpected argument
    '-3'";
  - `baseline show nope` gives `usage.invalid_argument` and should be `unknown_*`;
  - `top -o json` with no TTY prints a text error; `gui` without the feature exits 1 (should be 2).
  - There are 55 `bail!`/`anyhow!` sites in src/. `message_code` substring matching misses
    "unsupported storage scheme" (`config.rs:1123`), "invalid duration" (`util.rs:183`),
    "unknown preset" (`config.rs:150`), access (`config.rs:478`), "needs a bucket"
    (`config.rs:1043-1058`), provider (`cli/mod.rs:743`) and profile reads (`config.rs:1476`).
  - MCP maps every `internal` to `usage.invalid_argument` (`tools.rs` ~805), so the two surfaces
    disagree.
  - Preflight (`guard.rs:440-447`) turns OpenDAL `Unexpected` "failed to load signing credential"
    into Internal.
- **AG-1:** `start_tune` `structuredContent` has no winner, knobs or profile, and its summary text
  describes the last trial. There is no MCP equivalent of `--emit-profile` / `--emit-rust`. The
  emitted profile hard-codes `proxy = "none"`.
- **AG-2:** `doctor` says "2 entries visible" without naming them. `lab://` is refused as a write
  on a read-only server (`tools.rs:945` allows only memory).
- **AG-3:** `doctor` has `next_command` (string), `explain` has `next_commands` (CLI list), and
  MCP jobs have `next` (MCP syntax with a literal `<previous run>`, `tools.rs:1269`). `run`,
  `compare`, `analyze` and `target add` have none, and MCP `explain_run` returns CLI commands.
- **AG-4:** every command's help lists `-o html`, but most exit 2. `completions` and `man` with
  `-o json` print non-JSON and exit 0. `md` is a ```text fence for most commands.
- **AG-5:** `catalog.rs:90` and `docs/agents.md:194` say `analyze --where error`, which is
  invalid. `resources.rs:44` says the html resource is "Markdown until the HTML renderer lands".
  Compare's `gate.checks[]` uses a prose name, a formatted limit and unitless numbers, and
  `thresholds.error_rate_pp` and `min_samples` lack unit suffixes. The unit test walks only the
  report schema.
- **AG-6:** `get_run` reads `running` for about 8 s after `phase: done`; progress overshoots
  (12.0 of 4.125); an extra argument `bogus` is accepted.
- **AG-7:** `engine/operator.rs:31` says to use `-o KEY=VALUE` and `[target.options]`, and for
  `azblob://c/p` claims `container` is missing.
- **AG-8:** CLI `doctor` writes a 256 KiB probe by default. CLI `cleanup` deletes unless
  `--dry-run`, while MCP defaults to a dry run. Doctor dumps raw OpenDAL debug text, and its
  `InvalidAccessKeyId` hint says "export AWS_ACCESS_KEY_ID" even when the keys are exported.
- **AG-9:** credential discovery exists only in the TTY onboarding (`cli/onboard.rs`).
  `--warmup 99h -d 1s` is planned without a warning.
- **INST-1:**
  - `scripts/install.sh` uses only `api.github.com`; on 403 it fails without falling back.
  - `scripts/install.ps1:10` has the same problem. It also does not update the session's
    `$env:Path` and has no `$ProgressPreference`/`-UseBasicParsing`.
  - `install.ps1` detects ARM64 via `PROCESSOR_ARCHITECTURE`, which reads AMD64 under emulation.
  - `cli/self_update.rs:347` treats any 403 as a rate limit and reports `storage.throttled`.
- **DEMO-1:** `oprobe demo` uses `s3://demo` with root `/bench` (small-files, 64 objects of
  64 KiB, 0.5 s warmup). `lab://demo` resolves to bucket `lab`, root `/` and preset parquet, so
  compare flags target, dataset and mix.
- **DIST-1:**
  - The formula and cask are only release assets; `docs/install.md:50` has users run
    `brew tap-new` by hand; the formula and cask share the token `oprobe`.
  - Scoop has no bucket, and its autoupdate has no hash URL.
  - README, getting-started, install.md and the release notes (`release.yml:358`) say
    `winget install hxddh.oprobe`, but the manifests are zipped only for manual submission.
- **DIST-2:** `action.yml` `version` defaults to a pinned `3.0.0` under `@v3`. The comment step
  has no `continue-on-error` (fork PRs fail). "Save the named baseline" re-saves on every run, and
  `$ARGS` is unquoted.

## GUI / TUI (stream U)

- **VER-1:**
  - `RunForm::from_command` (`ui/launcher.rs:184-212`) keeps only target, preset, duration and
    knobs; `prepare` (`launcher.rs:111-115`) builds the workload from preset and duration only.
  - So the dataset defaults to 256 objects (`config.rs:275`) and warmup to 2 s (`config.rs:587`).
  - `object-size`, `access`, `rate`, `existing` and `max-ops` are dropped as well.
  - `from_report` (`launcher.rs:155-170`) also drops endpoint, region, anonymous, dataset, warmup,
    access, rate, HTTP version and tail-cut.
- **UI-9:** Cost uses three columns with a fixed 96 px name column (`ui/views/tabs.rs:290-325`,
  `ui/kit.rs:549`). At 120×30 the numbers wrap and the overview tiles are truncated (`20.3…`). On
  finished runs the run info covers the tab bar (`shell.rs:853-895`); live, the tabs are cut off.
- **UI-10:** the selectors are fixed at 260 px (`ui/views/compare.rs:79,89`), and the button text
  wraps (`kit.rs:372-380`).
- **UI-11:** `kit::btn` (`kit.rs:158-199`) never sets an accessible label; 20 of its 25 uses and
  the 6 tabs (`shell.rs:813`) depend on derived names. Charts expose only their heading
  (`chart.rs:353`).
- **UI-12:** the command list (`shell.rs:117-130`) feeds the palette (`shell.rs:1055`). It has no
  Verify and no disabled states, unlike the menu (`shell.rs:197-202`).
- **UI-13:** `refresh()` runs only at startup, job end and file open (`ui/store.rs:304,497,518,891`).
- **UI-14:** nothing in `src/ui` reads `RunSummary.interrupted`. A budget stop becomes a failure
  (`ui/bridge.rs:256-262`) with no refresh, so the saved partial run is hidden.
- **UI-15:** the New Run sheet has no initial focus, Enter does not submit, and the TUI shows no
  focus ring. After `/`, typing "api:GetObject" switches to Compare on "c" (`shell.rs:283-336`).
- **UI-16:** the empty Targets state says to run `oprobe target add` (`shell.rs:752`).
  "Connect…" opens only the generic New Run form, and the "AWS keys in the environment s3://"
  entry fills in a target with no bucket.
- **UI-17:**
  - App tune uses 0 s warmup, 8 trials and a 1 s cooldown (`ui/store.rs:296`, ~700); the CLI uses
    1 s, 24 and 5 s (`cli/args.rs:31-43`).
  - The Demo button runs the mixed preset for 8 s.
  - App runs pass no cost settings (`launcher.rs:136`).
- **UI-18:** "Critical: throttling" sits next to "ERRORS 0 %" (`ui/views/overview.rs:341-351`),
  with no throttle tile.
- **UI-19:** the default compare takes the next older run of any target (`ui/store.rs:1060-1071`).
- **UI-20:**
  - The live view rebuilds the whole series from the full timeline every 120 ms
    (`ui/store.rs:313,856`).
  - Each chart recomputes its line data on every update.
  - Each mouse move repaints the plot and formats a new readout (`chart.rs:274-352`).
  - Each target shows only 50 runs while the header says `RUNS (n)` (`shell.rs:717,739`).
  - The compare selectors rebuild the full list on each change (`compare.rs:46-60`).
- **UI-21:**
  - Live shows "Measuring" at 0 ops/s during upload and warmup.
  - The tune trial table has 5 headers for 6 columns (`ui/views/tools.rs:58-68`).
  - The tune result offers only "Save…".
