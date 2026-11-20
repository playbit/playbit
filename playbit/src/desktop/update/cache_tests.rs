#![allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
use super::*;
use std::{cell::Cell, os::unix::fs::PermissionsExt, time::SystemTime};

fn fixture() -> (tempfile::TempDir, Installation) {
    let root = tempfile::tempdir().unwrap();
    let installation =
        Installation { app: root.path().join("Playbit.app"), state: root.path().join("state") };
    installation::private_directory(&installation.state).unwrap();
    (root, installation)
}

fn release() -> Release {
    Release {
        app_version: "0.1.0-beta.10".into(),
        sequence: 10,
        url: format!("https://blob.playbit.app/{}", crate::identifier::encode(&[0; 32])),
        bundle_id: "playbit.engine".into(),
        filename: "Playbit-macos-arm64.zip".into(),
        sha256: "a".repeat(64),
        bytes: 42,
        platform: "macos-arm64".into(),
        revision: "b".repeat(40),
        checks_sha256: "c".repeat(64),
    }
}

#[test]
fn missing_and_unusable_cache_contents_fetch_a_fresh_result() {
    // An internal prelaunch build's 95-byte cache lacks the channel field. It is
    // disposable, just like truncated JSON and schema-incompatible contents.
    let predecessor = br#"{"finished":{"secs_since_epoch":1792394516,"nanos_since_epoch":370655000},"result":{"Ok":null}}"#;
    assert_eq!(predecessor.len(), 95);
    let mut invalid_release = release();
    invalid_release.sequence = 0;
    let invalid_release = serde_json::to_vec(&serde_json::json!({
        "channel": "testing",
        "finished": SystemTime::now(),
        "result": { "Ok": invalid_release },
    }))
    .unwrap();
    for contents in [
        None,
        Some(predecessor.to_vec()),
        Some(b"{".to_vec()),
        Some(b"{}".to_vec()),
        Some(vec![b' '; MAX_MANIFEST as usize + 1]),
        Some(invalid_release),
    ] {
        let (_root, installation) = fixture();
        if let Some(contents) = contents {
            installation.write("check.json", &contents).unwrap();
        }
        let requests = Cell::new(0);
        let result = coalesced_check_with(
            &installation,
            SystemTime::UNIX_EPOCH,
            ReleaseChannel::Testing,
            |channel| {
                assert_eq!(channel, ReleaseChannel::Testing);
                requests.set(requests.get() + 1);
                Ok(Some(release()))
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(requests.get(), 1);
        assert_eq!(result.identity(), release().identity());
        let cached = coalesced_check_with(
            &installation,
            SystemTime::UNIX_EPOCH,
            ReleaseChannel::Testing,
            |_| panic!("overlapping checks reuse the newly fetched result"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(cached.identity(), result.identity());
    }
}

#[test]
fn later_explicit_checks_and_other_channels_do_not_reuse_results() {
    let (_root, installation) = fixture();
    for (requested, channel) in [
        (SystemTime::UNIX_EPOCH, ReleaseChannel::Public),
        (SystemTime::now() + Duration::from_secs(60), ReleaseChannel::Public),
        (SystemTime::UNIX_EPOCH, ReleaseChannel::Testing),
    ] {
        let requests = Cell::new(0);
        assert!(
            coalesced_check_with(&installation, requested, channel, |selected| {
                assert_eq!(selected, channel);
                requests.set(requests.get() + 1);
                Ok(None)
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(requests.get(), 1);
    }
}

#[test]
fn a_future_cache_timestamp_does_not_pin_an_obsolete_result() {
    let (_root, installation) = fixture();
    installation
        .write(
            "check.json",
            &serde_json::to_vec(&serde_json::json!({
                "channel": "testing",
                "finished": SystemTime::now() + Duration::from_secs(3600),
                "result": { "Ok": null },
            }))
            .unwrap(),
        )
        .unwrap();
    let result =
        coalesced_check_with(&installation, SystemTime::now(), ReleaseChannel::Testing, |_| {
            Ok(Some(release()))
        })
        .unwrap()
        .unwrap();
    assert_eq!(result.identity(), release().identity());
}

#[test]
fn overlapping_failures_are_coalesced_but_later_checks_retry() {
    let (_root, installation) = fixture();
    let failure = coalesced_check_with(
        &installation,
        SystemTime::UNIX_EPOCH,
        ReleaseChannel::Testing,
        |_| Err("offline".into()),
    )
    .unwrap_err();
    assert_eq!(failure.to_string(), "offline");
    let failure = coalesced_check_with(
        &installation,
        SystemTime::UNIX_EPOCH,
        ReleaseChannel::Testing,
        |_| panic!("overlapping callers share the original failure"),
    )
    .unwrap_err();
    assert_eq!(failure.to_string(), "offline");
    assert!(
        coalesced_check_with(
            &installation,
            SystemTime::now() + Duration::from_secs(60),
            ReleaseChannel::Testing,
            |_| Ok(None),
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn unsafe_cache_objects_fail_without_fetching_or_replacing_them() {
    use std::os::unix::fs::symlink;
    for kind in ["symlink", "dangling", "hardlink", "shared", "directory", "fifo"] {
        let (root, installation) = fixture();
        let cache = installation.state.join("check.json");
        let target = root.path().join("target");
        installation.write("target", b"preserved").unwrap();
        fs::rename(installation.state.join("target"), &target).unwrap();
        match kind {
            "symlink" => symlink(&target, &cache).unwrap(),
            "dangling" => symlink(root.path().join("absent"), &cache).unwrap(),
            "hardlink" => fs::hard_link(&target, &cache).unwrap(),
            "shared" => {
                installation.write("check.json", b"{}").unwrap();
                fs::set_permissions(&cache, fs::Permissions::from_mode(0o644)).unwrap();
            }
            "directory" => fs::create_dir(&cache).unwrap(),
            "fifo" => {
                assert!(
                    Command::new("/usr/bin/mkfifo")
                        .args(["-m", "600"])
                        .arg(&cache)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            _ => unreachable!(),
        }
        assert!(
            coalesced_check_with(
                &installation,
                SystemTime::UNIX_EPOCH,
                ReleaseChannel::Testing,
                |_| panic!("unsafe cache objects must remain explicit errors"),
            )
            .is_err(),
            "{kind}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"preserved");
        assert!(fs::symlink_metadata(cache).is_ok());
    }
}
