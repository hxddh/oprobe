//! oprobe's desktop and terminal app, built with Native Shell (NSH): native windows on
//! macOS (AppKit), Windows (Win32/Direct2D) and Linux (GTK 4), and the same UI in a terminal.
//! All measurement logic lives in the `oprobe` library; this crate is presentation only.

mod bridge;
mod files;
mod pages;
mod shell;
pub mod store;
mod term;
mod ui;

/// Initial form values from the command line (`oprobe-app s3://bucket/x --endpoint …`).
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

/// Run the app in the current terminal (`oprobe-app --tui`).
pub fn run_tui() -> std::io::Result<()> {
    term::run()
}

use nsh_base::theme;
use nsh_core::prelude::*;

/// The app's UI set (`nsh.json` `"rust"`).
#[native_module("oprobe/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The root: the whole surface on the theme's background.
#[component]
pub fn App() -> impl IntoElement {
    // The theme ships in the binary, so `cargo run` and packaged builds look the same as
    // `nsh run` (which also reads `nsh.json`'s "theme").
    match nsh_base::tokens_of(Some(include_str!("../theme.json"))) {
        Ok(tokens) => set_tokens(tokens),
        Err(e) => eprintln!("oprobe-app: theme.json: {e}"),
    }
    provide_context(store::Store::new());
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(shell::shell())
}
