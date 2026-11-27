use std::process::ExitCode;

fn main() -> ExitCode {
    oprobe::output::quiet_broken_pipes();
    let mut args = oprobe::cli::launch_args(std::env::args_os());
    // `oprobe RUN.oprobe` (a double click on a run file) is the bare app, opening it.
    let file = oprobe::cli::launch_file(&args);
    if file.is_some() {
        args.truncate(1);
    }
    let cli = match oprobe::cli::parse_cli(args) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    // A bare `oprobe` is the app: it owns the main thread (native windows need it) and runs
    // its work on a runtime of its own.
    if cli.command.is_none() {
        let code = oprobe::launch::bare(&cli, file);
        oprobe::output::flush_stderr(std::time::Duration::from_secs(2));
        return code;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // The same deep stacks as the main thread (see build.rs).
        .thread_stack_size(8 * 1024 * 1024)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            oprobe::output::stderr_line(format_args!("error: cannot start the async runtime: {e}"));
            oprobe::output::flush_stderr(std::time::Duration::from_secs(2));
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(oprobe::cli::main(cli));
    // Don't wait for lingering background tasks (idle connections, lab servers).
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    // What is still queued for stderr; a stderr nobody reads does not hold the exit.
    oprobe::output::flush_stderr(std::time::Duration::from_secs(2));
    code
}
