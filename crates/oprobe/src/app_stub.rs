//! The bare `oprobe` (the app). The nsh workbench is being rebuilt for oprobe 6; until it
//! lands, a bare `oprobe` says so and points at the commands.

use crate::cli::Cli;
use crate::output::OutputFormat;

/// What a bare `oprobe` prints until the app lands.
pub const MESSAGE: &str = "the oprobe app is coming in oprobe 6; meanwhile: `oprobe demo`, \
`oprobe run OPERATOR`, `oprobe show --list`, `oprobe prove RUN` (see `oprobe --help`)";

/// Start the app; returns the process exit code.
pub fn launch(cli: &Cli) -> u8 {
    if !cfg!(feature = "app") {
        return refuse(
            cli,
            "this oprobe was built without its app",
            "use the commands (`oprobe --help`), or install a build with the `app` feature",
        );
    }
    if cli.output_format() == Some(OutputFormat::Json) {
        return refuse(cli, MESSAGE, "use `oprobe --help` for the commands");
    }
    println!("{MESSAGE}");
    0
}

/// The app cannot start here: a `usage.invalid_argument` error (exit 2), as JSON under `-o json`.
fn refuse(cli: &Cli, message: &str, hint: &str) -> u8 {
    use crate::errors::{ErrorCode, OprobeError};
    let format = cli.output_format().unwrap_or_default();
    let body = crate::errors::classify(
        &OprobeError::new(ErrorCode::UsageInvalidArgument, message)
            .with_hint(hint)
            .into(),
        "s3",
    );
    crate::output::Output::new(format, true, true).error(&body)
}
