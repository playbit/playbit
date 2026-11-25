#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;

#[test]
fn every_file_boundary_retains_identity_and_recovers_without_replacing_edits() {
    // Drop every attempt and reopen its on-disk intent. Include partial writes,
    // the first intent before any manifest, and every individual publication.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let paths =
        template("Recovery", "recovery", &executable, playbit::package::Language::Rust).unwrap();
    let mut failures =
        vec![INTENT.to_string(), "writing:.pb-init.json".into(), "published:.pb-init.json".into()];
    for path in paths.keys() {
        failures.push(path.to_str().unwrap().into());
        failures.push(format!("writing:{}", path.file_name().unwrap().to_str().unwrap()));
        failures.push(format!("published:{}", path.file_name().unwrap().to_str().unwrap()));
    }
    for failure in failures {
        let source = tempfile::tempdir().unwrap();
        INTERRUPT.with_borrow_mut(|value| *value = failure.clone());
        let attempt = (|| -> Result<()> {
            let mut attempt =
                Initialization::begin(source.path(), Some("Recovery"), &executable, source.path())?;
            attempt.scaffold()
        })();
        INTERRUPT.with_borrow_mut(String::clear);
        assert!(attempt.is_err(), "{failure}");
        let path = source.path().join(INTENT);
        if failure == "writing:.pb-init.json" {
            assert_eq!(fs::read_dir(source.path()).unwrap().count(), 0);
        }
        let retained = path.exists().then(|| fs::read(&path).unwrap());
        let mut resumed =
            Initialization::begin(source.path(), Some("Recovery"), &executable, source.path())
                .unwrap();
        let root = resumed.original_manifest().unwrap().app_id;
        if let Some(retained) = retained {
            assert_eq!(fs::read(&path).unwrap(), retained);
        }
        resumed.scaffold().unwrap();
        resumed.validate().unwrap();
        assert_eq!(Manifest::read(source.path()).unwrap().app_id, root.to_string());
        drop(resumed);
        // Fully prepared source may be edited while the engine or Git is unavailable.
        fs::write(source.path().join("src/app.rs"), "// User work\n").unwrap();
        let mut resumed =
            Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
        resumed.scaffold().unwrap();
        resumed.validate().unwrap();
        assert_eq!(resumed.original_manifest().unwrap().app_id, root);
        assert_eq!(fs::read_to_string(source.path().join("src/app.rs")).unwrap(), "// User work\n");
    }
}

#[test]
fn conflicts_are_preserved_and_explicitly_recoverable() {
    // A conflicting partial file is never overwritten. Moving it aside permits
    // completion with the same ID, after which its edits can be restored.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let source = tempfile::tempdir().unwrap();
    let mut attempt =
        Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    let root = attempt.original_manifest().unwrap().app_id;
    fs::write(source.path().join("Cargo.toml"), "user work").unwrap();
    let error = attempt.scaffold().unwrap_err().to_string();
    assert!(error.contains("Move that file aside"), "{error}");
    assert_eq!(fs::read_to_string(source.path().join("Cargo.toml")).unwrap(), "user work");
    fs::rename(source.path().join("Cargo.toml"), source.path().join("Cargo.saved")).unwrap();
    attempt.scaffold().unwrap();
    assert_eq!(attempt.original_manifest().unwrap().app_id, root);
    assert_eq!(fs::read_to_string(source.path().join("Cargo.saved")).unwrap(), "user work");
    let original = fs::read(source.path().join("playbit.toml")).unwrap();
    let mut manifest = Manifest::read(source.path()).unwrap();
    manifest.app_id = "changed.identity".into();
    manifest.write(source.path()).unwrap();
    assert!(attempt.validate().unwrap_err().to_string().contains("retained identity"));
    fs::write(source.path().join("playbit.toml"), original).unwrap();
    let declaration = source.path().join(playbit::package::VOLUME_INITIALIZERS_FILE);
    let original = fs::read(&declaration).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
    changed["session"]["databases"][0]["name"] = "Changed".into();
    fs::write(&declaration, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(attempt.validate().unwrap_err().to_string().contains("retained declaration"));
    fs::write(declaration, original).unwrap();
    attempt.validate().unwrap();
}

#[test]
fn unrelated_completed_corrupt_and_concurrent_attempts_are_rejected() {
    // A manifest alone is not ownership; the directory lock spans the attempt.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("playbit.toml"), "user manifest").unwrap();
    assert!(Initialization::begin(source.path(), None, &executable, source.path()).is_err());
    fs::remove_file(source.path().join("playbit.toml")).unwrap();
    let mut attempt =
        Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    assert!(Initialization::begin(source.path(), None, &executable, source.path()).is_err());
    attempt.scaffold().unwrap();
    drop(attempt);
    assert!(
        Initialization::begin(source.path(), Some("Other name"), &executable, source.path())
            .is_err()
    );
    assert!(Initialization::begin(source.path(), None, &executable, install.path()).is_err());
    let attempt = Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    attempt.finish().unwrap();
    drop(attempt);
    assert!(Initialization::begin(source.path(), None, &executable, source.path()).is_err());
    fs::write(source.path().join(INTENT), "{").unwrap();
    assert!(Initialization::begin(source.path(), None, &executable, source.path()).is_err());
}

#[cfg(unix)]
#[test]
fn source_links_and_intent_paths_cannot_write_outside_the_project() {
    // Check both planned source ancestors and generated output ancestors.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let outside = tempfile::tempdir().unwrap();
    for relative in ["src", "Cargo.toml", "target"] {
        let source = tempfile::tempdir().unwrap();
        let mut attempt =
            Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
        std::os::unix::fs::symlink(outside.path(), source.path().join(relative)).unwrap();
        assert!(
            attempt.scaffold().and_then(|()| attempt.validate().map(|_| ())).is_err(),
            "{relative}"
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
    let source = tempfile::tempdir().unwrap();
    let attempt = Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), source.path().join(".git")).unwrap();
    attempt.initialize_git().unwrap();
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);

    for relative in ["../outside", "/absolute", "target/file", ".git/config", INTENT] {
        let source = tempfile::tempdir().unwrap();
        let mut attempt =
            Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
        attempt.intent.files.insert(relative.into(), Vec::new());
        assert!(attempt.check_intent().is_err(), "{relative}");
    }
}

#[test]
fn interrupted_prepared_marker_keeps_the_original_recovery_plan() {
    // A torn replacement must leave the old complete intent; no in-memory
    // prepared flag is needed to finish after the next process starts.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let source = tempfile::tempdir().unwrap();
    let mut attempt =
        Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    let root = attempt.original_manifest().unwrap().app_id;
    let before = fs::read(source.path().join(INTENT)).unwrap();
    INTERRUPT.with_borrow_mut(|value| *value = "writing:.pb-init.json".into());
    assert!(attempt.scaffold().is_err());
    INTERRUPT.with_borrow_mut(String::clear);
    drop(attempt);
    assert_eq!(fs::read(source.path().join(INTENT)).unwrap(), before);
    let mut attempt =
        Initialization::begin(source.path(), None, &executable, source.path()).unwrap();
    attempt.scaffold().unwrap();
    attempt.validate().unwrap();
    assert_eq!(attempt.original_manifest().unwrap().app_id, root);
}

#[test]
fn failed_exclusive_create_never_cleans_up_a_preexisting_file() {
    // Force the randomly selected staging name to be occupied immediately
    // before create_new. Cleanup must only remove files this attempt created.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("project");
    fs::create_dir(&source).unwrap();
    INTERRUPT.with_borrow_mut(|value| *value = "occupied staging".into());
    assert!(Initialization::begin(&source, None, &executable, parent.path()).is_err());
    INTERRUPT.with_borrow_mut(String::clear);
    let retained = fs::read_dir(parent.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1, "the preexisting staging-name collision must survive");
    assert_eq!(fs::read(&retained[0]).unwrap(), b"preexisting user file");
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    let mut attempt = Initialization::begin(&source, None, &executable, parent.path()).unwrap();
    attempt.scaffold().unwrap();
    attempt.validate().unwrap();
    assert_eq!(fs::read(&retained[0]).unwrap(), b"preexisting user file");
}

#[test]
fn invalid_new_declaration_does_not_claim_a_retained_identity() {
    // Validation before the first intent publication leaves no recoverable
    // state and must not tell the user that an identity has been saved.
    let install = fixture();
    let executable = install.path().join("Playbit.app/Contents/MacOS/pb");
    let declaration = resources(&executable).unwrap().join("project-template/playbit-volumes.json");
    fs::write(declaration, b"{").unwrap();
    let source = tempfile::tempdir().unwrap();
    let error = Initialization::begin(source.path(), None, &executable, source.path())
        .err()
        .expect("invalid declaration must fail")
        .to_string();
    assert!(error.contains("No project files or root were created"), "{error}");
    assert!(!error.contains("Initialization retained"), "{error}");
    assert_eq!(fs::read_dir(source.path()).unwrap().count(), 0);
}
