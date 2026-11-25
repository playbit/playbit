#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use crate::release_channel::ReleaseChannel;
use installation::ReleaseIdentity;

fn identity(version: &str, sequence: u64) -> ReleaseIdentity {
    ReleaseIdentity { app_version: version.into(), sequence }
}

#[gpui::test]
fn account_menu_tracks_readiness_and_rechecks_before_deferred_upgrade(
    cx: &mut gpui::TestAppContext,
) {
    // A prepared update changes the retained account control, while a stale menu
    // callback cannot quit after readiness has been withdrawn.
    let (choices, _) = async_channel::bounded(1);
    cx.update(|cx| {
        cx.set_global(Updates {
            installation: None,
            state: State::Idle,
            busy: false,
            refresh_pending: false,
            _observer: None,
            prepared: None,
            explicit: false,
            reopen: false,
            quitting: false,
            dialog: None,
            choices,
        });
    });
    let (host, window, _) = crate::desktop::test_support::host(cx);
    let control = host.read_with(cx, |host, _| host.navigation_controls["account"].clone());
    let count = std::rc::Rc::new(std::cell::Cell::new(0));
    let observed = count.clone();
    cx.update(|cx| cx.on_action(move |_: &Upgrade, _| observed.set(observed.get() + 1)));
    for ready in [false, true] {
        cx.update(|cx| {
            cx.global_mut::<Updates>().prepared = ready.then(|| "0.1.1".into());
        });
        host.update(cx, |host, cx| {
            assert_eq!(host.account_button(cx), control);
            cx.notify();
        });
        cx.run_until_parked();
        control.read_with(cx, |control, _| {
            let (items, update, _) = control.popup_for_test();
            assert_eq!(update, ready.then_some(6));
            assert_eq!(
                items
                    .iter()
                    .filter(|item| matches!(item, crate::runtime::MenuEntry::Action { id: 6, .. }))
                    .count(),
                usize::from(ready)
            );
            if ready {
                assert!(matches!(items.last(), Some(crate::runtime::MenuEntry::Action {
                    id: 6, label, disabled: false, ..
                }) if label == "Update Playbit"));
            }
        });
        gpui::AnyWindowHandle::from(window)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                let (_, account) = window
                    .accessibility_nodes_for_test()
                    .find(|(_, node)| node.label() == Some("Account"))
                    .unwrap();
                assert_eq!(account.description(), ready.then_some("Update ready to install"));
                let bounds = account.bounds().unwrap();
                assert_eq!((bounds.width(), bounds.height()), (24., 24.));
            })
            .unwrap();
    }
    let activate = control.read_with(cx, |control, _| control.popup_for_test().2);
    cx.update(|cx| {
        activate(6, cx);
        assert_eq!(count.get(), 0, "activation waits until the popup has closed");
        cx.global_mut::<Updates>().prepared = None;
    });
    cx.run_until_parked();
    assert_eq!(count.get(), 0);
    cx.update(|cx| {
        cx.global_mut::<Updates>().prepared = Some("0.1.1".into());
        activate(6, cx);
        assert_eq!(count.get(), 0);
    });
    cx.run_until_parked();
    assert_eq!(count.get(), 1);
}

#[gpui::test]
fn personal_startup_receives_dismiss_without_starting_update_work(cx: &mut gpui::TestAppContext) {
    let (host, _, _) = crate::desktop::test_support::host(cx);
    cx.update(|cx| {
        setup_for(host.downgrade(), ReleaseChannel::FixtureA, cx);
        check(false, cx);
        start(Operation::Initialize(Some("1".into())), cx);
        start(Operation::Check, cx);
        let updates = cx.global_mut::<Updates>();
        assert!(matches!(updates.state, State::Disabled(DisabledReason::PersonalTesting)));
        assert!(!updates.busy);
        assert!(updates.installation.is_none());
        assert!(updates._observer.is_none());
        // This is the same channel send as the AppKit Dismiss button. GPUI's
        // test app does not instantiate a real native window.
        updates.explicit = true;
        updates.choices.try_send(Choice::Dismiss).unwrap();
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let updates = cx.global::<Updates>();
        assert!(!updates.explicit);
        assert!(!ready(cx));
    });
}

#[test]
fn development_updates_require_an_exact_override() {
    // Exercise admission without touching the process environment or any app.
    use std::{ffi::OsStr, path::Path};
    let executable = Path::new("/tmp/playbit-updater-test/target/release/playbit");
    assert!(!package::enabled(executable, None).unwrap());
    for value in ["", "0", "true", "yes", "01", "1 ", " 1"] {
        assert!(!package::enabled(executable, Some(OsStr::new(value))).unwrap());
    }
    assert!(package::enabled(executable, Some(OsStr::new("1"))).unwrap());
}

#[test]
fn unsigned_bundles_are_disabled_even_outside_applications() {
    // Gate the running bundle independently of installation admission, and
    // confirm the override cannot admit an unsigned update for installation.
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("Playbit.app");
    let executable = app.join("Contents/MacOS/Playbit");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::copy("/bin/echo", &executable).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
          CFBundleIdentifier</key><string>playbit.engine</string><key>\
          CFBundleExecutable</key><string>Playbit</string><key>\
          CFBundlePackageType</key><string>APPL</string></dict></plist>",
    )
    .unwrap();
    assert!(!package::enabled(&executable, None).unwrap());
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!package::enabled(&executable, None).unwrap());
    assert!(package::enabled(&executable, Some(std::ffi::OsStr::new("1"))).unwrap());
    let installation = Installation::for_executable(&executable).unwrap().unwrap();
    assert!(installation.ensure_installable().is_err());
    assert!(package::verify(&app, &ReleaseIdentity::running()).is_err());
}

#[gpui::test]
fn pending_and_disabled_updates_cannot_start_work(cx: &mut gpui::TestAppContext) {
    // A check before initialization or a readiness/prepare request in a disabled
    // build must not contact the feed or open installation state.
    cx.update(|cx| {
        let (choices, _) = async_channel::bounded(1);
        cx.set_global(Updates {
            installation: None,
            state: State::Starting,
            busy: false,
            refresh_pending: false,
            _observer: None,
            prepared: None,
            explicit: false,
            reopen: false,
            quitting: false,
            dialog: None,
            choices,
        });
        for disabled in [false, true] {
            if disabled {
                complete(Ok(Response::Disabled), cx);
            }
            check(false, cx);
            start(Operation::Refresh, cx);
            start(
                Operation::Prepare(
                    package::Release::parse_download(&download(&release()), ReleaseChannel::Public)
                        .unwrap(),
                ),
                cx,
            );
            let updates = cx.global::<Updates>();
            assert!(!updates.busy);
            assert!(updates._observer.is_none());
            assert!(updates.dialog.is_none());
            assert!(updates.installation.is_none());
            assert!(!ready(cx));
        }
    });
}

#[test]
#[ignore = "requires PLAYBIT_TEST_NOTARIZED_APP pointing to a notarized Playbit bundle"]
fn notarized_bundle_enables_updates_without_the_override() {
    // Use a real release ticket; a signing-only fixture cannot prove notarization.
    let app = std::path::PathBuf::from(std::env::var_os("PLAYBIT_TEST_NOTARIZED_APP").unwrap());
    assert!(package::enabled(&app.join("Contents/MacOS/Playbit"), None).unwrap());
}

#[test]
fn initialization_disables_an_unbundled_development_executable() {
    // The actual initialization path must stop before discovery or recovery.
    assert!(matches!(execute(None, Operation::Initialize(None)).unwrap(), Response::Disabled));
    assert!(matches!(
        execute(None, Operation::Initialize(Some("1".into()))).unwrap(),
        Response::Initialized(None, None, None)
    ));
}

#[test]
fn discovered_update_failures_notify_with_the_version_and_recovery() {
    // Automatic preparation and installation failures must become visible, while
    // an offline release check remains quiet unless the user requested it.
    for state in [State::Preparing("0.1.0-beta.10".into()), State::Ready("0.1.0-beta.10".into())] {
        let failure = UpdateFailure::new(&state, installation::LOCATION_ERROR.into());
        assert!(failure.notify);
        assert!(failure.message.contains("Playbit 0.1.0-beta.10 is available"));
        assert!(failure.message.contains("Move Playbit to your Applications folder"));
        assert!(failure.message.contains("download and install the update manually"));
        assert_eq!(
            failure.diagnostic,
            format!("could not install Playbit 0.1.0-beta.10: {}", installation::LOCATION_ERROR)
        );
        assert_eq!(failure.diagnostic.lines().count(), 1);
    }
    let failure = UpdateFailure::new(&State::Checking, "offline".into());
    assert!(!failure.notify);
    assert_eq!(failure.diagnostic, "offline");
}

#[test]
fn release_sequence_orders_builds_without_a_semver_change() {
    let previous = identity("0.1.0+aaaa", 10);
    assert!(package::newer(&identity("0.1.0+bbbb", 11), &previous).unwrap());
    assert!(!package::newer(&identity("0.2.0", 9), &previous).unwrap());
    assert!(!package::newer(&identity("0.2.0", 10), &previous).unwrap());
    assert!(package::newer(&identity("1.0", 11), &previous).is_err());
    assert!(package::newer(&identity("0.2.0", u64::MAX), &previous).is_err());
}

fn release() -> serde_json::Value {
    serde_json::json!({"app_version":"0.1.0-beta.10", "sequence":10, "bundle_id":"playbit.engine",
        "url":format!("https://blob.playbit.app/{}", crate::identifier::encode(&[0; 32])),
        "filename":"Playbit-macos-arm64.zip", "sha256":"a".repeat(64), "bytes":42,
        "platform":"macos-arm64", "revision":"b".repeat(40), "checks_sha256":"c".repeat(64)})
}

fn download(artifact: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"channel":"public", "artifact":artifact})).unwrap()
}

#[test]
fn download_manifests_bind_artifacts_to_the_selected_channel() {
    use crate::release_channel::ReleaseChannel;
    let bytes = serde_json::to_vec(&serde_json::json!({"channel":"testing", "artifact":release()}))
        .unwrap();
    assert!(package::Release::parse_download(&bytes, ReleaseChannel::Testing).is_ok());
    assert!(package::Release::parse_download(&bytes, ReleaseChannel::Public).is_err());
    let mut invalid = release();
    invalid["sequence"] = serde_json::json!(0);
    let bytes =
        serde_json::to_vec(&serde_json::json!({"channel":"testing", "artifact":invalid})).unwrap();
    assert!(package::Release::parse_download(&bytes, ReleaseChannel::Testing).is_err());
}

#[test]
fn manifest_rejects_invalid_identity_sizes_and_versions() {
    let value = release();
    assert!(package::Release::parse_download(&download(&value), ReleaseChannel::Public).is_ok());
    for (key, invalid) in [
        ("bytes", serde_json::json!(0)),
        ("bytes", serde_json::json!(archive::MAX_ZIP + 1)),
        ("sequence", serde_json::json!(0)),
        ("sequence", serde_json::json!(u64::MAX)),
        ("url", serde_json::json!("https://example.test/archive.zip")),
        ("url", serde_json::json!("https://blob.playbit.app/invalid?secret")),
        ("app_version", serde_json::json!("0.1")),
        ("sha256", serde_json::json!("wrong")),
        ("filename", serde_json::json!("../other.zip")),
        ("bundle_id", serde_json::json!("another.app")),
        ("platform", serde_json::json!("macos-x86_64")),
    ] {
        let mut changed = value.clone();
        changed[key] = invalid;
        assert!(
            package::Release::parse_download(&download(&changed), ReleaseChannel::Public).is_err(),
            "{key}"
        );
    }
    assert!(package::Release::parse_download(b"{", ReleaseChannel::Public).is_err());
    assert!(package::Release::parse_download(&vec![b' '; 65537], ReleaseChannel::Public).is_err());
}

fn zip_file(path: &std::path::Path, names: &[(&str, &[u8])]) {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, content) in names {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default().unix_permissions(0o755))
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn extraction_preserves_bytes_and_executable_mode() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("update.zip");
    let thumbnail =
        "Playbit.app/Contents/PlugIns/PlaybitThumbnail.appex/Contents/MacOS/PlaybitThumbnail";
    zip_file(
        &archive,
        &[
            ("Playbit.app/Contents/MacOS/Playbit", b"executable"),
            (thumbnail, b"thumbnail executable"),
        ],
    );
    let output = root.path().join("incoming");
    archive::extract(&archive, &output).unwrap();
    let binary = output.join("Playbit.app/Contents/MacOS/Playbit");
    assert_eq!(std::fs::read(&binary).unwrap(), b"executable");
    assert_eq!(std::fs::metadata(binary).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::read(output.join(thumbnail)).unwrap(), b"thumbnail executable");
}

#[test]
fn extraction_checks_all_paths_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("update.zip");
    for name in [
        "../outside",
        "/Playbit.app/absolute",
        "Playbit.app/../escape",
        "Playbit.app/Contents/../../escape",
        "Other.app/file",
        "Playbit.app/./alias",
        "Playbit.app//alias",
        "Playbit.app/Contents/FILE",
        "Playbit.app/contents/other",
        "Playbit.app/Contents/file/child",
    ] {
        zip_file(&archive, &[("Playbit.app/Contents/file", b"ok"), (name, b"bad")]);
        let output = root.path().join("incoming");
        assert!(archive::extract(&archive, &output).is_err(), "{name}");
        assert!(!output.exists(), "nothing should be extracted before all paths are admitted");
    }
}

#[test]
fn extraction_rejects_links_and_oversized_directory_before_extracting() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("update.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer
        .add_symlink(
            "Playbit.app/link",
            "/private/tmp/escape",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer.finish().unwrap();
    assert!(archive::extract(&archive, &root.path().join("incoming")).is_err());
    zip_file(&archive, &[("Playbit.app/file", b"content")]);
    let mut bytes = std::fs::read(&archive).unwrap();
    let end = bytes.len() - 22;
    bytes[end + 8..end + 12].copy_from_slice(&[1, 1, 1, 1]);
    std::fs::write(&archive, bytes).unwrap();
    assert!(archive::extract(&archive, &root.path().join("incoming")).is_err());
}

#[test]
fn unsigned_bundle_cannot_be_admitted() {
    let root = tempfile::tempdir().unwrap();
    assert!(package::verify(root.path(), &identity("0.1.0-beta.10", 10)).is_err());
}

#[test]
fn journal_rejects_other_channels_and_nonincreasing_builds() {
    use crate::release_channel::ReleaseChannel;
    let root = tempfile::tempdir().unwrap();
    let installation =
        Installation { app: root.path().join("Playbit.app"), state: root.path().join("state") };
    installation::private_directory(&installation.state).unwrap();
    let mut journal = install::Journal {
        channel: ReleaseChannel::Testing,
        from: identity("0.1.0+aaaa", 9),
        to: identity("0.1.0+bbbb", 10),
        phase: install::Phase::Ready,
        adjacent: None,
    };
    journal.write(&installation).unwrap();
    assert_eq!(
        install::Journal::read(&installation, ReleaseChannel::Testing).unwrap().as_ref(),
        Some(&journal)
    );
    assert!(install::Journal::read(&installation, ReleaseChannel::Public).is_err());
    for sequence in [0, 8, 9, u64::MAX] {
        journal.to.sequence = sequence;
        journal.write(&installation).unwrap();
        assert!(install::Journal::read(&installation, ReleaseChannel::Testing).is_err());
    }
}

// These tests use an operator-selected real Developer ID certificate. They never
// replace an installed app, contact the release service, or open user profiles.
#[test]
#[ignore = "requires PLAYBIT_TEST_SIGN_IDENTITY in the macOS Keychain"]
fn developer_id_signature_identity_resources_and_recovery() {
    use std::{fs, path::Path, process::Command};
    fn sign(app: &Path, version: &str, id: &str, minimum: &str, sequence: u64) {
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::copy("/bin/echo", app.join("Contents/MacOS/Playbit")).unwrap();
        fs::write(app.join("Contents/Resources/data"), b"signed resource").unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
                 CFBundleIdentifier</key><string>{id}</string><key>CFBundleExecutable</key>\
                 <string>Playbit</string><key>CFBundlePackageType</key><string>APPL</string>\
                 <key>PlaybitVersion</key><string>{version}</string><key>\
                 PlaybitReleaseSequence</key><string>{sequence}</string><key>\
                 LSMinimumSystemVersion</key><string>{minimum}</string></dict></plist>"
            ),
        )
        .unwrap();
        let extension = app.join("Contents/PlugIns/PlaybitThumbnail.appex");
        fs::create_dir_all(extension.join("Contents/MacOS")).unwrap();
        fs::copy("/bin/echo", extension.join("Contents/MacOS/PlaybitThumbnail")).unwrap();
        let extension_id = format!("{id}.thumbnail");
        fs::write(
            extension.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
                 CFBundleIdentifier</key><string>{extension_id}</string><key>\
                 CFBundleExecutable</key><string>PlaybitThumbnail</string><key>\
                 CFBundlePackageType</key><string>XPC!</string></dict></plist>"
            ),
        )
        .unwrap();
        let mut entitlements = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut entitlements,
            b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
              com.apple.security.app-sandbox</key><true/></dict></plist>",
        )
        .unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--timestamp=none", "--sign"])
                .arg(std::env::var("PLAYBIT_TEST_SIGN_IDENTITY").unwrap())
                .args(["--identifier", &extension_id, "--entitlements"])
                .arg(entitlements.path())
                .arg(&extension)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--timestamp=none", "--sign"])
                .arg(std::env::var("PLAYBIT_TEST_SIGN_IDENTITY").unwrap())
                .args(["--identifier", id])
                .arg(app)
                .status()
                .unwrap()
                .success()
        );
    }
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("Playbit.app");
    let id = "playbit.engine";
    sign(&app, "1.0.0", id, "15.0", 1);
    assert!(!package::enabled(&app.join("Contents/MacOS/Playbit"), None).unwrap());
    assert!(
        Command::new("/usr/bin/codesign")
            .args([
                "--verify",
                "--deep",
                "--strict",
                "--all-architectures",
                "--verbose=4",
                "-R",
                package::REQUIREMENT
            ])
            .arg(&app)
            .status()
            .unwrap()
            .success()
    );
    package::verify(&app, &identity("1.0.0", 1)).unwrap();
    assert!(package::verify(&app, &identity("1.0.0", 2)).is_err());
    assert!(package::verify(&app, &identity("1.0.1", 2)).is_err());
    fs::write(app.join("Contents/Resources/data"), b"tampered").unwrap();
    assert!(package::verify(&app, &identity("1.0.0", 1)).is_err());
    sign(&app, "1.0.0", id, "15.0", 1);
    fs::write(
        app.join("Contents/PlugIns/PlaybitThumbnail.appex/Contents/MacOS/PlaybitThumbnail"),
        b"tampered",
    )
    .unwrap();
    assert!(package::verify(&app, &identity("1.0.0", 1)).is_err());
    sign(&app, "1.0.0", "another.app", "15.0", 1);
    assert!(package::verify(&app, &identity("1.0.0", 1)).is_err());
    sign(&app, "1.0.0", id, "999.0", 1);
    assert!(package::verify(&app, &identity("1.0.0", 1)).is_err());
    sign(&app, "1.0.0", id, "15.0", 1);
    assert!(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app)
            .status()
            .unwrap()
            .success()
    );
    assert!(package::verify(&app, &identity("1.0.0", 1)).is_err());
    sign(&app, "1.0.0", id, "15.0", 1);
    let installation = Installation { app, state: root.path().join("state") };
    installation.open().unwrap();
    let staged = installation.state.join("staged.app");
    sign(&staged, "1.0.0", id, "15.0", 2);
    install::arm(
        &installation,
        &identity("1.0.0", 2),
        crate::release_channel::ReleaseChannel::Public,
    )
    .unwrap();
    let mut journal =
        install::Journal::read(&installation, crate::release_channel::ReleaseChannel::Public)
            .unwrap()
            .unwrap();
    install::recover(&installation, &mut journal).unwrap();
    assert_eq!(journal.phase, install::Phase::Ready);
    journal.phase = install::Phase::Swapping;
    journal.write(&installation).unwrap();
    install::recover(&installation, &mut journal).unwrap();
    assert_eq!(installation.release().unwrap(), identity("1.0.0", 1));
    install::swap(&installation.app, &staged).unwrap();
    // Crash after exchange but before recording it: finish the transaction.
    install::recover(&installation, &mut journal).unwrap();
    assert_eq!(journal.phase, install::Phase::Installed);
    assert_eq!(installation.release().unwrap(), identity("1.0.0", 2));
    package::verify(&installation.state.join("previous.app"), &identity("1.0.0", 1)).unwrap();
    for bundle in [&installation.app, &installation.state.join("previous.app")] {
        assert!(
            bundle
                .join("Contents/PlugIns/PlaybitThumbnail.appex/Contents/MacOS/PlaybitThumbnail")
                .is_file()
        );
    }
    install::recover(&installation, &mut journal).unwrap();
    // Crash after retaining the previous bundle but before journal completion.
    journal.phase = install::Phase::Swapping;
    journal.write(&installation).unwrap();
    install::recover(&installation, &mut journal).unwrap();
    assert_eq!(installation.release().unwrap(), identity("1.0.0", 2));
    assert_eq!(journal.phase, install::Phase::Installed);
}

#[test]
fn transfer_rejects_truncation_excess_tampering_and_io_failure() {
    use std::io::{self, Read};
    let mut release =
        package::Release::parse_download(&download(&release()), ReleaseChannel::Public).unwrap();
    release.bytes = 3;
    release.url = format!(
        "https://blob.playbit.app/{}",
        crate::identifier::encode(blake3::hash(b"abc").as_bytes())
    );
    release.sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into();
    let mut output = Vec::new();
    package::transfer(&mut &b"abc"[..], &mut output, &release).unwrap();
    assert_eq!(output, b"abc");
    let mut wrong_address = release.clone();
    wrong_address.url = format!("https://blob.playbit.app/{}", crate::identifier::encode(&[0; 32]));
    assert!(package::transfer(&mut &b"abc"[..], &mut Vec::new(), &wrong_address).is_err());
    for bytes in [&b"ab"[..], &b"abcd"[..], &b"abd"[..]] {
        assert!(package::transfer(&mut &*bytes, &mut Vec::new(), &release).is_err());
    }
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
    assert!(package::transfer(&mut Broken, &mut Vec::new(), &release).is_err());
}

#[test]
fn failed_exchange_leaves_the_current_bundle_in_place() {
    let root = tempfile::tempdir().unwrap();
    let current = root.path().join("current.app");
    std::fs::create_dir(&current).unwrap();
    std::fs::write(current.join("data"), b"current").unwrap();
    assert!(install::swap(&current, &root.path().join("absent.app")).is_err());
    assert_eq!(std::fs::read(current.join("data")).unwrap(), b"current");
}

#[test]
fn duplicate_entries_and_lying_sizes_are_rejected_before_extraction() {
    // Patch the wire bytes: ZipWriter itself refuses duplicate file names.
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("update.zip");
    let output = root.path().join("incoming");
    zip_file(&archive, &[("Playbit.app/file1", b"one"), ("Playbit.app/file2", b"two")]);
    let original = std::fs::read(&archive).unwrap();
    let mut duplicate = original.clone();
    for offset in 0..duplicate.len() - 5 {
        if duplicate[offset..].starts_with(b"file2") {
            duplicate[offset + 4] = b'1';
        }
    }
    std::fs::write(&archive, duplicate).unwrap();
    assert!(archive::extract(&archive, &output).is_err());
    assert!(!output.exists());
    let mut oversized = original.clone();
    let central = oversized.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    oversized[central + 24..central + 28]
        .copy_from_slice(&((archive::MAX_EXPANDED + 1) as u32).to_le_bytes());
    std::fs::write(&archive, oversized).unwrap();
    assert!(archive::extract(&archive, &output).is_err());
    assert!(!output.exists());
    let mut encrypted = original;
    encrypted[central + 8] |= 1;
    encrypted[6] |= 1;
    std::fs::write(&archive, encrypted).unwrap();
    assert!(archive::extract(&archive, &output).is_err());
    assert!(!output.exists());
}
