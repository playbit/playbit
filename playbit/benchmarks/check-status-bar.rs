//! Native visual fixture; no engine, network connection, or user profile.
//! Cmd-Alt-1/2/3/4/5/0 selects volumes/restart/connection/failure/crash report/idle.
fn main() {
    assert!(std::env::var_os("PLAYBIT_PROFILE").is_some(), "isolated preview profile required");
    playbit::desktop::main_status_bar_preview();
}
