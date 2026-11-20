//! The one native GUI entry point shared by Playbit and generated guest apps.
#![cfg(target_os = "macos")]

/// Called once on the launcher's main thread. Rust panics cannot cross this ABI.
#[unsafe(no_mangle)]
pub extern "C" fn playbit_main() {
    playbit::desktop::app_main();
}
