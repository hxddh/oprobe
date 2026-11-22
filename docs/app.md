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

## Four views

**Runs.** The history, newest first, with live runs streaming in (from the app, the CLI or
another window). Each row shows the operator, workload, verdict, goodput and p99. Prove trials are
folded under their prove run. Start a run from here: an operator (URL, file or configured name)
and a preset.

**Profile.** One run: the verdict, where the time went (the stage split of every operation), the
request waterfall of the slowest operations (each request on a shared time axis, coloured by
stage, with its connection, status and request ID), the per-second timeline and the latency
heatmap (horizontal bands above the bulk are stragglers, vertical stripes are stalls). The
findings sit beside the charts with their evidence; selecting one highlights its requests.

**Operator.** The run's layer stack beside each finding's remedy. Toggle a layer or change a value
(a limit, a chunk size), then press **Prove**: the A/B pairs stream in live with the running
estimate and its interval until the sequential test decides. **Copy** puts the winning diff (Rust
or TOML) on the clipboard.

**Compare.** Two runs, or the operators of one multi-operator run, side by side: goodput, latency
percentiles, the stage split and the findings of each.

## Language

The app speaks **简体中文** and **English**. It follows the system locale (a `zh` locale selects
Chinese) and a setting in the app overrides it. The CLI, the JSON output and the run files are
English only.

## Keys

Everything works from the keyboard, in the terminal and in a window; the app's help lists the
keys. In a window the mouse and the platform's usual shortcuts work too.

## Without a display

Over SSH or in a container the app opens in the terminal. Where neither works (no terminal, a
pipe), use the CLI: `oprobe run`, `oprobe show`, `oprobe prove` print the same profile as text or
JSON.
