# oprobe-app

The desktop and terminal app of [oprobe](../README.md), built with [Native Shell (NSH)](https://github.com/longbridge/nsh).
See [docs/app.md](../docs/app.md).

```bash
cargo run              # native window (AppKit / Win32 / GTK 4)
cargo run -- --tui     # the same UI in the terminal
cargo test             # headless UI flows
```

Layout: `src/bridge.rs` runs the `oprobe` engine on its own Tokio runtime; `src/store.rs` holds the app state as
signals; `src/ui/` is the app's component library on `nsh_base`; `src/pages/` are the screens; `src/term.rs` hosts the
terminal target. This crate is a separate workspace so NSH stays out of the `oprobe` crate.
