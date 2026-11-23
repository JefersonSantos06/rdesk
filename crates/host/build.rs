//! Two shorthands for the code:
//! * `desktop`: a computer with its screen, keyboard and sound (Windows, macOS, and Linux with
//!   the `screen` feature);
//! * `headless`: Linux without it (a server reached by terminal, files and tunnels).
//!
//! Phones are neither: the app around the host brings the screen and input.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(desktop)");
    println!("cargo::rustc-check-cfg=cfg(headless)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let screen = std::env::var_os("CARGO_FEATURE_SCREEN").is_some();
    match os.as_str() {
        "android" | "ios" => {}
        "linux" if !screen => println!("cargo::rustc-cfg=headless"),
        _ => println!("cargo::rustc-cfg=desktop"),
    }
}
