//! The oprobe app on Native Shell (nsh, design v6 §4 and D1): four views — Runs, Profile,
//! Operator, Compare — in a native window (AppKit, Win32, GTK 4; feature `gui`) or on the
//! terminal grid. It has no logic of its own: every action calls `oprobe_core::app::App`.
//!
//! Layers: [`data`] (the one adapter onto the service layer's result types), [`jobs`] (`run`
//! and `prove` off the UI thread), [`store`] (state and its writers), [`i18n`] (English and
//! 简体中文), `kit`/`chart`/`heatmap` (controls and charts), `views`.

mod chart;
pub mod data;
mod fmt;
mod heatmap;
pub mod i18n;
pub mod jobs;
mod kit;
pub mod store;
mod term;
// A clipboard write answers once done; nothing waits for it.
#[allow(clippy::let_underscore_future)]
mod views;

pub use store::{Launch, set_launch};

use nsh_base::theme;
use nsh_core::prelude::*;

/// The UI sets every host serves: `nsh/base` (plots included) and oprobe's root.
pub(crate) fn ui_sets() -> Vec<nsh_core::element::Registry> {
    vec![nsh_base::registry(), crate::registry()]
}

/// Run the app full-screen in the current terminal.
pub fn run_tui(monochrome: bool) -> std::io::Result<()> {
    term::run(monochrome)
}

/// Run the app in a native window; blocks the calling (main) thread.
#[cfg(feature = "gui")]
pub fn run_gui() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    use nsh_platform_linux as platform;
    #[cfg(target_os = "macos")]
    use nsh_platform_macos as platform;
    #[cfg(target_os = "windows")]
    use nsh_platform_windows as platform;
    platform::set_ui_sets(ui_sets());
    let options = platform::AppOptions {
        title: "oprobe".to_owned(),
        app_id: "io.github.hxddh.oprobe".to_owned(),
        size: (1280, 860),
        ..Default::default()
    };
    // `run_app` returns `ExitCode` on macOS / Windows and `glib::ExitCode` on Linux.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::useless_conversion))]
    let code: std::process::ExitCode = platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<App>()),
        options,
    )
    .into();
    // The window is gone: a job still running is stopped and its test data removed.
    store::finish_jobs();
    if code != std::process::ExitCode::SUCCESS {
        log_fatal("the window ended with an error");
    }
    code
}

/// Where fatal errors of the app are written when stderr may be invisible (launched from a
/// desktop): `~/Library/Logs/oprobe.log` on macOS, `%LOCALAPPDATA%\oprobe\oprobe.log` on
/// Windows, `$XDG_STATE_HOME/oprobe/oprobe.log` (or `~/.local/state/…`) elsewhere.
pub fn log_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Logs/oprobe.log"));
    }
    if cfg!(windows) {
        return env("LOCALAPPDATA").map(|d| d.join("oprobe").join("oprobe.log"));
    }
    env("XDG_STATE_HOME")
        .or_else(|| env("HOME").map(|h| h.join(".local/state")))
        .map(|d| d.join("oprobe").join("oprobe.log"))
}

/// Append a line to [`log_path`] (best effort; restarted past 256 KiB).
pub fn log_fatal(msg: &str) {
    use std::io::Write;
    let Some(path) = log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!big)
        .write(true)
        .truncate(big)
        .open(&path);
    if let Ok(mut f) = file {
        let _ = writeln!(
            f,
            "{} oprobe {}: {msg}",
            crate::util::now_rfc3339(),
            env!("CARGO_PKG_VERSION")
        );
    }
}

/// The main window: a minimum size, and on macOS the toolbar as the title bar.
fn setup_window() {
    use nsh_core::desktop::WindowAttributes;
    use nsh_core::element::desktop::main_window;
    if kit::in_terminal() {
        return;
    }
    let mut attrs = WindowAttributes::default();
    attrs.title = Some("oprobe".into());
    attrs.min_size = Some(Some(size(px(760.0), px(480.0))));
    main_window().set(attrs);
}

/// The root: the theme, the store, and the views in the chosen language (they are drawn
/// again when it changes).
#[component]
pub fn App() -> impl IntoElement {
    match nsh_base::tokens_of(Some(include_str!("theme.json"))) {
        Ok(tokens) => set_tokens(tokens),
        Err(e) => eprintln!("oprobe: theme.json: {e}"),
    }
    let s = store::Store::new();
    provide_context(s);
    setup_window();
    nsh_core::element::desktop::on_app_quit(move || {
        store::finish_jobs();
        async {}
    });
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(dynamic(move || {
            i18n::set(s.lang.get());
            untracked(|| views::shell::shell().into_element())
        }))
}
