use playbit_api::ui::{CheckState, TextStyle};
fn main() {
    // Encode only fixed UI fixtures. Runtime app/host ownership and data work are C.
    for (name, checked, disabled) in [
        ("opening", false, true),
        ("off", false, false),
        ("on", true, false),
        ("pending_off", false, true),
        ("pending_on", true, true),
    ] {
        let mut scene_state = playbit_api::context::ContextState::default();
        let mut scene = scene_state.view();
        scene.column_start();
        if name == "opening" {
            scene.text(TextStyle::Body, "Opening…");
        } else {
            scene.checkbox(
                1,
                "Durable C app",
                &mut if checked { CheckState::On } else { CheckState::Off },
                disabled,
            );
            scene.checkbox(2, "Disabled", &mut CheckState::Off, true);
            if disabled {
                scene.text(TextStyle::Muted, "Updating…");
            }
        }
        scene.column_end();
        println!(
            "static const uint8_t {name}[] = {{ {} }};",
            scene.finish().iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        );
    }
}
