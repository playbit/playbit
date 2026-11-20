#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;

fn fixture(root: &Path) -> std::path::PathBuf {
    let app = root.join("Playbit.app");
    for (path, bytes, mode) in [
        ("Contents/MacOS/Playbit", &b"executable"[..], 0o755),
        ("Contents/Info.plist", &b"signed bundle metadata"[..], 0o644),
        ("Contents/_CodeSignature/CodeResources", &b"signature resources"[..], 0o644),
        ("Contents/CodeResources", &b"stapled ticket"[..], 0o644),
        (
            "Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine",
            &b"engine executable"[..],
            0o755,
        ),
    ] {
        let target = app.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, bytes).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
    }
    fs::create_dir(app.join("Contents/Resources")).unwrap();
    app
}

fn assert_copied(source: &Path, destination: &Path) {
    for path in [
        "Contents/MacOS/Playbit",
        "Contents/Info.plist",
        "Contents/_CodeSignature/CodeResources",
        "Contents/CodeResources",
        "Contents/Helpers/playbit-engine.app/Contents/MacOS/playbit-engine",
    ] {
        assert_eq!(fs::read(source.join(path)).unwrap(), fs::read(destination.join(path)).unwrap());
        assert_eq!(
            fs::metadata(source.join(path)).unwrap().mode() & 0o777,
            fs::metadata(destination.join(path)).unwrap().mode() & 0o777,
        );
    }
    assert!(destination.join("Contents/Resources").is_dir());
    assert_eq!(fs::metadata(destination).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn bundle_copy_preserves_signing_resources_and_executable_modes() {
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let destination = root.path().join("copy.app");
    bundle(&source, &destination).unwrap();
    assert_copied(&source, &destination);
}

#[test]
fn unsafe_copy_sources_fail_before_creating_a_destination_file() {
    use std::os::unix::fs::symlink;
    for kind in ["symlink", "hardlink", "directory", "fifo", "oversized"] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let target = root.path().join("target");
        fs::write(&target, b"preserved").unwrap();
        match kind {
            "symlink" => symlink(&target, &source).unwrap(),
            "hardlink" => fs::hard_link(&target, &source).unwrap(),
            "directory" => fs::create_dir(&source).unwrap(),
            "fifo" => {
                assert!(
                    std::process::Command::new("/usr/bin/mkfifo")
                        .args(["-m", "600"])
                        .arg(&source)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            "oversized" => {
                File::create(&source).unwrap().set_len(MAX_EXPANDED + 1).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(file(&source, &destination, MAX_EXPANDED).is_err(), "{kind}");
        assert!(!destination.exists(), "{kind}");
        assert_eq!(fs::read(&target).unwrap(), b"preserved");
    }
}

#[test]
fn copy_refuses_existing_destinations_without_truncating_them() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let link = root.path().join("link");
    fs::write(&source, b"replacement").unwrap();
    fs::write(&destination, b"preserved").unwrap();
    symlink(&destination, &link).unwrap();
    for target in [&destination, &link] {
        assert!(file(&source, target, MAX_EXPANDED).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"preserved");
    }
    let app = fixture(root.path());
    assert!(bundle(&app, &app).is_err());
    assert_eq!(fs::read(app.join("Contents/MacOS/Playbit")).unwrap(), b"executable");
}

#[test]
fn bundle_copy_rejects_root_and_nested_directory_symlinks() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let source = fixture(root.path());
    let alias = root.path().join("alias.app");
    let destination = root.path().join("destination.app");
    symlink(&source, &alias).unwrap();
    assert!(bundle(&alias, &destination).is_err());
    assert!(!destination.exists());
    let external = root.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("private"), b"unrelated data").unwrap();
    symlink(&external, source.join("Contents/External")).unwrap();
    assert!(bundle(&source, &destination).is_err());
    assert!(!destination.join("Contents/External/private").exists());
    assert_eq!(fs::read(external.join("private")).unwrap(), b"unrelated data");
}

#[test]
fn bundle_copy_enforces_entry_and_size_limits() {
    for oversized in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.app");
        fs::create_dir(&source).unwrap();
        if oversized {
            File::create(source.join("large")).unwrap().set_len(MAX_EXPANDED + 1).unwrap();
        } else {
            for index in 0..=MAX_ENTRIES {
                fs::write(source.join(index.to_string()), b"").unwrap();
            }
        }
        assert!(bundle(&source, &root.path().join("destination.app")).is_err());
    }
}

#[test]
fn admitted_zip_with_maximum_files_and_implicit_directories_can_be_copied() {
    let root = tempfile::tempdir().unwrap();
    let zip_path = root.path().join("update.zip");
    let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
    for index in 0..MAX_ENTRIES {
        zip.start_file(
            format!("Playbit.app/Contents/Resources/{index}/item"),
            zip::write::SimpleFileOptions::default().unix_permissions(0o644),
        )
        .unwrap();
        zip.write_all(b"resource").unwrap();
    }
    zip.finish().unwrap();
    let unpacked = root.path().join("unpacked");
    super::super::archive::extract(&zip_path, &unpacked).unwrap();
    let source = unpacked.join("Playbit.app");
    let destination = root.path().join("copied.app");
    bundle(&source, &destination).unwrap();
    for index in 0..MAX_ENTRIES {
        assert_eq!(
            fs::read(destination.join(format!("Contents/Resources/{index}/item"))).unwrap(),
            b"resource"
        );
    }
    fs::write(source.join("overflow"), b"one file too many").unwrap();
    assert!(bundle(&source, &root.path().join("over-limit.app")).is_err());
}

#[test]
#[ignore = "requires PLAYBIT_TEST_UPDATE_VOLUME pointing to a writable different filesystem"]
fn bundle_copy_crosses_actual_filesystems() {
    let root = tempfile::tempdir().unwrap();
    let volume = std::env::var_os("PLAYBIT_TEST_UPDATE_VOLUME").unwrap();
    let other = tempfile::tempdir_in(volume).unwrap();
    assert_ne!(fs::metadata(root.path()).unwrap().dev(), fs::metadata(other.path()).unwrap().dev());
    let source = fixture(root.path());
    let destination = other.path().join("Playbit.app");
    bundle(&source, &destination).unwrap();
    assert_copied(&source, &destination);
}
