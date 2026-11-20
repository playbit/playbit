#![no_main]

use std::{collections::BTreeMap, io::Cursor};

use libfuzzer_sys::fuzz_target;
use playbit::package::{Manifest, Package, trace_catalog::TraceCatalog};

fn decode_bundle(data: &[u8]) {
    let _ = Package::read_display_name(&mut Cursor::new(data));
    let _ = Package::read_display_icon(&mut Cursor::new(data));
    let schematic =
        Package::read_trace_catalog(&mut Cursor::new(data), blake3::hash(data).as_bytes());
    let decoded = Package::from_bytes(data);
    assert_eq!(schematic.is_ok(), decoded.is_ok(), "streaming and full canonical admission agree");
    if let Ok(package) = decoded {
        assert_eq!(
            schematic.expect("valid package schematic").as_deref(),
            package.trace().map(|bytes| bytes.as_ref())
        );
        assert_eq!(package.to_bytes().expect("decoded bundle encodes"), data);
        assert_eq!(
            package.id().expect("decoded bundle has identity"),
            playbit::identifier::encode(blake3::hash(data).as_bytes())
        );
        assert_eq!(
            Package::read_display_name(&mut Cursor::new(data)).expect("display name"),
            package.name().to_owned()
        );
        let expected = package.index().entry(playbit::package::codec::ICON).map(|entry| {
            package.read(&entry.path, 0, entry.length as usize).expect("validated icon")
        });
        assert_eq!(
            Package::read_display_icon(&mut Cursor::new(data)).expect("display icon"),
            expected
        );
    }
}

fn construct_bundle(data: &[u8]) {
    let Some((&name_len, data)) = data.split_first() else { return };
    let (name, payload) = data.split_at((name_len as usize).min(data.len()));
    let Ok(name) = std::str::from_utf8(name) else { return };
    let resources = BTreeMap::from([(playbit::package::codec::ICON.into(), payload.to_vec())]);
    if let Ok(package) =
        Package::new("test".into(), name.into(), b"\0asm\x01\0\0\0".to_vec(), resources)
    {
        let bytes = package.to_bytes().expect("bounded constructed package encodes");
        decode_bundle(&bytes);
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, data)) = data.split_first() else { return };
    if data.len() > 1024 * 1024 {
        return;
    }
    match mode % 5 {
        0 => decode_bundle(data),
        1 => {
            if let Ok(source) = std::str::from_utf8(data) {
                let _ = Manifest::from_source(source);
            }
        }
        2 => construct_bundle(data),
        4 => {
            let manifest = Manifest::from_source(include_str!("../../../guests/todo/playbit.toml"))
                .expect("fixture manifest");
            let _ = playbit::package::metadata::check_wasm(&manifest, data);
            let package = Package::new(
                "test".into(),
                manifest.name.clone(),
                b"\0asm\x01\0\0\0".to_vec(),
                BTreeMap::new(),
            )
            .expect("bounded fixture package");
            let _ = playbit::package::metadata::check_native(&manifest, &package, data);
        }
        _ => {
            if let Ok(schematic) = TraceCatalog::decode(data) {
                let source = schematic.constants().expect("validated declarations generate");
                assert!(source.len() <= 20_000);
                let encoded = serde_json::to_vec(&schematic).expect("bounded JSON");
                assert_eq!(TraceCatalog::decode(&encoded).expect("round trip"), schematic);
            }
        }
    }
});
