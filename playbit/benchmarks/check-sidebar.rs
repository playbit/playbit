//! Isolated production sidebar/session preview with optional numbered PNG app icons.
fn main() {
    assert!(std::env::var_os("PLAYBIT_PROFILE").is_some(), "isolated preview profile required");
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let (icons, session) = if first.as_deref() == Some("--session") {
        let mode = args.next().expect("active, stopped, empty or new session preview");
        assert!(matches!(
            mode.as_str(),
            "active" | "stopped" | "empty" | "new" | "benchmark" | "loading" | "builds"
        ));
        (args.next().map(Into::into), Some(mode))
    } else {
        (first.map(Into::into), None)
    };
    assert!(args.next().is_none(), "unexpected preview argument");
    playbit::desktop::main_sidebar_preview(icons, session);
}
