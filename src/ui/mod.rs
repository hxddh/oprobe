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
    pub preset: Option<String>,
    pub duration: Option<String>,
    pub concurrency: Option<String>,
    pub insecure: bool,
    pub anonymous: bool,
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

/// Run the app in a native window (`oprobe gui`). Blocks the calling (main) thread.
#[cfg(feature = "gui")]
pub fn run_gui() {
    #[cfg(target_os = "linux")]
    use nsh_platform_linux as platform;
    #[cfg(target_os = "macos")]
    use nsh_platform_macos as platform;
    #[cfg(target_os = "windows")]
    use nsh_platform_windows as platform;
    let _ = platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<App>()),
        platform::AppOptions {
            title: "oprobe".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "io.github.hxddh.oprobe".to_owned(),
            size: (1280, 860),
            ..Default::default()
        },
    );
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
    provide_context(store::Store::new());
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(shell::shell())
}
