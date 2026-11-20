//! oprobe's interactive UI, built with Native Shell (NSH): the same app in a terminal
//! (`oprobe top`, feature `tui`) and in a native window (`oprobe gui`, feature `gui`: AppKit on
//! macOS, Win32/Direct2D on Windows, GTK 4 on Linux). Presentation only: every number comes from
//! the engine in the rest of this crate.

mod bridge;
mod files;
mod pages;
mod shell;
pub mod store;
mod term;
mod widgets;

/// Initial form values from the command line (`oprobe top s3://bucket/x --endpoint …`).
#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub target: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub preset: Option<String>,
    pub duration: Option<String>,
    pub concurrency: Option<String>,
    pub ca_cert: Option<std::path::PathBuf>,
    pub insecure: bool,
    pub proxy: Option<String>,
    pub anonymous: bool,
    /// Draw the terminal UI without colors (`--color never`, `NO_COLOR`).
    pub monochrome: bool,
}

static LAUNCH: std::sync::OnceLock<Launch> = std::sync::OnceLock::new();

/// Set the launch options before the UI starts.
pub fn set_launch(l: Launch) {
    let _ = LAUNCH.set(l);
}

pub(crate) fn launch() -> Launch {
    LAUNCH.get().cloned().unwrap_or_default()
}

/// Run the app in the current terminal (`oprobe top`).
pub fn run_tui() -> std::io::Result<()> {
    term::run()
}

#[cfg(feature = "gui")]
fn exit_code(code: impl Into<std::process::ExitCode>) -> std::process::ExitCode {
    code.into()
}

/// Run the app in a native window (`oprobe gui`). Blocks the calling (main) thread and returns
/// the platform's exit code.
#[cfg(feature = "gui")]
pub fn run_gui() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    use nsh_platform_linux as platform;
    #[cfg(target_os = "macos")]
    use nsh_platform_macos as platform;
    #[cfg(target_os = "windows")]
    use nsh_platform_windows as platform;
    // The same UI sets as the terminal host (`term::run`): all of `nsh/base` (plots included)
    // and oprobe's own components.
    platform::set_ui_sets(ui_sets());
    // `run_app` returns `ExitCode` on macOS / Windows and `glib::ExitCode` on Linux.
    let code = exit_code(platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<App>()),
        platform::AppOptions {
            title: "oprobe".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "io.github.hxddh.oprobe".to_owned(),
            size: (1280, 860),
            ..Default::default()
        },
    ));
    // The window is gone; a job still running (its window closed mid-run) is stopped and its
    // test data removed before the process ends. (The quit listeners in `App` do the same
    // where the platform ends the process itself.)
    finish_jobs();
    if code != std::process::ExitCode::SUCCESS {
        log_fatal("the window ended with an error (see the messages above, if any)");
    }
    code
}

/// The UI sets every host serves: `nsh/base` (plots included) and oprobe's components.
pub(crate) fn ui_sets() -> Vec<nsh_core::element::Registry> {
    vec![nsh_base::registry(), crate::registry()]
}

/// Stop running jobs and wait (bounded) for their cleanup, telling the user on stderr.
pub(crate) fn finish_jobs() {
    if bridge::abandoned() || !bridge::busy() {
        return;
    }
    eprintln!("oprobe: stopping the running job and cleaning up its test data…");
    if !bridge::shutdown(bridge::SHUTDOWN_TIMEOUT) {
        let msg = "the running job did not finish cleaning up in time; leftover test data \
                   (oprobe-run-*) can be removed with `oprobe cleanup`";
        eprintln!("oprobe: {msg}");
        log_fatal(msg);
    }
}

/// Where fatal errors of the app are written when stderr may be invisible (launched from
/// Finder or Explorer): `~/Library/Logs/oprobe.log` on macOS, `%LOCALAPPDATA%\oprobe\oprobe.log`
/// on Windows, `$XDG_STATE_HOME/oprobe/oprobe.log` (or `~/.local/state/…`) elsewhere.
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

/// Append a timestamped line to [`log_path`] (best effort; the file is restarted once it
/// passes 256 KiB so it never grows unbounded).
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

use nsh_base::theme;
use nsh_core::prelude::*;

/// The root: the whole surface on the theme's background.
#[component]
pub fn App() -> impl IntoElement {
    // The theme ships in the binary, so `cargo run` and packaged builds look the same as
    // `nsh run` (which also reads `nsh.json`'s "theme").
    match nsh_base::tokens_of(Some(include_str!("theme.json"))) {
        Ok(tokens) => set_tokens(tokens),
        Err(e) => eprintln!("oprobe: theme.json: {e}"),
    }
    let s = store::Store::new();
    provide_context(s);
    // Quitting while a job runs: stop it, show that it is cleaning up, and quit once its test
    // data is gone (bounded by the job's own cleanup). `Store::pull` calls `quit()`.
    nsh_core::element::desktop::on_quit_request(move || s.request_quit());
    // However the app ends (its last window closed, the system ending it), wait for the
    // cleanup of a job still running before the platform ends the process.
    nsh_core::element::desktop::on_app_quit(move || {
        s.finish();
        async {}
    });
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(shell::shell())
}
