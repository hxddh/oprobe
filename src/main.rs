use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = oprobe::cli::Cli::parse_from(oprobe::cli::launch_args(std::env::args_os()));
    if let Some(code) = oprobe::cli::ui_entry(&cli) {
        return code;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(oprobe::cli::main(cli));
    // Don't wait for lingering background tasks (idle connections, lab servers).
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    code
}
