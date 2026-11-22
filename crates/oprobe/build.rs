//! Windows gives the main thread a 1 MiB stack, far less than Linux and macOS (8 MiB). Deep but
//! bounded recursion (JSON Schema generation for `capabilities`/`schema`, serde of large
//! reports) overflowed it in debug builds, so the binary asks for 8 MiB like the others.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" {
        if env == "msvc" {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
        }
    }
}
