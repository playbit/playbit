use playbit::engine::ffi::{AppOutput, AppStep};
use playbit_api::ui::{CheckState, TextStyle};
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--layout") {
        println!(
            "{},{},{},{},{},{}",
            size_of::<AppOutput>(),
            align_of::<AppOutput>(),
            std::mem::offset_of!(AppOutput, user_data),
            size_of::<AppStep>(),
            align_of::<AppStep>(),
            std::mem::offset_of!(AppStep, flags)
        );
        return;
    }
    if let Some(path) = std::env::args().nth(1) {
        assert_eq!(std::env::args().len(), 2, "one disposable profile path");
        let store = playbit::store::LocalStore::open(&path).expect("open C probe profile");
        let declaration = playbit::store::RootInitialization::from_json(
            br#"{
            "permissions":{"grants":[]},
            "databases":[{"id":"2d7dWtQMvj9WttA3mMnX","name":"records",
                "kind":{"type":"object_db","schema":{"types":{}},"objects":[]}}],
            "blobs":[]
        }"#,
        )
        .expect("C probe declaration");
        let root =
            store.prepare_app_root("C native", &declaration).expect("prepare C probe root");
        std::fs::write(std::path::Path::new(&path).join("c-probe-root.bin"), root.id.as_bytes())
            .expect("write binary C probe root identity");
        return;
    }
    // UI bytes and disposable profile setup only; C owns runtime calls and app state.
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
