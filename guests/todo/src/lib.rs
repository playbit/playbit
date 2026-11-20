mod app;

#[allow(unsafe_code, reason = "The SDK resolves this app registration by its ABI name.")]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(app::App::new(), app::App::turn, true)
}
