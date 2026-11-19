use std::process::ExitCode;

use clap::Parser;
use oprobe::cli::{dispatch_cli, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch_cli(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("oprobe error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
