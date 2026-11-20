//! oprobe's interactive UI, built with Native Shell (NSH): the same app in a terminal
//! (`oprobe top`, feature `tui`) and in a native window (`oprobe gui`, feature `gui`: AppKit on
//! macOS, Win32/Direct2D on Windows, GTK 4 on Linux). Presentation only: every number comes from
//! the engine in the rest of this crate.
//!
//! Layers: `model` (plain Rust: formatting, scales, series, filters — unit-tested), `kit` (the
//! app's styled controls on `nsh_base`), `chart` / `heatmap` (honest charts on a canvas),
//! `store` (state and its only writers), `bridge` (engine jobs), `views` (the screens).

pub mod bridge;
mod chart;
// A menu, reveal or URL request answers once done; nothing waits for it.
#[allow(clippy::let_underscore_future)]
mod files;
mod heatmap;
mod kit;
pub mod launcher;
pub mod model;
pub mod store;
mod term;
#[allow(clippy::let_underscore_future)]
mod views;

/// Initial values from the command line (`oprobe top s3://bucket/x --endpoint …`).
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
    /// Where runs and the config file are (default: [`crate::store::Paths::resolve`]).
    pub paths: Option<crate::store::Paths>,
}

static LAUNCH: std::sync::Mutex<Option<Launch>> = std::sync::Mutex::new(None);

/// Set the launch options before the UI starts (again before each start, in tests).
pub fn set_launch(l: Launch) {
    *LAUNCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
}

pub(crate) fn launch() -> Launch {
    LAUNCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Whether an engine job (run, tuning, probe) is still running, its cleanup included.
pub fn jobs_running() -> bool {
    bridge::busy()
}

/// Run the app in the current terminal (`oprobe top`).
pub fn run_tui() -> std::io::Result<()> {
    term::run()
}

#[cfg(feature = "gui")]
fn exit_code(code: impl Into<std::process::ExitCode>) -> std::process::ExitCode {
    code.into()
}

/// The file types the app opens (`.oprobe` run files): file associations where the platform
/// registers them from the app (Linux desktop entry; the bundles on macOS and Windows).
#[cfg(feature = "gui")]
fn file_types() -> Vec<nsh_core::file_type::FileType> {
    let json = serde_json::json!([{
        "name": "oprobe run",
        "extensions": ["oprobe"],
        "mimeTypes": ["application/x-oprobe-run"],
        "contentTypes": ["io.github.hxddh.oprobe.run"],
        "role": "viewer"
    }]);
    nsh_core::file_type::file_types_from_json(&json)
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
    // The same UI sets as the terminal host (`term::run`).
    platform::set_ui_sets(ui_sets());
    let mut options = platform::AppOptions {
        title: "oprobe".to_owned(),
        app_id: "io.github.hxddh.oprobe".to_owned(),
        size: (1360, 880),
        ..Default::default()
    };
    options.file_types = file_types();
    options.single_instance = true;
    // `run_app` returns `ExitCode` on macOS / Windows and `glib::ExitCode` on Linux.
    let code = exit_code(platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<App>()),
        options,
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
use nsh_core::storage::Storage;

const PLACEMENT_KEY: &str = "oprobe.window.v3";

/// The main window: native title bar style and material per platform, its placement restored.
fn setup_window(s: store::Store) {
    use nsh_core::desktop::{Backdrop, TitleBarOptions, TitleBarStyle, WindowAttributes};
    use nsh_core::element::desktop::main_window;
    if kit::in_terminal() {
        return;
    }
    let w = main_window();
    let mut attrs = WindowAttributes::default();
    attrs.title = Some("oprobe".into());
    attrs.min_size = Some(Some(size(px(760.0), px(480.0))));
    match os() {
        // macOS: the toolbar is the title bar (traffic lights inside it); the sidebar's
        // vibrancy shows through.
        nsh_core::platform::Os::MacOs => {
            let mut tb = TitleBarOptions::default();
            tb.style = TitleBarStyle::Hidden;
            tb.height = Some(px(44.0));
            attrs.title_bar = Some(tb);
            attrs.backdrop = Some(Backdrop::Window);
        }
        // Windows 11: Mica behind the window.
        nsh_core::platform::Os::Windows => attrs.backdrop = Some(Backdrop::Window),
        _ => {}
    }
    w.set(attrs);
    if let Some(p) = Storage::get_item(PLACEMENT_KEY).and_then(|j| serde_json::from_str(&j).ok()) {
        w.set_placement(p);
    }
    let save = move || {
        if let Some(st) = main_window().state()
            && let Ok(j) = serde_json::to_string(&st.placement)
        {
            let _ = Storage::set_item(PLACEMENT_KEY, j);
        }
    };
    w.on_bounds_change(move |_| save());
    w.on_mode_change(move |_, _| save());
    w.on_focus_change(move |f| s.window_focused(f));
}

/// Files the system opens the app with (Open With, a double click, a second launch).
fn listen_for_files(s: store::Store) {
    spawn(async move {
        loop {
            let files = next_open_files().await;
            for f in files {
                s.open_path(f);
            }
        }
    });
    spawn(async move {
        loop {
            let second = next_second_instance().await;
            for a in second.args.iter().skip(1) {
                let p = std::path::Path::new(a);
                if p.extension().is_some_and(|e| e == "oprobe") {
                    s.open_path(std::path::Path::new(&second.cwd).join(p));
                }
            }
        }
    });
}

/// The root: the whole surface on the theme's background.
#[component]
pub fn App() -> impl IntoElement {
    // The theme ships in the binary, so `cargo run` and packaged builds look the same.
    match nsh_base::tokens_of(Some(include_str!("theme.json"))) {
        Ok(tokens) => set_tokens(tokens),
        Err(e) => eprintln!("oprobe: theme.json: {e}"),
    }
    let s = store::Store::new();
    provide_context(s);
    views::shell::apply_appearance(&s.prefs.with_untracked(|p| p.appearance.clone()));
    setup_window(s);
    listen_for_files(s);
    // `oprobe gui run.oprobe`: open it.
    if let Some(t) = launch().target.filter(|t| t.ends_with(".oprobe")) {
        s.overlay.set(None);
        s.open_path(std::path::PathBuf::from(t));
    }
    // Quitting while a job runs: stop it, show that it is cleaning up, and quit once its test
    // data is gone (bounded by the job's own cleanup). `Store::pull` calls `quit()`.
    nsh_core::element::desktop::on_quit_request(move || s.request_quit());
    // However the app ends (its last window closed, the system ending it), wait for the
    // cleanup of a job still running before the platform ends the process.
    nsh_core::element::desktop::on_app_quit(move || {
        s.finish();
        async {}
    });
    // Cold start: from the store's creation to the first frame.
    spawn(async move {
        next_frame().await;
        let ms = s.cold_start_ms();
        if std::env::var_os("OPROBE_UI_TIMING").is_some() {
            eprintln!("oprobe: first frame after {ms:.1} ms");
        }
    });
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(views::shell::shell())
}
