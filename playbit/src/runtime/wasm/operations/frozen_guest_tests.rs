//! Retained release-candidate bytes. Current SDK builds never update this test inventory.
#![allow(clippy::unwrap_used, reason = "Tests require intact immutable fixture metadata.")]
pub(super) const MEMORY: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/memory-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/memory-rust.wasm")),
];
pub(super) const RESULT: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/result-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/result-rust.wasm")),
];
pub(super) const SCHEMA: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/schema-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/schema-rust.wasm")),
];
pub(super) const QUERY: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/query-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/query-rust.wasm")),
];
pub(super) const PENDING: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/pending-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/pending-rust.wasm")),
];
pub(super) const AUTHENTICATED: [(&str, &[u8]); 2] = [
    ("c", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/authenticated-c.wasm")),
    ("rust", include_bytes!("../../../../tests/fixtures/guest-abi-rc2/authenticated-rust.wasm")),
];

#[test]
fn frozen_v1_rc2_bytes_and_provenance_are_immutable() {
    // Pin the retained manifest and every binary independently of today's SDK/compiler.
    let raw = include_bytes!("../../../../tests/fixtures/guest-abi-rc2/manifest.json");
    assert_eq!(
        blake3::hash(raw).to_hex().as_str(),
        "8f6419ffc23d97961d2a6841b1c11b8dfafa625f5157a178532dbeb3aa124743"
    );
    let manifest: serde_json::Value = serde_json::from_slice(raw).unwrap();
    assert_eq!(manifest["status"], "release_candidate");
    assert_eq!(manifest["abi"], "playbit.syscall/1");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 13);
    for (family, modules) in [
        ("memory", MEMORY),
        ("result", RESULT),
        ("schema", SCHEMA),
        ("query", QUERY),
        ("pending", PENDING),
        ("authenticated", AUTHENTICATED),
    ] {
        for (language, bytes) in modules {
            let name = format!("{family}-{language}.wasm");
            let entry = artifacts.iter().find(|entry| entry["file"] == name).unwrap();
            assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap(), "{name}");
            assert_eq!(blake3::hash(bytes).to_hex().as_str(), entry["blake3"], "{name}");
        }
    }
    let events = include_bytes!("../../../../tests/fixtures/guest-abi-rc2/events-rust.wasm");
    let entry = artifacts.iter().find(|entry| entry["file"] == "events-rust.wasm").unwrap();
    assert_eq!(blake3::hash(events).to_hex().as_str(), entry["blake3"]);
    let source = include_bytes!("../../../../tests/fixtures/guest-abi-rc2/sources.tar.gz");
    assert!(source.len() <= 200 * 1024);
    assert_eq!(blake3::hash(source).to_hex().as_str(), manifest["source_snapshot"]["blake3"]);
}
