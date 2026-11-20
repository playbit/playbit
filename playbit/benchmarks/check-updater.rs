//! Isolated native updater UI. Cmd-Alt-P/U/A/E/D: progress/current/available/error/disabled.
fn main() {
    assert!(std::env::var_os("PLAYBIT_PROFILE").is_some(), "isolated preview profile required");
    playbit::desktop::main_update_preview();
}
