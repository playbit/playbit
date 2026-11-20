//! Native Mac startup. The app registers its state; the SDK supplies lifecycle callbacks.
#![allow(
    unsafe_code,
    reason = "The launcher retains its app callbacks through AppKit termination."
)]
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("the native starter currently supports aarch64-apple-darwin only");
// Link the app library, including its registration symbol.
extern crate playbit_app;
use playbit_api::abi::operations::App;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/playbit/native-app.rs"));
unsafe extern "C" {
    fn pb_mac_run_portable(
        profile: *const u8,
        profile_length: u32,
        package_path: *const u8,
        package_path_length: u32,
        digest: *const u8,
        app: *const App,
    ) -> i32;
}
fn main() {
    if let Err(error) = start() {
        eprintln!("{APP_NAME}: {error}");
        std::process::exit(1);
    }
}
fn start() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .skip(1)
        .any(|arg| arg == "--gas-limit" || arg.as_encoded_bytes().starts_with(b"--gas-limit="))
    {
        return Err(
            "--gas-limit is unsupported: native executables cannot enforce Wasm fuel limits".into(),
        );
    }
    let mut args = std::env::args_os().skip(1);
    // With no positional profile, the shared runtime resolves PLAYBIT_PROFILE
    // independently of PLAYBIT_CHANNEL before choosing either default root.
    let profile = args.next().map(std::path::absolute).transpose()?;
    if args.next().is_some() {
        return Err("expected at most one profile directory argument".into());
    }
    let profile = profile
        .as_ref()
        .map(|path| path.to_str().ok_or("profile path must be UTF-8"))
        .transpose()?;
    let executable = std::env::current_exe()?;
    let contents = executable.parent().and_then(std::path::Path::parent).ok_or("missing app Contents")?;
    let path = contents.join("Resources/app.pbapp");
    let path = path.to_str().ok_or("bundle path is not UTF-8")?;
    let app = playbit_api::native_app();
    // SAFETY: all buffers and callback code remain live until early failure or
    // AppKit process termination, which joins the worker and destroys app state.
    let status = unsafe {
        pb_mac_run_portable(
            profile.map_or(std::ptr::null(), str::as_ptr),
            profile.map_or(0, str::len).try_into()?,
            path.as_ptr(),
            path.len().try_into()?,
            PACKAGE_DIGEST.as_ptr(),
            &app,
        )
    };
    Err(format!("native host returned status {status}").into())
}
