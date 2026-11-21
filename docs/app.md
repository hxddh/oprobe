# Workbench: window and terminal

`oprobe` has one interactive workbench, built with [Native Shell (NSH)](https://github.com/longbridge/nsh). It is the
same app in a native window and in any terminal:

```bash
oprobe                                         # opens the workbench (window if there is a display, else terminal)
oprobe gui s3://my-bucket/oprobe --preset parquet -d 30s    # window
oprobe top s3://my-bucket/oprobe --preset parquet -d 30s    # this terminal (also over SSH)
oprobe top s3://bench/demo --endpoint https://localhost:9000 --insecure
```

| Where | Drawn with | Comes with |
|---|---|---|
| macOS 14+ | AppKit, Core Animation, CoreText | every macOS build: `.pkg`, `.dmg` (`oprobe.app`), archives, Homebrew |
| Windows 10 1809+ | Win32, Direct2D, DirectWrite | every Windows build: `.msi` (Start menu shortcut), `.zip`, Scoop |
| Linux (GTK 4.14+) | GTK 4, Pango | `oprobe-desktop-<ver>-x86_64-unknown-linux-gnu.tar.gz`, or `cargo install oprobe --features gui` |
| Any terminal | NSH terminal backend | every build, including the static Linux binaries, `.deb`, `.rpm` and `cargo install oprobe` |

Linux packages for servers (`.deb`, `.rpm`, static musl archives) are built without the window so they need no GTK;
`oprobe top` is always there.

![Workbench](images/app-workbench.png)

## What it does

The window has three panes:

- **Sidebar**: the run in progress (pinned, with its phase), then every saved run grouped by target, your targets and
  baselines. The list is live: runs saved by the CLI (or another window) appear within about a second, and again
  whenever the window comes to the front. A run stopped by a safety budget or interrupted keeps its place with a
  badge (*partial · budget*, *partial · interrupted*): a budget stop is not a failure. A run stopped before its
  measurement window began reads *stopped during warmup* instead of 0 ops/s. The first 50 runs are listed with
  *Show more* below them; with more than eight runs a search field filters by id, label, target or verdict, and the
  header says what is listed (`RUNS (50 of 180)`).
- **Run inspector**: tabs for the selected run.
  - **Overview**: the verdict, the top findings (each with *Verify*), key numbers, where operation time goes
    (including retry backoff), and live charts with real axes. The number tiles include **THROTTLED** (throttled
    requests ÷ requests, with the retry share under it) next to ERRORS, so a "throttling" verdict always has its
    number; COST/MO says when the stored volume is the built-in assumption (*assumed 1 TiB stored*). Latency can be
    drawn on a log scale; nothing is smoothed. Every chart has a text summary for screen readers and a **Data**
    button that shows its numbers as a table.
  - **Requests**: every backend request in a table that stays fast at 100 000 rows. Sort, resize, filter
    (`api:GetObject status:5xx >100ms remote:10.0. conn:3 retry cold t:2..5`), copy as JSON, reveal in the
    timeline, export CSV.
  - **Timeline**: operations and their requests as a waterfall, coloured by stage, with zoom and pan.
  - **Connections**: per remote IP and per connection: reuse, dial phases, TLS handshake kind, TCP RTT and
    retransmits.
  - **Findings**, **Cost** (the storage assumption is labelled: *(assumed 1 TiB stored — set --stored-gib)*).
- **Inspector**: everything about the selected request or operation (stages, TLS, TCP, retries, request id), or
  *what is different* about a selection. Drag across the heatmap to ask which attributes (API, cold connections,
  a remote IP, a stage…) are over-represented among those requests. Below 1200 points (120 terminal columns) it
  opens over the content (Enter) so the tabs keep their room.

### Runs are replayable

Every run stores its spec (`report.spec`: the resolved profile, the target as you named it, the seed, …).
**Re-run** replays it exactly — the same as `oprobe run --from RUN` — and **Edit and Run Again…** opens the New Run
sheet on it: the fields you change become `--set` overrides of the spec, and everything the sheet does not show
(dataset, mix, warmup, seed, access pattern, rate, HTTP version, tail-cut, `--existing`, `--max-ops`, cost settings)
is kept. Credentials are resolved again from the target, never stored.

### Verify

A finding's **Verify** (and *Verify Top Finding* in the Run menu, `v` in the terminal) replays the run's spec with the
finding's change (`Verification.overrides`), then opens **Compare** on the original run (A) and the verify run (B)
with a verdict banner: *confirmed*, *refuted* or *inconclusive*, with the measured numbers against the finding's
expectation. In the app this is one run per side (low confidence, and the banner says so); `oprobe verify RUN CODE`
repeats and interleaves the pair.

### Compare

**Compare** pairs the shown run with the newest earlier run of the **same target and workload** (the target identity
and `workload_hash`), or the two runs marked in the sidebar. Each selector lists the runs of the other side's target
and workload first (marked `=`) and shrinks with an ellipsis instead of wrapping. A pair that measured different
things says why (*target: …*, *duration: 10s ≠ 30s*, …) and gets no regression verdict.

### Targets

**Add Target…** (the Targets list, the welcome view, the palette, `t` in the terminal) saves a named target to the
config file (`[targets.NAME]`, exactly as `oprobe target add`): the URL with its bucket, region, endpoint and the
credentials source. The sources found on this machine are offered by name — AWS profiles and environment keys,
Google application default credentials, Azure environment variables — and no secret is read. **Test Connection** is
read-only: it resolves the target as a run would and lists at most one entry. *Switch Target…* starts the next run
on a saved target.

### Commands and keyboard

One command list drives the menu bar and the command palette (⌘K / Ctrl+K) with the same enabled states: New Run,
Open Run (search the history), Open Run File, Add Target, Switch Target, Export, Run, Stop, Run Again, Edit and Run
Again, Verify Top Finding, Fix (recommend settings: the tuner on the shown run), the demo, Compare, Tune, Probe, and
the view toggles. A command that cannot run now is shown disabled in both. The app menus carry the usual shortcuts
(⌘N new run, ⇧⌘O open run, ⌘O open file, ⌘R run, ⌘. stop, ⇧⌘R run again, ⇧⌘C compare, ⌘1–6 tabs, ⌘, settings; Ctrl
on Windows and Linux). Sheets put the focus in their first field, Enter submits and Esc cancels. With no runs yet, the
app opens on a welcome view: try the demo (`lab://demo`), add a target (detected AWS, GCP and Azure credentials are
offered), or open a run file.

![Requests](images/app-requests.png)

![Compare](images/app-compare.png)

The terminal workbench (`oprobe top`) has the same model, driven by the keyboard: `r` run again, `s` stop, `n` new
run, `v` verify, `o` open a run, `t` add a target, `1`–`6` tabs, `/` filter, `j`/`k` move, Enter inspect, `c`
compare, `?` help, `q` quit, Tab moves the focus (shown as a reversed band). Single-letter keys act only while no text
field has the focus, so typing a filter never triggers them. It fits 80×24; at 120×30 the tabs, the eight tiles (on
two rows) and the Cost tab (one column) fit without wrapping numbers. It works without colour.

### Live runs and tuning

A live run shows its phase — preparing (uploading the dataset), warmup, measuring, cleanup — and counts progress only
while measuring; its numbers read "—" until the measurement window starts. Live charts append each second instead of
being rebuilt, chart lines are computed once per data change, and the hover crosshair is drawn on its own layer.

The workbench drives the same engine as the CLI with the same defaults, read from one place (`config::defaults()`):
the preset, window and warmup of new runs, the tuner's trial length, trial warmup, maximum trials and cooldown, and
the demo (`lab://demo`), so the numbers are identical. The tuner's trial table has a header for every column, and its
result offers **Run with these settings** (the tuned run's spec with the winner applied), **Verify** (that run
compared with the original, with a verdict) and **Save profile…** (plus the Rust snippet). Test data is cleaned up
when a run ends or is stopped. Credentials come from the usual environment (`AWS_*`, `~/.aws`,
`GOOGLE_APPLICATION_CREDENTIALS`, …), proxies from `HTTPS_PROXY` / `NO_PROXY` (or the *Proxy* field / `--proxy`),
exactly as for the CLI. `top` and `gui` also take `--region`, `--ca-cert`, `--insecure` and `--proxy`.

**Quitting mid-run** (Ctrl+C in the terminal, closing the window) stops the job and waits — up to 30 s — until its test
data is removed; the workbench shows *Stopping and cleaning up test data…* meanwhile. A second Ctrl+C quits at once
(`oprobe cleanup` removes what is left).

`oprobe top` needs an interactive terminal (use `oprobe run` in scripts); it honors `--color never` and `NO_COLOR`.
`oprobe gui` without a display (no `DISPLAY` / `WAYLAND_DISPLAY` on Linux, or an SSH session) says so and exits with
code 2 — use `oprobe top` there. When the window cannot start, the reason is also written to a log file, since
stderr is invisible when launched from Finder or Explorer: `~/Library/Logs/oprobe.log` (macOS),
`%LOCALAPPDATA%\oprobe\oprobe.log` (Windows), `$XDG_STATE_HOME/oprobe/oprobe.log` or
`~/.local/state/oprobe/oprobe.log` (Linux). Saving from the terminal writes to the working directory and never
replaces an existing file (`report-1.json`, …).

## Opening saved runs

Every run, from the CLI or the app, is saved as a `.oprobe` file. Open one in the app with
`oprobe runs open RUNREF` (it starts `oprobe gui --open FILE`), or start the terminal workbench on it with
`oprobe top --open FILE`.

## Unsigned desktop builds

The macOS `.dmg`/`.pkg` and the Windows `.msi` are not signed with a developer certificate yet. macOS 15+: open the
app once, then *System Settings → Privacy & Security → Open Anyway*; macOS 14: right-click `oprobe.app` → *Open*; or
`xattr -dr com.apple.quarantine /Applications/oprobe.app`. Windows: in the SmartScreen prompt choose *More info* →
*Run anyway*. Installing through Homebrew, Scoop or `cargo install` avoids both prompts.

## Building from source

```bash
cargo install oprobe                    # CLI + terminal workbench (Rust ≥ 1.97, a C compiler)
cargo install oprobe --features gui     # + native window (Linux: sudo apt install libgtk-4-dev)

cargo run --features gui -- gui         # from a checkout
cargo test                              # includes headless UI flows on the real NSH engine
```

NSH is built with only what the workbench uses (`plot`); its JS engine, WebGPU shaders, network loader, updater,
math, crypto and media features are off.
