use std::process::ExitCode;

fn main() -> ExitCode {
    oprobe::output::quiet_broken_pipes();
    let cli = match oprobe::cli::parse_cli(oprobe::cli::launch_args(std::env::args_os())) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // The same deep stacks as the main thread (see build.rs).
        .thread_stack_size(8 * 1024 * 1024)
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
