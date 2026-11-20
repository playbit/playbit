#![allow(clippy::unwrap_used, reason = "Tests assert successful temporary source fixtures.")]

#[path = "../build.rs"]
mod build_identity;

#[test]
fn channel_constants_reject_shared_authority_and_oversized_definitions() {
    // The shared browser/native definition is admitted before emitting code;
    // a bad source cannot silently leave a different environment's constants.
    let root = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("playbit/src")).unwrap();
    let file = root.path().join("playbit/src/channels.json");
    let original = include_bytes!("../src/channels.json");
    std::fs::write(&file, original).unwrap();
    build_identity::channel_endpoints(root.path(), output.path()).unwrap();
    let emitted = std::fs::read(output.path().join("release_channel_endpoints.rs")).unwrap();
    let mut invalid: serde_json::Value = serde_json::from_slice(original).unwrap();
    invalid["testing"]["ap_hostname"] = invalid["public"]["ap_hostname"].clone();
    for data in [serde_json::to_vec(&invalid).unwrap(), vec![b' '; 4097]] {
        std::fs::write(&file, data).unwrap();
        assert!(build_identity::channel_endpoints(root.path(), output.path()).is_err());
        assert_eq!(
            std::fs::read(output.path().join("release_channel_endpoints.rs")).unwrap(),
            emitted
        );
    }
}

#[test]
fn identity_is_reproducible_and_changes_for_source_or_dependency_edits() {
    use std::fs;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for root in [first.path(), second.path()] {
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/wire.rs"), "enum Request { Stop, Run }").unwrap();
        fs::write(root.join("Cargo.toml"), "rkyv = '=0.8.18'").unwrap();
    }
    let inputs = ["src", "Cargo.toml"];
    let identity = build_identity::source_identity(first.path(), &inputs).unwrap();
    assert_eq!(
        identity,
        build_identity::source_identity(second.path(), &["Cargo.toml", "src"]).unwrap()
    );
    fs::write(second.path().join("src/wire.rs"), "enum Request { Run, Stop }").unwrap();
    assert_ne!(identity, build_identity::source_identity(second.path(), &inputs).unwrap());
    fs::write(second.path().join("src/wire.rs"), "enum Request { Stop, Run }").unwrap();
    fs::write(second.path().join("Cargo.toml"), "rkyv = '=0.8.19'").unwrap();
    assert_ne!(identity, build_identity::source_identity(second.path(), &inputs).unwrap());
    fs::write(second.path().join("Cargo.toml"), "rkyv = '=0.8.18'").unwrap();
    fs::write(second.path().join("src/new.rs"), "struct Added;").unwrap();
    assert_ne!(identity, build_identity::source_identity(second.path(), &inputs).unwrap());
}

#[test]
fn identity_ignores_editor_locks_metadata_and_symlinks() {
    use std::{fs, os::unix::fs::symlink};
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/wire.rs"), "struct Request;").unwrap();
    let identity = build_identity::source_identity(root.path(), &["src"]).unwrap();
    symlink("missing", root.path().join("src/.#wire.rs")).unwrap();
    symlink("missing", root.path().join("src/dangling.rs")).unwrap();
    symlink("wire.rs", root.path().join("src/alias.rs")).unwrap();
    fs::write(root.path().join("src/._wire.rs"), "metadata").unwrap();
    fs::create_dir(root.path().join("src/.editor")).unwrap();
    fs::write(root.path().join("src/.editor/snapshot.rs"), "unsaved").unwrap();
    assert_eq!(identity, build_identity::source_identity(root.path(), &["src"]).unwrap());
}

#[test]
fn unreadable_identity_input_names_its_path() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("missing.rs");
    let error = build_identity::source_identity(root.path(), &["missing.rs"]).unwrap_err();
    assert!(error.to_string().contains(path.to_str().unwrap()));
}
