//! NSH 原生桌面独立宿主入口（对齐 `longbridge/nsh` 官方 `rust-recipe.md`）：
//! 在 `native/` 目录执行 `cargo run` 将直接拉起当前系统的原生图形窗口；
//! 在仓库根目录执行 `nsh run --target gpui` 或 `nsh run --target tui` 则由 NSH CLI 挂载 `nsh_app_oprobe::App`。

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> impl std::process::Termination {
    let _ = env_logger::try_init();
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<nsh_app_oprobe::App>()),
        platform::AppOptions {
            title: "oprobe — OpenDAL Storage Stack Profiler".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "io.github.hxddh.oprobe".to_owned(),
            size: (1240, 820),
            ..Default::default()
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("Use `nsh run` for mobile/web targets, or run `oprobe` at repo root for SSH TUI.");
