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
  - Two or more selected runs show **Compare**.
  - Secondary-click a run for its menu: Open, Compare With Selected, Prove…, Copy Run ID,
    Copy Command and Delete…. Delete always asks first.
- **Toolbar**: it sits in the title bar. It holds the run's id and operator, the
  **Profile / Operator** switch, **New Run** (＋), and **Run Again** ▶, which becomes **Stop** ■
  while a job runs. On Windows and Linux it also holds the main menu (⋯ or ☰).
- **Menus**: on macOS the menu bar holds File, Edit, Run, View, Window and Help. On Windows and
  Linux the same menus are in the main menu.
- **Dialogs**: New Run (mod+N) and Settings (mod+,) open as dialogs. Settings is where you choose
  the language.
- **Run files**: open a `.oprobe` file with File ▸ Open Run File…, or drop it on the window.
- **Progress**: a running job shows its progress on the Dock or taskbar icon.

**Profile.** One run: the verdict, where the time went (the stage split of every operation, and
of each verb when there are several), the request waterfall of the slowest operations (each
request on a shared time axis, coloured by stage, with its connection, status and request ID), the
per-second timeline (the seconds that were throttled or failed shaded under goodput), the latency
distribution with its p50 and p99 marked, and the latency heatmap (horizontal bands above the bulk
are stragglers, vertical stripes are stalls). Each finding carries the chart of its evidence:

| Finding | Evidence chart |
|---|---|
| `throttling`, `key_limit` | the share throttled at each number of requests in flight, the fitted limit's interval shaded |
| `rate_limit` | throttled responses per second |
| `bandwidth_cap` | bandwidth (MiB/s) over the run |
| `tail_stragglers`, `hung_requests` | the latency distribution, p50 and p99 marked |
| `errors` | failed operations per second |

**Operator.** The run's layer stack beside each finding's remedy. Toggle a layer or change a value
(a limit, a chunk size), then press **Prove**: the A/B pairs stream in live and are drawn as they
converge: each pair's change as a dot, the running estimate with its 95 % interval narrowing, and
the A/A noise floor around no change, until the sequential test decides. **Copy** puts the winning diff (Rust
or TOML) on the clipboard.

**Compare.** Two runs, or the operators of one multi-operator run, side by side: goodput, latency
percentiles, the stage split and the findings of each, with their goodput over time and their
latency distributions drawn on one chart each.

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
| mod+shift+P | Prove the current change |
| mod+1, mod+2 | Profile, Operator |
| ctrl+mod+S (macOS), mod+shift+S | Show or hide the sidebar |
| mod+shift+C | Copy the selected run IDs |
| mod+Backspace | Delete the selected runs (asks first) |
| mod+, | Settings |
| F10 (terminal) | The main menu |

## Without a display

Over SSH or in a container the app opens in the terminal. Where neither works (no terminal, a
pipe), use the CLI: `oprobe run`, `oprobe show`, `oprobe prove` print the same profile as text or
JSON.
