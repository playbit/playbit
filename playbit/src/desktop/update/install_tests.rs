#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;

fn sign(app: &Path, sequence: u64) {
    let identity = std::env::var_os("PLAYBIT_TEST_SIGN_IDENTITY").expect("test signing identity");
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::create_dir_all(app.join("Contents/Resources")).unwrap();
    fs::copy("/bin/echo", app.join("Contents/MacOS/Playbit")).unwrap();
    fs::write(app.join("Contents/Resources/data"), b"retained resource").unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>\
             CFBundleIdentifier</key><string>playbit.engine</string><key>\
             CFBundleExecutable</key><string>Playbit</string><key>\
             CFBundlePackageType</key><string>APPL</string><key>PlaybitVersion</key>\
             <string>1.0.0</string><key>PlaybitReleaseSequence</key><string>\
             {sequence}</string><key>LSMinimumSystemVersion</key><string>15.0</string>\
             </dict></plist>"
        ),
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--timestamp=none", "--sign"])
            .arg(identity)
            .arg(app)
            .output()
            .unwrap()
            .status
            .success()
    );
}

fn exchange_case(parent: &Path, interruption: u8) {
    let root = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir_in(parent).unwrap();
    let installation = Installation {
        app: destination.path().join("Playbit.app"),
        state: root.path().join("state"),
    };
    sign(&installation.app, 1);
    installation.open().unwrap();
    let staged = installation.state.join("staged.app");
    sign(&staged, 2);
    let to = ReleaseIdentity { app_version: "1.0.0".into(), sequence: 2 };
    arm(&installation, &to, ReleaseChannel::Testing).unwrap();
    let mut journal = Journal::read(&installation, ReleaseChannel::Testing).unwrap().unwrap();
    let candidate = prepare_exchange(&installation, &mut journal).unwrap();
    let cross = fs::metadata(&installation.state).unwrap().dev()
        != fs::metadata(destination.path()).unwrap().dev();
    assert_eq!(journal.adjacent.is_some(), cross);
    let temporary = journal.staging_directory(&installation).unwrap();
    if cross {
        // A crash in the first copy keeps the installed original authoritative.
        fs::remove_file(candidate.join("Contents/Info.plist")).unwrap();
        recover(&installation, &mut journal).unwrap();
        assert_eq!(installation.release().unwrap().sequence, 1);
        prepare_exchange(&installation, &mut journal).unwrap();
    }
    journal.phase = Phase::Swapping;
    journal.write(&installation).unwrap();
    recover(&installation, &mut journal).unwrap();
    assert_eq!(installation.release().unwrap().sequence, 1);
    swap(&installation.app, &candidate).unwrap();
    simulate_retention_interruption(&installation, &candidate, &mut journal, interruption);
    recover(&installation, &mut journal).unwrap();
    assert_eq!(journal.phase, Phase::Installed);
    assert!(journal.adjacent.is_none());
    assert_eq!(installation.release().unwrap(), to);
    package::verify(&installation.state.join("previous.app"), &journal.from).unwrap();
    if cross {
        assert!(!temporary.exists());
        assert!(!staged.exists());
    }
    recover(&installation, &mut journal).unwrap();
}

fn simulate_retention_interruption(
    installation: &Installation,
    source: &Path,
    journal: &mut Journal,
    interruption: u8,
) {
    let previous = installation.state.join("previous.app");
    match interruption {
        0 => (),
        1 => {
            let incoming = previous.with_extension("incoming");
            installation::private_directory(&incoming).unwrap();
            fs::write(incoming.join("partial"), b"partial copy").unwrap();
        }
        2 | 3 => {
            retain_previous(source, &previous, &journal.from).unwrap();
            if interruption == 3 {
                journal.phase = Phase::Installed;
                journal.write(installation).unwrap();
            }
        }
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "requires PLAYBIT_TEST_SIGN_IDENTITY"]
fn signed_profile_exchange_recovers_after_swap() {
    exchange_case(&std::env::temp_dir(), 0);
}

#[test]
#[ignore = "requires PLAYBIT_TEST_SIGN_IDENTITY and PLAYBIT_TEST_UPDATE_VOLUME"]
fn signed_cross_filesystem_exchange_recovers_each_copy_and_cleanup_boundary() {
    let volume = std::env::var_os("PLAYBIT_TEST_UPDATE_VOLUME").expect("test update volume");
    assert_ne!(
        fs::metadata(&volume).unwrap().dev(),
        fs::metadata(std::env::temp_dir()).unwrap().dev()
    );
    for interruption in 0..=3 {
        exchange_case(Path::new(&volume), interruption);
    }
}
