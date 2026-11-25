//! Bundle admission, canonical encoding and read-only File behavior.
#![allow(clippy::unwrap_used, reason = "Tests assert fixture construction.")]
use super::*;
use std::io::{Cursor, Read};
fn example() -> Package {
    Package::new(
        "test".into(),
        "Example".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::from([("images/one".into(), vec![1, 2, 3]), ("empty".into(), vec![])]),
    )
    .unwrap()
}

fn with_metadata(manifest: &[u8], extras: &[(&str, &[u8])]) -> Vec<u8> {
    let package = example();
    let mut sources = BTreeMap::new();
    for entry in &package.index().entries {
        sources.insert(
            entry.path.clone(),
            codec::Source::Bytes(package.read(&entry.path, 0, entry.length as usize).unwrap()),
        );
    }
    sources.insert(codec::MANIFEST.into(), codec::Source::Bytes(manifest.to_vec()));
    for (path, bytes) in extras {
        sources.insert((*path).into(), codec::Source::Bytes(bytes.to_vec()));
    }
    let mut output = Vec::new();
    codec::write(&sources, &mut output).unwrap();
    output
}

#[test]
fn optional_metadata_survives_storage_display_and_copy_with_original_identity() {
    let bytes = with_metadata(
        br#"{"app_id":"test","future":{"theme":["dark",7]},"name":"Example"}"#,
        &[("app/extensions/example.notes/readme", b"future optional resource")],
    );
    let package = Package::from_bytes(&bytes).unwrap();
    assert_eq!(package.digest(), *blake3::hash(&bytes).as_bytes());
    assert_eq!(Package::read_display_name(&mut Cursor::new(&bytes)).unwrap(), "Example");
    assert_eq!(
        package.read("app/extensions/example.notes/readme", 0, 64).unwrap(),
        b"future optional resource"
    );
    let mut copy = Vec::new();
    package.write_to(&mut copy).unwrap();
    assert_eq!(copy, bytes);
    assert_eq!(Package::from_bytes(&copy).unwrap().digest(), package.digest());
    let authenticated =
        signature::testing::authenticate(&package, crate::store::Id::from_bytes([2; 16]), 100)
            .unwrap();
    authenticated.admit_execution(|_| false).unwrap();
}

#[test]
fn required_features_gate_execution_without_blocking_retention_or_display() {
    let bytes = with_metadata(
        br#"{"app_id":"test","name":"Example","required_host_features":["future.operation"]}"#,
        &[],
    );
    let package = Package::from_bytes(&bytes).unwrap();
    assert_eq!(package.required_host_features(), ["future.operation"]);
    assert!(
        package.admit_execution(|_| false).unwrap_err().to_string().contains("future.operation")
    );
    let authenticated =
        signature::testing::authenticate(&package, crate::store::Id::from_bytes([2; 16]), 100)
            .unwrap();
    authenticated.admit_execution(|feature| feature == "future.operation").unwrap();
    assert_eq!(package.to_bytes().unwrap(), bytes);
    assert_eq!(Package::read_display_name(&mut Cursor::new(&bytes)).unwrap(), "Example");
    for manifest in [
        br#"{"app_id":"test","name":"Example","required_host_features":["x","x"]}"#.as_slice(),
        br#"{"app_id":"test","name":"Example","required_host_features":["z","a"]}"#,
        br#"{"app_id":"test","name":"Example","required_host_features":[""]}"#,
        br#"{"app_id":"test","name":"Example","future":{"x":1,"x":2}}"#,
    ] {
        assert!(Package::from_bytes(&with_metadata(manifest, &[])).is_err());
    }
}

#[test]
fn optional_trace_annotations_preserve_executable_trace_ids_and_original_bytes() {
    let trace = b"{\"future\":{\"color\":\"blue\"},\"names\":[{\"description\":\"Start\",\
        \"future\":true,\"kind\":\"mark\",\"name\":\"app.start\"}],\"version\":1}";
    let bytes =
        with_metadata(br#"{"app_id":"test","name":"Example"}"#, &[(trace_catalog::FILE, trace)]);
    let package = Package::from_bytes(&bytes).unwrap();
    assert_eq!(package.trace().unwrap().as_ref(), trace);
    let projection = trace_catalog::TraceCatalog::decode(package.trace().unwrap()).unwrap();
    assert_eq!(projection.names[0].name, "app.start");
    assert!(projection.constants().unwrap().contains("APP_START: u16 = 0"));
    assert_eq!(package.to_bytes().unwrap(), bytes);
}
#[test]
fn canonical_roundtrip_and_identity_cover_index_metadata_and_payloads() {
    // A complete canonical file has one exact identity and re-encodes unchanged.
    let package = example();
    let bytes = package.to_bytes().unwrap();
    assert_eq!(Package::from_bytes(&bytes).unwrap().to_bytes().unwrap(), bytes);
    assert_eq!(package.id().unwrap(), crate::identifier::encode(blake3::hash(&bytes).as_bytes()));
    assert_eq!(Package::read_display_name(&mut Cursor::new(&bytes)).unwrap(), "Example");
    assert!(Package::read_display_icon(&mut Cursor::new(&bytes)).unwrap().is_none());
    assert!(
        Package::read_trace_catalog(&mut bytes.as_slice(), &package.digest()).unwrap().is_none()
    );
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() ^= 1;
    assert_ne!(Package::from_bytes(&changed).unwrap().id().unwrap(), package.id().unwrap());
    assert!(Package::read_trace_catalog(&mut changed.as_slice(), &package.digest()).is_err());
}
#[test]
fn malformed_truncated_and_noncanonical_bundles_fail() {
    // Every truncated prefix fails; index fields and alignment bytes are admitted.
    let bytes = example().to_bytes().unwrap();
    for end in 0..bytes.len() {
        assert!(Package::from_bytes(&bytes[..end]).is_err(), "{end}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Package::from_bytes(&trailing).is_err());
    for offset in [0, 8, 12, 16, 24, 32, 36, 40, 48] {
        let mut changed = bytes.clone();
        changed[offset] = 255;
        assert!(Package::from_bytes(&changed).is_err(), "{offset}");
    }
    let mut padding = bytes.clone();
    // Locate actual index padding after the first path.
    let name_len = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
    if !(32 + 24 + name_len).is_multiple_of(8) {
        padding[32 + 24 + name_len] = 1;
        assert!(Package::from_bytes(&padding).is_err());
    }
}
#[test]
fn paths_and_directory_conflicts_are_rejected() {
    // The writer and reader share directory-relative path admission.
    for path in ["", "a//b", "a/../b", "./a", "/a", "a\\b", "a\0b", "app/unknown"] {
        assert!(
            Package::new(
                "test".into(),
                "App".into(),
                b"\0asm\x01\0\0\0".to_vec(),
                BTreeMap::from([(path.into(), vec![])])
            )
            .is_err(),
            "{path:?}"
        );
    }
    assert!(
        Package::new(
            "test".into(),
            "App".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::from([("a".into(), vec![]), ("a/b".into(), vec![])])
        )
        .is_err()
    );
}
#[test]
fn payload_limits_reject_assets_code_and_aggregate_before_writing() {
    use std::io::{Result as IoResult, Write};
    struct Unwritten;
    impl Write for Unwritten {
        fn write(&mut self, _: &[u8]) -> IoResult<usize> {
            panic!("oversized package must fail before publication");
        }
        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sparse");
    let sparse = fs::File::create(&path).unwrap();
    let package = example();
    for (name, size) in [
        ("asset", codec::MAX_FILE_BYTES + 1),
        (codec::EXECUTABLE, codec::MAX_WASM_BYTES + 1),
        (codec::ICON, codec::MAX_ICON_BYTES + 1),
        ("aggregate", codec::MAX_FILE_BYTES),
    ] {
        sparse.set_len(size as u64).unwrap();
        let mut sources = BTreeMap::new();
        for entry in &package.index().entries {
            sources.insert(
                entry.path.clone(),
                codec::Source::Bytes(package.read(&entry.path, 0, entry.length as usize).unwrap()),
            );
        }
        sources.insert(name.into(), codec::Source::File(fs::File::open(&path).unwrap()));
        if name == "aggregate" {
            sources.insert("second".into(), codec::Source::File(fs::File::open(&path).unwrap()));
        }
        assert!(codec::write(&sources, &mut Unwritten).is_err(), "{name}");
    }
}
#[test]
#[cfg(any(
    target_arch = "wasm32",
    feature = "wasm-runtime",
    feature = "native-runtime",
    feature = "connected-engine",
    feature = "desktop-ui"
))]
fn file_objects_list_read_and_reject_writes() {
    // Bundle objects exercise the same File requests used by every host.
    use crate::runtime::host::files::{Action, Output};
    use playbit_api::abi::files::*;
    let root = filesystem::Object::root(example());
    let completion =
        root.execute(Action::Open { path: b"images/one".to_vec(), mode: FILE_READ }).unwrap();
    let reader = completion.created.unwrap();
    assert!(
        matches!(reader.execute(Action::Read { offset: 1, length: 9 }).unwrap().output, Output::Read { bytes } if bytes == [2, 3])
    );
    assert!(
        matches!(reader.execute(Action::Read { offset: 99, length: 1 }).unwrap().output, Output::Read { bytes } if bytes.is_empty())
    );
    assert_eq!(reader.status().unwrap().flags, FILE_SIZE_VALID | FILE_CONTENTS_IMMUTABLE);
    assert!(matches!(
        root.execute(Action::Open { path: b"new".to_vec(), mode: FILE_WRITE }),
        Err(playbit_api::abi::syscall::Error::AccessDenied)
    ));
    let list = root.execute(Action::ListOpen).unwrap().created.unwrap();
    assert!(
        matches!(list.execute(Action::ListNext { index: 0 }).unwrap().output, Output::Entry { name, kind: FILE_KIND_DIRECTORY } if name == b"app")
    );
}
#[test]
fn implicit_directories_sort_by_child_name_and_share_index_names() {
    // A directory named `a` sorts before `a-`, although `a/file` sorts after it.
    let package = Package::new(
        "test".into(),
        "Directories".into(),
        b"\0asm\x01\0\0\0".to_vec(),
        BTreeMap::from([
            ("a-".into(), vec![]),
            ("a/file".into(), vec![]),
            ("a/second".into(), vec![]),
        ]),
    )
    .unwrap();
    let index = package.index();
    let names: Vec<_> = index.children("").iter().map(|i| index.child("", *i).unwrap()).collect();
    assert_eq!(names, [("a", true), ("a-", false), ("app", true)]);
}
#[test]
fn packaging_sorts_sources_and_requires_the_initializer() {
    // Identical source contents produce identical bytes despite declaration order.
    let dir = tempfile::tempdir().unwrap();
    let source = "app_id='test'\nname='App'\nbuild_cmd='cargo build'\n";
    let manifest = Manifest::from_source(source).unwrap();
    assert!(manifest.bundle(dir.path(), b"\0asm\x01\0\0\0".to_vec()).is_err());
    fs::write(dir.path().join(VOLUME_INITIALIZERS_FILE), b"{}").unwrap();
    fs::write(dir.path().join("one"), b"one").unwrap();
    fs::write(dir.path().join("two"), b"two").unwrap();
    let first = Manifest::from_source(&format!("{source}[files]\nz='two'\na='one'\n")).unwrap();
    let second = Manifest::from_source(&format!("{source}[files]\na='one'\nz='two'\n")).unwrap();
    let wasm = b"\0asm\x01\0\0\0".to_vec();
    assert_eq!(
        first.bundle(dir.path(), wasm.clone()).unwrap().id().unwrap(),
        second.bundle(dir.path(), wasm.clone()).unwrap().id().unwrap()
    );
    let original = first.bundle(dir.path(), wasm.clone()).unwrap().digest();
    fs::File::open(dir.path().join("one"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    assert_eq!(first.bundle(dir.path(), wasm).unwrap().digest(), original);
    std::os::unix::fs::symlink("one", dir.path().join("link")).unwrap();
    let unsafe_manifest = Manifest::from_source(&format!("{source}[files]\na='link'\n")).unwrap();
    assert!(unsafe_manifest.bundle(dir.path(), b"\0asm\x01\0\0\0".to_vec()).is_err());
    fs::create_dir(dir.path().join("assets")).unwrap();
    fs::write(dir.path().join("assets/two"), b"two").unwrap();
    fs::write(dir.path().join("assets/one"), b"one").unwrap();
    let imported = Manifest::from_source(&format!("{source}files_dir='assets'\n")).unwrap();
    let declared =
        Manifest::from_source(&format!("{source}[files]\none='one'\ntwo='two'\n")).unwrap();
    let wasm = b"\0asm\x01\0\0\0".to_vec();
    assert_eq!(
        imported.bundle(dir.path(), wasm.clone()).unwrap().digest(),
        declared.bundle(dir.path(), wasm.clone()).unwrap().digest()
    );
    let collision =
        Manifest::from_source(&format!("{source}files_dir='assets'\n[files]\none='one'\n"))
            .unwrap();
    assert!(collision.bundle(dir.path(), wasm).is_err());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("data"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked-directory")).unwrap();
    assert!(source_file(dir.path(), "linked-directory/data").is_err());
    assert!(source_file(dir.path(), "../data").is_err());
    std::os::unix::fs::symlink(outside.path().join("data"), dir.path().join("assets/link"))
        .unwrap();
    assert!(imported.bundle(dir.path(), b"\0asm\x01\0\0\0".to_vec()).is_err());
    let sources = first.sources(dir.path(), b"\0asm\x01\0\0\0".to_vec()).unwrap();
    fs::rename(dir.path().join("one"), dir.path().join("moved")).unwrap();
    fs::write(dir.path().join("one"), b"replacement").unwrap();
    let mut bytes = Vec::new();
    codec::write(&sources, &mut bytes).unwrap();
    assert_eq!(Package::from_bytes(&bytes).unwrap().read("a", 0, 99).unwrap(), b"one");
}
#[test]
fn large_unused_payloads_stay_file_backed() {
    // Sparse files exercise maximum-size streaming without retaining resources.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("asset");
    let asset = fs::File::create(&path).unwrap();
    asset.set_len(codec::MAX_FILE_BYTES as u64).unwrap();
    drop(asset);
    let fixture = example();
    let mut sources = BTreeMap::new();
    for entry in &fixture.index().entries {
        sources.insert(
            entry.path.clone(),
            codec::Source::Bytes(fixture.read(&entry.path, 0, entry.length as usize).unwrap()),
        );
    }
    sources.insert("unused".into(), codec::Source::File(fs::File::open(path).unwrap()));
    let path = directory.path().join("large.pbapp");
    codec::write(&sources, &mut fs::File::create(&path).unwrap()).unwrap();
    let package = Package::open(&path).unwrap();
    assert_eq!(package.index().entry("unused").unwrap().length, codec::MAX_FILE_BYTES as u64);
    assert_eq!(package.read("unused", codec::MAX_FILE_BYTES as u64 - 1, 4).unwrap(), [0]);
    #[cfg(any(feature = "wasm-runtime", feature = "native-runtime", feature = "connected-engine"))]
    {
        use crate::runtime::host::files::{Action, Output};
        let root = filesystem::Object::root(package.clone());
        let reader = root
            .execute(Action::Open {
                path: b"unused".to_vec(),
                mode: playbit_api::abi::files::FILE_READ,
            })
            .unwrap()
            .created
            .unwrap();
        let output = reader
            .execute(Action::Read { offset: codec::MAX_FILE_BYTES as u64 - 1, length: 1 })
            .unwrap()
            .output;
        assert_eq!(output, Output::Read { bytes: vec![0] });
    }
    assert_eq!(package.read("images/one", 1, 1).unwrap(), [2]);
    struct Count<R> {
        reader: R,
        max: usize,
    }
    impl<R: Read> Read for Count<R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.max = self.max.max(bytes.len());
            self.reader.read(bytes)
        }
    }
    let mut input = Count { reader: fs::File::open(&path).unwrap(), max: 0 };
    codec::verify(&mut input).unwrap();
    assert!(input.max <= 128 * 1024);
    // Multiple legal extents reach the local package limit without a single
    // large allocation. Old readers continue using their retained descriptor.
    let source = directory.path().join("unused-2");
    let file = fs::File::create(&source).unwrap();
    file.set_len(255 * 1024 * 1024).unwrap();
    sources.insert("unused-2".into(), codec::Source::File(fs::File::open(source).unwrap()));
    let replacement = directory.path().join("replacement.pbapp");
    codec::write(&sources, &mut fs::File::create(&replacement).unwrap()).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let larger = Package::open(&path).unwrap();
    assert!(larger.index().length > 511 * 1024 * 1024);
    assert!(larger.index().length <= MAX_BUNDLE_BYTES as u64);
    assert!(package.index().entry("unused-2").is_none());
    assert_eq!(larger.read("unused-2", 255 * 1024 * 1024 - 1, 4).unwrap(), [0]);
    let mut input = Count { reader: fs::File::open(&path).unwrap(), max: 0 };
    codec::verify(&mut input).unwrap();
    assert!(input.max <= 128 * 1024);
}
