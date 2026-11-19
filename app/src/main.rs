//! `oprobe-app`: the desktop app; `oprobe-app --tui` runs the same UI in the terminal.

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("oprobe-app {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "oprobe-app {}: oprobe's desktop app (Native Shell)\n\n\
             Usage: oprobe-app [TARGET] [OPTIONS]\n\n\
             Opens the workbench, pre-filled with TARGET (s3://bucket/prefix, memory://, ...).\n\n\
             Options:\n  \
               --tui                 run the same UI in this terminal instead of a window\n  \
               --endpoint <URL>      custom endpoint (MinIO, R2, oprobe lab)\n  \
               --preset <NAME>       parquet | checkpoint | small-files | metadata | mixed\n  \
               -d, --duration <D>    measurement window, e.g. 30s\n  \
               -c, --concurrency <N> concurrent operations\n  \
               --insecure            skip TLS certificate verification\n  \
               --anonymous           public bucket, no credentials",
            env!("CARGO_PKG_VERSION")
        );
        return std::process::ExitCode::SUCCESS;
    }
    let mut launch = oprobe_app::Launch::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tui" => {}
            "--insecure" => launch.insecure = true,
            "--anonymous" => launch.anonymous = true,
            "--endpoint" => launch.endpoint = it.next().cloned(),
            "--preset" => launch.preset = it.next().cloned(),
            "-d" | "--duration" => launch.duration = it.next().cloned(),
            "-c" | "--concurrency" => launch.concurrency = it.next().cloned(),
            other if !other.starts_with('-') && launch.target.is_none() => {
                launch.target = Some(other.to_owned())
            }
            other => {
                eprintln!("error: unknown argument `{other}` (see --help)");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    oprobe_app::set_launch(launch);
    if args.iter().any(|a| a == "--tui") {
        return match oprobe_app::run_tui() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    env_logger::init();
    let _ = platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<oprobe_app::App>()),
        platform::AppOptions {
            title: "oprobe".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "io.github.hxddh.oprobe".to_owned(),
            size: (1280, 860),
            ..Default::default()
        },
    );
    std::process::ExitCode::SUCCESS
}
