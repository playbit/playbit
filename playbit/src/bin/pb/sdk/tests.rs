#![allow(
    clippy::unwrap_used,
    reason = "Tests assert successful fixture construction and rejected inputs."
)]
use super::download::{
    Artifact, MAX_ARCHIVE, ORIGIN, address, catalog, catalog_with_key, check_sequence,
};
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use playbit::release_channel::ReleaseChannel;

#[test]
fn rust_requirements_validate_input_and_inherit_workspace_minimum() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("guest");
    fs::create_dir(&project).unwrap();
    fs::write(root.path().join("Cargo.toml"), "[workspace.package]\nrust-version='1.99'\n")
        .unwrap();
    fs::write(project.join("Cargo.toml"), "[package]\nrust-version.workspace=true\n").unwrap();
    assert_eq!(RustRequirements::read(&project).unwrap().minimum.unwrap().to_string(), "1.99.0");
    fs::write(project.join("Cargo.toml"), "[package]\nrust-version='invalid'\n").unwrap();
    assert!(RustRequirements::read(&project).is_err());
    fs::write(project.join("Cargo.toml"), "[package\n").unwrap();
    assert!(RustRequirements::read(&project).is_err());
}

#[test]
fn rust_toolchain_release_aliases_require_the_matching_series() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let compiler = root.path().join("rustc");
    fs::write(&compiler, "#!/bin/sh\ncase \"$1\" in --version) echo 'rustc 1.99.0 (fixture)' ;; *) echo '/sdk/fixture' ;; esac\n").unwrap();
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o700)).unwrap();
    let tools = Tools {
        language: Language::Rust,
        programs: BTreeMap::from([("rustc".into(), compiler)]),
        environment: BTreeMap::new(),
        identity: "fixture".into(),
        description: "fixture".into(),
        _lease: None,
    };
    for channel in ["1", "1.99", "1.99.0", "stable", "1.98", "nightly"] {
        let requested = RustRequirements { channel: Some(channel.into()), minimum: None };
        assert_eq!(
            requested.check(&tools, root.path()).is_ok(),
            !matches!(channel, "1.98" | "nightly"),
            "{channel}"
        );
    }
    for version in ["1.101.0-nightly", "1.101.0-beta.2", "1.101.0"] {
        fs::write(&tools.programs["rustc"], format!("#!/bin/sh\ncase \"$1\" in --version) echo 'rustc {version} (fixture)' ;; *) echo '/sdk/fixture' ;; esac\n")).unwrap();
        let channel = if version.contains("nightly") {
            "nightly"
        } else if version.contains("beta") {
            "beta"
        } else {
            "stable"
        };
        for minimum in ["1.100.0", "1.101.0", "1.101.1", "1.102.0"] {
            let requested = RustRequirements {
                channel: Some(channel.into()),
                minimum: Some(semver::Version::parse(minimum).unwrap()),
            };
            assert_eq!(
                requested.check(&tools, root.path()).is_ok(),
                matches!(minimum, "1.100.0" | "1.101.0"),
                "{version} requires {minimum}"
            );
        }
    }
}

#[test]
fn policy_and_archive_origins_are_explicit() {
    for (text, policy) in
        [("auto", Policy::Auto), ("builtin", Policy::Builtin), ("system", Policy::System)]
    {
        assert_eq!(Policy::parse(text).unwrap(), policy);
    }
    assert!(Policy::parse("rustup").is_err());
    let artifact = Artifact {
        language: Language::Rust,
        version: "1.99.0".into(),
        driver: 1,
        platform: "macos-arm64".into(),
        url: format!("{ORIGIN}{}", playbit::identifier::encode(&[1; 32])),
        bytes: 1,
        expanded_bytes: 1,
    };
    assert!(address(&artifact).is_ok());
    assert!(address(&Artifact { url: "https://example.com/file".into(), ..artifact }).is_err());
    assert!(catalog(br#"{"payload":"e30=","signature":""}"#, ReleaseChannel::Testing).is_err());
}

#[test]
fn signed_catalog_admits_only_the_channel_and_bounded_artifacts() {
    use ring::signature::KeyPair;
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
    let encode = |sequence, channel, size, driver| {
        let payload = serde_json::to_vec(&serde_json::json!({"channel":channel,"sequence":sequence,"artifacts":[{"language":"ocaml","version":"5.3.0","platform":"macos-arm64","driver":driver,"url":format!("{ORIGIN}{}",playbit::identifier::encode(&[1;32])),"bytes":size,"expanded_bytes":9}]})).unwrap();
        serde_json::to_vec(&serde_json::json!({"payload":STANDARD.encode(&payload),"signature":STANDARD.encode(pair.sign(&payload).as_ref())})).unwrap()
    };
    let valid = encode(1, "testing", 3, 1);
    let current =
        catalog_with_key(&valid, ReleaseChannel::Testing, pair.public_key().as_ref()).unwrap();
    assert!(download::artifact(&current, Language::Ocaml).is_ok());
    assert!(download::artifact(&current, Language::Rust).is_err());
    let incompatible = catalog_with_key(
        &encode(1, "testing", 3, 2),
        ReleaseChannel::Testing,
        pair.public_key().as_ref(),
    )
    .unwrap();
    assert!(download::artifact(&incompatible, Language::Ocaml).is_err());
    let next = catalog_with_key(
        &encode(2, "testing", 3, 1),
        ReleaseChannel::Testing,
        pair.public_key().as_ref(),
    )
    .unwrap();
    let changed = catalog_with_key(
        &encode(1, "testing", 4, 1),
        ReleaseChannel::Testing,
        pair.public_key().as_ref(),
    )
    .unwrap();
    assert!(check_sequence(&current, &current).is_ok());
    assert!(check_sequence(&current, &next).is_ok());
    assert!(check_sequence(&next, &current).is_err());
    assert!(check_sequence(&current, &changed).is_err());
    assert!(catalog_with_key(&valid, ReleaseChannel::Testing, pair.public_key().as_ref()).is_ok());
    assert!(catalog_with_key(&valid, ReleaseChannel::Testing, &[1; 32]).is_err());
    for invalid in [
        encode(0, "testing", 3, 1),
        encode(1, "public", 3, 1),
        encode(1, "testing", MAX_ARCHIVE + 1, 1),
    ] {
        assert!(
            catalog_with_key(&invalid, ReleaseChannel::Testing, pair.public_key().as_ref())
                .is_err()
        );
    }
}

#[test]
fn archive_rejects_links_traversal_collisions_and_size_changes() {
    let root = tempfile::tempdir().unwrap();
    let source = r#"
import tarfile, io, sys
from pathlib import Path
root = Path(sys.argv[1])
cases = {
    'valid': [('sdk/bin/compiler', tarfile.REGTYPE, 3, '')],
    'parent': [('../escape', tarfile.REGTYPE, 3, '')],
    'absolute': [('/tmp/escape', tarfile.REGTYPE, 3, '')],
    'traversal': [('sdk/../escape', tarfile.REGTYPE, 3, '')],
    'outside': [('other/value', tarfile.REGTYPE, 3, '')],
    'symlink': [('sdk/link', tarfile.SYMTYPE, 0, '/tmp')],
    'hardlink': [('sdk/link', tarfile.LNKTYPE, 0, 'sdk/value')],
    'fifo': [('sdk/pipe', tarfile.FIFOTYPE, 0, '')],
    'duplicate': [('sdk/value', tarfile.REGTYPE, 3, '')] * 2,
    'case': [('sdk/A/value', tarfile.REGTYPE, 3, ''), ('sdk/a/other', tarfile.REGTYPE, 3, '')],
    'collision': [('sdk/value', tarfile.REGTYPE, 3, ''), ('sdk/value/other', tarfile.REGTYPE, 3, '')],
}
for name, entries in cases.items():
    with tarfile.open(root / (name + '.tar.xz'), 'w:xz') as archive:
        for path, kind, size, link in entries:
            entry = tarfile.TarInfo(path)
            entry.type, entry.size, entry.linkname = kind, size, link
            archive.addfile(entry, io.BytesIO(b'abc'))
"#;
    assert!(
        Command::new("python3").args(["-c", source]).arg(root.path()).status().unwrap().success()
    );
    for name in [
        "valid",
        "parent",
        "absolute",
        "traversal",
        "outside",
        "symlink",
        "hardlink",
        "fifo",
        "duplicate",
        "case",
        "collision",
    ] {
        let result = super::archive::extract(
            &root.path().join(format!("{name}.tar.xz")),
            &root.path().join(name),
            if matches!(name, "duplicate" | "case" | "collision") { 6 } else { 3 },
        );
        assert_eq!(result.is_ok(), name == "valid", "{name}: {result:?}");
    }
    let archive = root.path().join("valid.tar.xz");
    // Both understated and overstated expanded lengths are rejected.
    for expanded in [2, 4] {
        assert!(
            super::archive::extract(
                &archive,
                &root.path().join(format!("size{expanded}")),
                expanded
            )
            .is_err()
        );
    }
    let mut bytes = fs::read(&archive).unwrap();
    bytes.truncate(bytes.len() / 2);
    fs::write(&archive, bytes).unwrap();
    assert!(super::archive::extract(&archive, &root.path().join("truncated"), 3).is_err());
}
