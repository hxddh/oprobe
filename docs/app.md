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

- **Workbench** — choose a target (`s3://…`, `gcs://…`, `memory://`, …), a preset, duration and client knobs, then
  *Run*. While it runs: verdict, KPIs, live operations/s and p99 charts and where the time goes. When it finishes:
  per-operation and per-API tables, connections, findings, the projected cost per provider, and the **slowest
  operation as a waterfall** — one track per backend request, colored by stage, with the async call stack captured
  while it was stuck. *Save report…* writes the same JSON (`oprobe diff`-compatible) and Markdown as `oprobe run`.
- **Tune** — runs the throttle-aware tuner against the target, shows every trial as it completes, and offers the
  winner: *Apply to workbench*, *Save profile…* (`oprobe run --profile`), *Save Rust code…*.
- **Probe** — DNS / TCP / proxy CONNECT / TLS and cold vs warm TTFB of an endpoint, with clock skew.

![Slowest operation](images/app-slowest-op.png)

The workbench drives the same engine as the CLI, so the numbers are identical. Test data is cleaned up when a run
ends or is stopped. Credentials come from the usual environment (`AWS_*`, `~/.aws`,
`GOOGLE_APPLICATION_CREDENTIALS`, …), proxies from `HTTPS_PROXY` / `NO_PROXY`, exactly as for the CLI.

## Unsigned desktop builds

The macOS `.dmg`/`.pkg` and the Windows `.msi` are not signed with a developer certificate yet. macOS: right-click
`oprobe.app` → *Open* the first time (or `xattr -dr com.apple.quarantine /Applications/oprobe.app`). Windows: in
the SmartScreen prompt choose *More info* → *Run anyway*. Installing through Homebrew, Scoop or `cargo install` avoids
both prompts.

## Building from source

```bash
cargo install oprobe                    # CLI + terminal workbench (Rust ≥ 1.97, a C compiler)
cargo install oprobe --features gui     # + native window (Linux: sudo apt install libgtk-4-dev)

cargo run --features gui -- gui         # from a checkout
cargo test                              # includes headless UI flows on the real NSH engine
```

NSH is built with only what the workbench uses (`plot`); its JS engine, WebGPU shaders, network loader, updater,
math, crypto and media features are off.
