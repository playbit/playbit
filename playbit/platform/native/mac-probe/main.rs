//! Standalone Rust launcher linked to the C SDK, without an engine rlib dependency.
#![allow(
    unsafe_code,
    reason = "The launcher owns static callbacks through the process-owned AppKit loop."
)]
extern crate playbit_app;
use playbit_api::abi::operations::App;

unsafe extern "C" {
    fn pb_mac_run(
        profile: *const u8,
        profile_length: u32,
        title: *const u8,
        title_length: u32,
        declaration: *const u8,
        declaration_length: u32,
        app: *const App,
    ) -> i32;
}
fn main() {
    let profile = std::env::args().nth(1).expect("absolute profile path required");
    let title: &[u8] = include!("title.rs");
    let declaration = include_bytes!("native-root.json");
    let app = playbit_api::native_app();
    // SAFETY: buffers and callback code remain live until process exit or an early host error.
    let status = unsafe {
        pb_mac_run(
            profile.as_ptr(),
            profile.len().try_into().expect("bounded profile"),
            title.as_ptr(),
            title.len() as u32,
            declaration.as_ptr(),
            declaration.len() as u32,
            &app,
        )
    };
    assert_eq!(status, 0, "native host failed");
}
