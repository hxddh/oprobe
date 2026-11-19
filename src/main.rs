use std::process::ExitCode;

use clap::Parser;
use oprobe::cli::{run_cli_with_diagnostics, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    run_cli_with_diagnostics(cli).await
}
