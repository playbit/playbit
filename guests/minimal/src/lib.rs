#[cfg(not(target_arch = "wasm32"))]
extern crate playbit;

use playbit_api::context::Context;
use playbit_api::ui::{ButtonStyle, ViewEncoder};

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app((), turn, true)
}

fn turn(_: &mut (), context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    view.column_start();
    for _ in 0..view.button(1, "Click me", ButtonStyle::Default, false).click_count() {
        context.log("Button clicked");
    }
    view.column_end();
    view.submit(context).unwrap();
}
