# The app

`oprobe` with no command opens the app, built on [Native Shell (nsh)](https://github.com/longbridge/nsh):
a native window when there is a display, the same app in the terminal otherwise (also over SSH).

```bash
oprobe          # the app
```

| Where | Drawn with | Comes with |
|---|---|---|
| macOS 14+ | AppKit, Core Animation, CoreText | the `.pkg`, the `.dmg` (`oprobe.app`), the macOS archives, Homebrew |
| Windows 10 1809+ | Win32, Direct2D, DirectWrite | the `.msi` (Start menu entry), the Windows archives, Scoop, winget |
| Linux (GTK 4.14+) | GTK 4, Pango | `oprobe-desktop-<ver>-x86_64-unknown-linux-gnu.tar.gz`, or `cargo install oprobe --features gui` |
| any terminal | the nsh terminal backend | every build except the CLI-only archives |

Linux packages for servers (`.deb`, `.rpm`, the static musl archives) have the app in the
terminal and need no GTK. The CLI-only archives (`oprobe-cli-…`, `install.sh --cli`) have no app
at all.

The app has no logic of its own: it calls the same service layer as the CLI (`run`, `prove`,
`show`), so a run started in the app is a run in `oprobe show --list`, and the reverse.

## The window

The window works like other professional tools on your platform (design:
[app-native](design/app-native.md)):

- **Sidebar**:
  - Lists the runs, newest first, grouped under their operator. Each row shows the verdict, the
    run's short id, the preset, when it ran, and its goodput and p99.
  - Runs started from the app, the CLI or another window show up here.
  - A running job is the first row, with its progress.
  - Click a run to show it. With ⌘ (macOS) or Ctrl held, a click adds or removes a run; with
    Shift held, it extends the selection.
  - The runs are one list for the keyboard (one Tab stop) and for screen readers (a list of
    options, the selected ones said so): ↑ / ↓ move the selection, Shift+↑ / ↓ extend it,
    Home / End go to the ends, typing a run's short id, operator or preset finds it, Return
    opens it, and Delete (⌘⌫ on macOS) asks to delete the selection.
  - The selection looks like the platform's: filled with the accent on macOS (grey while the
    list or the window is not active), a subtle fill with an accent mark on Windows and Linux.
  - Two or more selected runs show **Compare**.
  - Secondary-click a run for its menu: Open, Compare With Selected, Prove…, Copy Run ID,
    Copy Command and Delete…. The row the menu acts on is outlined while it is open.
  - Drag the sidebar's edge to resize it. Its width, whether it shows, the page (Profile or
    Operator) and, on macOS and Windows, the window's size and position are restored at the
    next start.
  - On macOS it is drawn on the window's sidebar material, on Windows 11 on Mica; elsewhere it
    has its own colour.
- **Toolbar**: it sits in the title bar, at the platform's height. It holds the run's id and
  operator, the **Profile / Operator** switch, **New Run** (＋), and **Run Again** ▶, which
  becomes **Stop** ■ while a job runs. On Windows and Linux it also holds the main menu (⋯ or ☰),
  which opens under its button: New Run…, Open Run File…, Show/Hide Sidebar, Full Screen,
  Preferences (Settings on Windows), the documentation, About oprobe, and Quit (Exit on
  Windows).
- **Menus**: on macOS the menu bar holds oprobe, File, Edit, Run, View, Window and Help. Every
  item, the system's own (Undo, Quit, …) included, is in the app's language. On Windows and
  Linux the same menus' shortcuts work under the hidden title bar. Shortcuts that a text field
  also uses (Delete, ⌘⌫) are left to the field while you type, and the menus' actions wait while
  a dialog is open.
- **New Run** (mod+N) is a dialog: the operator (a URL, an `op.toml`, a configured name) with a
  **Choose** pop-up that lists the lab's scenarios, the operators of your config file
  (`[operators.NAME]`), the `op.toml` files you ran before and **Choose op.toml…**; the workload
  (a pop-up); the duration, concurrency and an optional read-only **Existing prefix**. The operator field has the focus when it opens,
  Return anywhere presses the default button and Escape cancels. A `lab://` run starts at once;
  a real target is **planned first**: **Review Plan** shows what the run will write, read and
  delete (and under which prefix), how long it takes and the budgets that stop it, and only then
  does the button become **Run**. Changing the form drops the plan.
- **Run Again** (mod+R) repeats the selected run as it ran: its operator, its whole workload
  (the open-loop rate, the dataset, the seed, the key popularity, …) and every operator value
  the run set on top of its operator (the `op.toml` file or the configured operator is read
  again; for a URL, what is not the default of the URL). A `lab://` run starts at once. A real
  target opens New Run on its plan: the fields show the run's operator, workload, duration,
  concurrency and existing prefix, and **Runs again as** shows the command of the run the form
  will start, as it stands now. Change the duration, the concurrency or the prefix and only
  that changes. Choose another workload and the operator's values stay (its endpoint, its
  layers: the run still goes where it went), while the rate, the dataset and the seed of the
  old workload go. Choose another operator and it is a new run, with nothing carried over.
  Secrets are never stored in a run: they come from the operator's file, the config file or
  the environment, as for any run.
- **Opening a file** (`.oprobe`, a trace, a report): a file is read as untrusted input. A run
  file may hold blocks of at most 256 MiB, decompress to at most 4 GiB and its rows may take
  at most 4 GiB of memory (about nine times the default cap of a million rows); a file beyond
  that, or one that claims more than it holds, is refused with an error, never opened halfway.
- **Settings** (mod+,): the language, where runs are stored and the config file. Each change
  applies at once. On macOS it is a window of its own; on Windows and Linux a preferences dialog
  with a close button; in a terminal a dialog.
- **About oprobe**: the version and links (the system's About panel on macOS).
- **Delete** asks first with the system's alert (Cancel is the default button). The runs go to
  oprobe's trash (`<data>/trash/`), not away: **Undo** in the notice bar brings them back. Once
  the notice goes, the folder goes on to the system's trash where there is one; oprobe's trash
  is emptied after a week. `oprobe show --rm` in the CLI still deletes for good.
- **Quitting** (⌘Q, Ctrl+Q, Exit, or the window's close button) while a run or a prove goes on
  asks first: **Stop and Quit** stops it, or Cancel keeps it running. The app then waits for
  the job to remove its test data, showing what it is doing and for how long, and quits by
  itself when that is done. It waits 60 s at most (an abort of a multipart upload may take
  30 s to give up on a service that does not answer, and the cleanup gets as long again).
  Past that it says which job was left and that objects under its `oprobe-run-…/` prefix may
  remain on the target (the next run against the same target removes them), and offers
  **Quit Anyway**; the same line goes to the log file. In a terminal, Ctrl+C waits the same
  way and prints what was left; a second Ctrl+C leaves at once.
- **Run files**: open a `.oprobe`, `.prove.json`, report `.json` or `op.toml` file with File ▸ Open Run File… (the
  system's open dialog, filtered to those types), drop it on the window (the window is outlined
  while a file is dragged over it), or double-click a `.oprobe` / `.json` file: `oprobe.app` (macOS), the `.msi`
  (Windows) and the Linux desktop archive's `.desktop` entry and MIME type
  (`application/x-oprobe-run`) register `.oprobe` files, and run `oprobe FILE`, which opens the
  file in the app. The archives and the `.deb` / `.rpm` register nothing.
- **Progress**: a running job shows its progress on the Dock or taskbar icon. When it ends while
  the window is in the background, a notification says so and the Dock icon or taskbar button
  asks for attention.
- **Accessibility**: messages in the notice bar are read out by screen readers; control heights
  grow with the system's text size; the dialogs appear without a fade when reduced motion is on.

**Profile.** One run, from the verdict down to the raw numbers:

- **The numbers**: goodput, bandwidth, p50 / p99 / p99.9, failed operations (per operation) and
  throttled requests (per request), with how many operations and requests were measured.
  A **Read these numbers with care** note comes first when the client was saturated, the run is
  partial, open-loop arrivals started late, or too few operations were measured.
- **Findings**, each with its remedy and the evidence it rests on (below). Its named values are
  labelled in the app's language and formatted like the tiles (`9.87 ms`, `2.60 %`). **Show N
  examples** expands the waterfalls of the operations that show the problem.
- **Where the time went**: the stage split of every operation, and of each verb with its
  operations per second, MiB/s, requests per operation and, when any, its throttled share.
- **Requests by API**: for each S3 API, the request count, the status codes, TTFB p50 / p99,
  total p50 / p99 (the same statistics as `oprobe show`) with a bar of the p50 inside the p99,
  scaled to the slowest API, bytes down / up and connection reuse. For a run that wrote in
  parts, a **Writes** line gives the multipart part size OpenDAL used (with the requested chunk
  and the service's limits when it was clamped) and the parts uploaded with their mean size.
  Click an API to list its slowest requests, and one of those to see its operation's
  waterfall.
- **Retries and failures**: retries, time spent waiting between attempts, 429 / 503 responses,
  the longest `Retry-After`, other errors and failure classes.
- **The slowest operations** as a request waterfall. Each request is on a shared time axis and
  coloured by stage, the body's wait for the consumer (stall) its own segment; the wait before a
  retry is drawn as a line in the retry colour. Hovering or focusing a request reads out its stages and,
  when recorded, the provider's error code (`SlowDown`), `Retry-After`, tail-cut / incomplete body status, the connection's rtt,
  min rtt and retransmits, `Server-Timing`, HTTP/2 active
  streams, when the upload reached the kernel and was acknowledged, the CDN cache status, the
  region and the request ID ([request signals](reading-the-profile.md#request-signals)). An operation whose
  async call stack was captured has **Async call stack while in flight** under it, closed until
  clicked.
- **Timeline**, with a verb filter above it (All verbs, or one verb):
  - goodput (successful operations) with the attempted rate, failures included, as a faint line
    behind it, and the intervals with throttled responses or failed operations shaded;
  - p50 / p99 / p99.9; an interval with too few operations for a quantile (fewer than
    10 / (1 − q): 1 000 for p99) draws it faint and dashed, and the readout says so
    (`n=12, too few for p99`);
  - throughput: MiB per second read and written, interval by interval (the bandwidth tile over
    time);
  - requests per second (retries included);
  - **Problems per second**, on its own axis and only when any happened: the retries, the
    throttled responses and the failed operations per second (on the requests' axis a few
    retries would be flat at zero; here one 429 does not look like a thousand either);
  - the number of requests in flight, with the connections dialed in each interval (churn
    behind a latency spike) and, faint, the client's CPU in cores busy (its saturation over
    time).

  Throughput, requests, problems and in-flight are not split by verb: with a verb chosen they
  say "all verbs". Every chart ends where the measured window ends, as the tiles do: what
  operations did while draining after the window counts in its last interval. A legend lists
  only the series that are not zero throughout.
- **Latency distribution**: a tail plot, the share of operations slower than each latency on
  log axes (latency across, 100 % down to 0.01 % up the side), with p50, p99 and p99.9 marked,
  so the tail gets as much room as the body. It is drawn from every row when no row was sampled
  away, else from the run's latency histogram, which counts every operation. With a verb chosen
  (and no range), it is that verb's.
- **Heatmap**: operations by start time and latency, in fine rows (1-1.5-2-3-5-7 per decade, the
  slowest row closed at the next step) when every row was kept, with a colour key from the fewest to the most operations in a cell. Click a cell to list its slowest
  operations (verb, key, latency, status, attempts), and one of those to see its waterfall.
- **Connections**: the connections dialed, each count with its window (`28 during warm-up · 1
  in the window`), failed dials, reuse, the protocols used, TLS, the proxy, peak requests in
  flight, each dial phase's p50 / p99 (DNS, TCP, proxy CONNECT, TLS), and whether the network or
  the server is the limit: the kernel's minimum round trip (the network's) and its smoothed
  round trip p50 / p99 — on a fast network the smoothed one includes the server's delay and
  delayed acknowledgements, and the app says so when it is far above the minimum —, the
  connections that retransmitted, the share of sending time limited by the receive window or
  the send buffer, the delivery rate, the TLS handshakes (resumed share, HelloRetryRequests)
  and the cold start (the warm-up dials' connect time and the first operations' latency).
- **Details** (open on demand):
  - bytes asked for against bytes moved: read, write and request amplification, cache hits,
    and the bytes on the wire (in / out, from the kernel's counters) when they were measured;
  - the hottest keys, object sizes (as ranges: `0 B`, `4 KiB – 64 KiB`, …), prefixes, the first
    read and re-reads of a key, and server addresses, each with its share of the requests (with
    a bar), its throttled share, error share and p99 (a column no row has is left out: hot keys
    carry their share of the throttling only).
- **Run setup** (open on demand): target, workload, load, dataset, mix, the write path, seed, the client (CPU and
  scheduling lag), the share of trace rows kept, and the `oprobe run …` command that runs it
  again.

**A time range.** The timeline charts and the heatmap share one time cursor: hovering one shows
the time on all. Drag across any of them to select a range, shaded on every time chart. The key
numbers, the requests by API, the verbs, the distribution and the slowest operations then show
that range, recomputed from the trace rows (an operation counts in the range it started in, a
request in the range it was sent in); when the run kept only a share of its rows, the range says
so and weights each row by the operations it stands for. Escape, a double-click on a chart or
**Clear** drops the range. `oprobe show RUN --since 3s --until 8s` prints the same range.

Every chart has the Image role and a label that summarises it for screen readers.

Each finding carries the evidence it rests on:

| Finding | Evidence |
|---|---|
| `throttling` | the share throttled at each number of requests in flight, with the fitted limit's interval shaded |
| `key_limit`, `repeated_reads` | the hottest keys as horizontal bars of their share of all throttling (of all requests when nothing was throttled) |
| `small_ranges` | bytes asked for against bytes downloaded, and requests per operation |
| `rate_limit` | throttled responses per second |
| `bandwidth_cap` | bandwidth (MiB/s) over the run |
| `tail_stragglers`, `hung_requests` | the latency tail plot, with p50, p99 and p99.9 marked |
| `errors` | failed operations per second |

**Operator.** The run's layer stack beside each finding's remedy. A finding's **Prove** (and
**Prove this remedy** in Profile) proves that finding as its `next` command (`oprobe prove RUN CODE`, with its objective) does: prove
sizes the remedy from the run (a limit from the fitted throttling limit, say) and then proves
it. Toggle a layer or change a value (a limit, a chunk size) yourself and **Prove** proves your
change with the values you set. The trials stream in live and their A/B pairs are drawn as they
converge: each pair's change as a dot, the running estimate with its 95 % interval narrowing,
and the noise floor around no change, until the sequential test decides: **improved**,
**worse** or **inconclusive**. **Copy** puts the winning diff (Rust or TOML) on the clipboard; when
the run came from an `op.toml` file, **Save to op.toml** writes the proven change back to it in
place (preserving credentials and `${ENV}` references).

**Compare.** Two runs, or the operators of one multi-operator run, side by side: goodput,
bandwidth, requests per second, latency p50 / p99 / p99.9, failed operations, retries, throttled
responses, the stage split and the findings of each, with their goodput, p99 and requests per
second over time and their latency tails drawn on one chart each, and each backend API's
requests and TTFB p99 in every run. Shares (errors, throttled) change by percentage
points. The changes are shown in a neutral colour, never as better or worse: each column is a
single run, and its difference includes run-to-run noise. To measure a change, prove it.

## Language

The app speaks **简体中文** and **English**. It follows the system locale (a `zh` locale selects
Chinese) and a setting in the app overrides it. The CLI, the JSON output and the run files are
English only.

## Keys

`mod` is ⌘ on macOS and Ctrl elsewhere (and in a terminal).

| Keys | Does |
|---|---|
| mod+N | New Run… |
| mod+O | Open Run File… |
| mod+R | Run the selected run again |
| mod+. | Stop the running job |
| mod+shift+P | Prove (your change, else the chosen or top finding's remedy) |
| mod+1, mod+2 | Profile, Operator |
| ctrl+⌘+S (macOS), mod+shift+S | Show or hide the sidebar |
| mod+shift+C | Copy the selected run IDs |
| ⌘⌫ (macOS), Delete | Delete the selected runs (asks first; Undo brings them back) |
| ↑ ↓, Shift+↑ ↓, Home, End, letters | Move, extend and find in the sidebar's list |
| mod+, | Settings |
| F11 (Windows, Linux), ctrl+⌘+F (macOS) | Full screen |
| Ctrl+Q (Linux), ⌘Q (macOS) | Quit (asks first while a job runs) |
| F10 (terminal) | The main menu |
| Ctrl+C (terminal) | Quit (stops a job and cleans up first) |

## Without a display

Over SSH or in a container the app opens in the terminal. Where neither works (no terminal, a
pipe), use the CLI: `oprobe run`, `oprobe show`, `oprobe prove` print the same profile as text or
JSON.
