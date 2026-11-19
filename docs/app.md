# Desktop & terminal app

`oprobe-app` is oprobe's graphical front end, built with [Native Shell (NSH)](https://github.com/longbridge/nsh):
one Rust UI rendered with each platform's own toolkit, and the same UI on a terminal grid.

| Platform | Drawn with | Package |
|---|---|---|
| macOS 14+ | AppKit, Core Animation, CoreText | `oprobe-app-<ver>-macos-universal.dmg` (Apple Silicon + Intel `oprobe.app`) |
| Windows 10 1809+ | Win32, Direct2D, DirectWrite | `oprobe-app-<ver>-x64.msi` (Start menu shortcut), or the portable `.zip` |
| Linux (GTK 4.14+) | GTK 4, Pango | `oprobe-app-<ver>-x86_64-unknown-linux-gnu.tar.gz` (`install.sh`, `.desktop` entry, icon) |
| Any terminal | NSH terminal backend (ratatui) | `oprobe-app --tui` — same UI, keyboard and mouse |

![Workbench](images/app-workbench.png)

## What it does

- **Workbench** — choose a target (`s3://…`, `gcs://…`, `memory://`, …), a preset, duration and client knobs, then
  *Run*. While it runs: verdict, KPIs, live operations/s and p99 charts and where the time goes. When it finishes:
  per-operation and per-API tables, connections, findings, the projected cost per provider, and the **slowest
  operation as a waterfall** — one track per backend request, colored by stage, with the async call stack captured
  while it was stuck. *Save report…* writes the same JSON (`oprobe diff`-compatible) and Markdown as the CLI.
- **Tune** — runs the throttle-aware tuner against the target, shows every trial as it completes, and offers the
  winner: *Apply to workbench*, *Save profile…* (`oprobe run --profile`), *Save Rust code…*.
- **Probe** — DNS / TCP / proxy CONNECT / TLS and cold vs warm TTFB of an endpoint, with clock skew.

![Slowest operation](images/app-slowest-op.png)

All measurement logic is the `oprobe` library — the app, the CLI and the built-in `oprobe top` produce identical
numbers. Test data is cleaned up when a run ends or is stopped.

## Launching

```bash
oprobe-app                                     # opens the workbench
oprobe-app s3://my-bucket/oprobe --preset parquet -d 30s
oprobe-app s3://bench/demo --endpoint https://localhost:9000 --insecure
oprobe-app --tui                               # the same UI in this terminal
```

Credentials come from the usual environment (`AWS_*`, `~/.aws`, `GOOGLE_APPLICATION_CREDENTIALS`, …), proxies from
`HTTPS_PROXY` / `NO_PROXY`, exactly as for the CLI.

## Terminal UIs: `oprobe top` vs `oprobe-app --tui`

| | `oprobe top` | `oprobe-app --tui` |
|---|---|---|
| Ships with | the `oprobe` CLI (`cargo install oprobe`) | the desktop app |
| Built with | ratatui directly | NSH's terminal backend (ratatui underneath) |
| UI | compact single-screen dashboard, single-key controls | the full app: forms, tabs, tuner, probe, mouse |
| Dependencies | pure Rust | NSH (includes some C, e.g. its syntax highlighter's regex engine), Rust ≥ 1.97 |

`oprobe top` stays in the CLI because NSH's terminal backend currently pulls C code and needs Rust 1.97; putting it in
the `oprobe` crate would break the pure-Rust `cargo install oprobe`. If NSH makes those features optional, `oprobe top`
can render the NSH app directly (`nsh-platform-tui` embeds into a ratatui frame).

## Building from source

```bash
cd app
cargo run                    # desktop window (Linux needs GTK 4.14+, GStreamer dev packages)
cargo run -- --tui           # terminal
cargo test                   # headless UI flows on the real NSH engine
```

Linux build dependencies (Debian/Ubuntu 24.04):
`sudo apt install clang libclang-dev libgtk-4-dev libwebkitgtk-6.0-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`.

With the [NSH CLI](https://nsh.dev/docs/installation) you can also use `nsh run`, `nsh run --target tui` and
`nsh package` from `app/` (`nsh.json` describes the app).
